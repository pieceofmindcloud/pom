//! The Chat's image functions through the API: the same tools the Chat offers
//! its model (`generate_image` for the built-in workflow, `wf_<slug>` per
//! published ComfyUI workflow), so any OpenAI-compatible client with a key
//! this node minted runs the standard tool-call loop against POM, plus the
//! OpenAI images API on the built-in workflow.
//!
//! - `GET /v1/pom/tools` lists the definitions (`?for=chat` describes an image
//!   parameter as `use_attached_image`, the Chat's attachment; otherwise it is
//!   `<key>_url`, a data URL).
//! - `POST /v1/pom/tools/call {name, arguments}` runs one call to its end and
//!   answers `{content, media}`; `content` is what the client sends back as
//!   the `tool` message.
//! - `GET /v1/pom/media/:file` serves what a run stored (`<hash>.<ext>`).
//! - `POST /v1/images/generations` (OpenAI shape) runs the built-in workflow.
//!
//! The gateway never runs a tool inside `/v1/chat/completions`: the client
//! keeps the loop, as with any OpenAI tool.

use serde_json::{json, Map, Value};

use crate::chat_image::{RunRequest, Workflow};
use crate::gateway::GatewayState;

pub(crate) const GENERATE_IMAGE: &str = "generate_image";
const TOOL_PREFIX: &str = "wf_";
/// At most this many images per OpenAI images request.
const MAX_N: u32 = 4;
const ASPECTS: [(&str, u32, u32); 4] = [
    ("square", 1024, 1024),
    ("landscape", 1216, 832),
    ("portrait", 832, 1216),
    ("wide", 1344, 768),
];

/// The tool name of a published workflow: `wf_` and its slug (`pub:<slug>`)
/// with every character outside `[a-z0-9_]` as `_`. A built-in workflow has
/// none: `generate_image` stands for it.
pub(crate) fn tool_name(workflow: &Workflow) -> Option<String> {
    let slug = workflow.id.strip_prefix("pub:")?;
    let clean: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(format!("{TOOL_PREFIX}{clean}").chars().take(64).collect())
}

/// The built-in workflow a call without a choice runs: this node's when it
/// lists one (the list puts it first), else the first.
fn builtin_of(workflows: &[Workflow]) -> Option<&Workflow> {
    workflows.iter().find(|w| w.id.starts_with("builtin:"))
}

fn generate_image_tool(chat: bool) -> Value {
    let mut properties = json!({
        "prompt": {"type": "string", "description": "Detailed visual description, in English."},
        "negative_prompt": {"type": "string", "description": "What the image must not show."},
        "aspect": {"type": "string", "enum": ["square", "landscape", "portrait", "wide"]},
    });
    if chat {
        properties["use_attached_image"] =
            json!({"type": "boolean", "description": "Start from the image the user attached."});
    } else {
        properties["image_url"] = json!({"type": "string",
            "description": "A base image to start from, as a PNG or JPEG data URL."});
    }
    json!({"type": "function", "function": {
        "name": GENERATE_IMAGE,
        "description": "Create an image for the user. Call it when the user asks for a picture, drawing, photo, illustration, or an edit of an attached image. Write `prompt` as a detailed visual description in English that a diffusion model can follow (subject, setting, style, lighting, composition), using the conversation for context.",
        "parameters": {"type": "object", "properties": properties, "required": ["prompt"]},
    }})
}

/// The JSON schema property of one parameter, and whether a call must give it.
fn property_of(param: &crate::plugin_services::WorkflowParam, chat: bool) -> (String, Value, bool) {
    let mut description = param.label.clone();
    if let Some(more) = &param.description {
        description = format!("{description}: {more}");
    }
    if param.kind == "image" {
        return if chat {
            (
                "use_attached_image".to_owned(),
                json!({"type": "boolean", "description": format!("{description} (the image the user attached)")}),
                false,
            )
        } else {
            (
                format!("{}_url", param.key),
                json!({"type": "string", "description": format!("{description}, as a PNG or JPEG data URL")}),
                false,
            )
        };
    }
    let mut schema = match param.kind.as_str() {
        "number" => json!({"type": "number"}),
        "integer" => json!({"type": "integer"}),
        "boolean" => json!({"type": "boolean"}),
        "choice" => json!({"enum": param.options.clone().unwrap_or_default()}),
        _ => json!({"type": "string"}),
    };
    schema["description"] = Value::String(description);
    if let Some(min) = param.min {
        schema["minimum"] = json!(min);
    }
    if let Some(max) = param.max {
        schema["maximum"] = json!(max);
    }
    if let Some(default) = &param.default {
        schema["default"] = default.clone();
    }
    let required = param.default.is_none() && param.kind != "boolean";
    (param.key.clone(), schema, required)
}

/// The tools of the workflows the network offers now: `generate_image` while
/// a built-in one serves, then one per published workflow with parameters.
pub(crate) fn tool_definitions(workflows: &[Workflow], chat: bool) -> Vec<Value> {
    let mut tools = Vec::new();
    if builtin_of(workflows).is_some() {
        tools.push(generate_image_tool(chat));
    }
    let mut names = vec![GENERATE_IMAGE.to_owned()];
    for workflow in workflows {
        let Some(name) = tool_name(workflow) else {
            continue;
        };
        if workflow.params.is_empty() || names.contains(&name) {
            continue;
        }
        names.push(name.clone());
        let mut properties = Map::new();
        let mut required = Vec::new();
        for param in &workflow.params {
            let (key, schema, needed) = property_of(param, chat);
            if properties.contains_key(&key) {
                continue;
            }
            if needed {
                required.push(Value::String(key.clone()));
            }
            properties.insert(key, schema);
        }
        let description = workflow
            .description
            .clone()
            .unwrap_or_else(|| format!("Run the ComfyUI workflow \"{}\".", workflow.name));
        tools.push(json!({"type": "function", "function": {
            "name": name,
            "description": description,
            "parameters": {"type": "object", "properties": properties, "required": required},
        }}));
    }
    tools
}

/// The arguments of a call: an object, or a JSON string of one (how a model's
/// `tool_calls[].function.arguments` arrives).
fn arguments_of(value: &Value) -> Result<Map<String, Value>, String> {
    let parsed = match value {
        Value::String(text) if text.trim().is_empty() => Value::Object(Map::new()),
        Value::String(text) => {
            serde_json::from_str(text).map_err(|_| "the arguments are not JSON".to_owned())?
        }
        Value::Null => Value::Object(Map::new()),
        other => other.clone(),
    };
    match parsed {
        Value::Object(map) => Ok(map),
        _ => Err("the arguments must be an object".into()),
    }
}

fn text_arg(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// The run a tool call asks for. `attached` is the image the caller attached
/// (the Chat), used when the call says `use_attached_image`.
pub(crate) fn call_to_request(
    workflows: &[Workflow],
    name: &str,
    arguments: &Value,
    attached: Option<&str>,
) -> Result<RunRequest, String> {
    let args = arguments_of(arguments)?;
    let use_attached = args.get("use_attached_image") == Some(&Value::Bool(true));
    if name == GENERATE_IMAGE {
        let workflow = builtin_of(workflows).ok_or("no image model serves on the network")?;
        let prompt = text_arg(&args, "prompt").ok_or("generate_image needs a prompt")?;
        let (width, height) = text_arg(&args, "aspect")
            .and_then(|a| ASPECTS.iter().find(|(name, _, _)| *name == a))
            .map(|(_, w, h)| (Some(*w), Some(*h)))
            .unwrap_or((None, None));
        let image = text_arg(&args, "image_url")
            .or_else(|| use_attached.then(|| attached.map(str::to_owned)).flatten());
        return Ok(RunRequest {
            workflow_id: workflow.id.clone(),
            node_id: workflow.node_id.clone(),
            prompt,
            params: None,
            negative: text_arg(&args, "negative_prompt"),
            width,
            height,
            seed: None,
            count: None,
            image,
            strength: None,
        });
    }
    let workflow = workflows
        .iter()
        .find(|w| !w.params.is_empty() && tool_name(w).as_deref() == Some(name))
        .ok_or_else(|| format!("no workflow is called {name}"))?;
    let mut params = Map::new();
    for param in &workflow.params {
        if param.kind == "image" {
            let given = text_arg(&args, &format!("{}_url", param.key))
                .or_else(|| use_attached.then(|| attached.map(str::to_owned)).flatten());
            if let Some(data) = given {
                if !data.starts_with("data:image/") {
                    return Err(format!("{} must be a PNG or JPEG data URL", param.label));
                }
                params.insert(param.key.clone(), Value::String(data));
            }
        } else if let Some(value) = args.get(&param.key).filter(|v| !v.is_null()) {
            params.insert(param.key.clone(), value.clone());
        }
    }
    Ok(RunRequest {
        workflow_id: workflow.id.clone(),
        node_id: workflow.node_id.clone(),
        prompt: params
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        params: Some(params),
        negative: None,
        width: None,
        height: None,
        seed: None,
        count: None,
        image: None,
        strength: None,
    })
}

/// The run an OpenAI images request asks for: `prompt`, `n` (1 to 4),
/// `size` (`WxH`, or `auto`) and `model` (a workflow's id or name; the
/// built-in one when absent or unknown).
pub(crate) fn images_request(workflows: &[Workflow], body: &Value) -> Result<RunRequest, String> {
    let prompt = body["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or("prompt is required")?;
    let chosen = body["model"].as_str().and_then(|model| {
        workflows
            .iter()
            .find(|w| w.id.starts_with("builtin:") && (w.id == model || w.name == model))
    });
    let workflow = chosen
        .or_else(|| builtin_of(workflows))
        .ok_or("no image model serves on the network")?;
    let n = body["n"].as_u64().unwrap_or(1);
    if !(1..=u64::from(MAX_N)).contains(&n) {
        return Err(format!("n must be between 1 and {MAX_N}"));
    }
    let (width, height) = match body["size"].as_str() {
        None | Some("auto") => (None, None),
        Some(size) => {
            let (w, h) = size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                .filter(|(w, h)| (64..=4096).contains(w) && (64..=4096).contains(h))
                .ok_or("size must be WIDTHxHEIGHT, 64 to 4096 each, or auto")?;
            (Some(w), Some(h))
        }
    };
    Ok(RunRequest {
        workflow_id: workflow.id.clone(),
        node_id: workflow.node_id.clone(),
        prompt: prompt.to_owned(),
        params: None,
        negative: None,
        width,
        height,
        seed: None,
        count: Some(n as u32),
        image: None,
        strength: None,
    })
}

/// What a finished run answers through the API: each stored output with its
/// URL on this node, and the text for the `tool` message.
pub(crate) fn call_result(workflow_name: &str, stored: &Value) -> Value {
    let list = stored["media"]
        .as_array()
        .cloned()
        .or_else(|| {
            stored["images"].as_array().map(|images| {
                images
                    .iter()
                    .map(|i| {
                        let mut i = i.clone();
                        i["kind"] = json!("image");
                        i
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    let media: Vec<Value> = list
        .iter()
        .filter_map(|m| {
            let hash = m["hash"].as_str()?;
            let ext = m["ext"].as_str()?;
            let mut out = json!({
                "url": format!("/v1/pom/media/{hash}.{ext}"),
                "kind": m["kind"], "hash": hash, "ext": ext,
            });
            for size in ["width", "height"] {
                if let Some(v) = m[size].as_u64().filter(|v| *v > 0) {
                    out[size] = json!(v);
                }
            }
            Some(out)
        })
        .collect();
    let mut counts: Vec<String> = Vec::new();
    for kind in ["image", "video", "audio"] {
        let n = media.iter().filter(|m| m["kind"] == kind).count();
        if n > 0 {
            counts.push(format!("{n} {kind}{}", if n > 1 { "s" } else { "" }));
        }
    }
    let urls: Vec<&str> = media.iter().filter_map(|m| m["url"].as_str()).collect();
    let content = format!(
        "\"{workflow_name}\" made {} and showed it to the user: {}",
        counts.join(", "),
        urls.join(" ")
    );
    json!({ "content": content, "media": media })
}

// ── Routes ──────────────────────────────────────────────────────────────────

/// Runs one workflow to its end: the stored outputs (`{media, images}`) or
/// the reason it failed.
async fn run_to_end(state: &GatewayState, request: RunRequest) -> Result<Value, String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let state = state.clone();
    tokio::spawn(async move {
        crate::chat_image::run_into(&state, request, tx).await;
    });
    while let Some(line) = rx.recv().await {
        if let Some(error) = line["error"].as_str() {
            return Err(error.to_owned());
        }
        if line.get("media").is_some() || line.get("images").is_some() {
            return Ok(line);
        }
    }
    Err("the workflow ended without an output".into())
}

fn refuse(status: axum::http::StatusCode, message: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        status,
        axum::Json(json!({"error": {"message": message, "type": "invalid_request_error"}})),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct ToolsQuery {
    #[serde(default, rename = "for")]
    audience: Option<String>,
}

/// `GET /v1/pom/tools`.
async fn tools_route(
    _auth: crate::auth::ApiKeyAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::extract::Query(query): axum::extract::Query<ToolsQuery>,
) -> axum::Json<Value> {
    let workflows = crate::chat_image::network_workflows(&state).await;
    let chat = query.audience.as_deref() == Some("chat");
    axum::Json(json!({ "tools": tool_definitions(&workflows, chat) }))
}

/// `POST /v1/pom/tools/call`.
async fn call_route(
    _auth: crate::auth::ApiKeyAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let Some(name) = body["name"].as_str() else {
        return refuse(StatusCode::BAD_REQUEST, "name is required");
    };
    let workflows = crate::chat_image::network_workflows(&state).await;
    let request = match call_to_request(&workflows, name, &body["arguments"], None) {
        Ok(request) => request,
        Err(error) => return refuse(StatusCode::BAD_REQUEST, &error),
    };
    let workflow_name = workflows
        .iter()
        .find(|w| w.id == request.workflow_id)
        .map(|w| w.name.clone())
        .unwrap_or_default();
    match run_to_end(&state, request).await {
        Ok(stored) => axum::Json(call_result(&workflow_name, &stored)).into_response(),
        Err(error) => refuse(StatusCode::BAD_GATEWAY, &error),
    }
}

/// `POST /v1/images/generations`.
async fn images_route(
    _auth: crate::auth::ApiKeyAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use base64::Engine;
    let workflows = crate::chat_image::network_workflows(&state).await;
    let request = match images_request(&workflows, &body) {
        Ok(request) => request,
        Err(error) => return refuse(StatusCode::BAD_REQUEST, &error),
    };
    let as_url = body["response_format"].as_str() == Some("url");
    let stored = match run_to_end(&state, request).await {
        Ok(stored) => stored,
        Err(error) => return refuse(StatusCode::BAD_GATEWAY, &error),
    };
    let root = state.auth.read().await.workspace_root_path();
    let dir = crate::chat_image::images_dir(&root);
    let mut data = Vec::new();
    for image in stored["images"].as_array().into_iter().flatten() {
        let (Some(hash), Some(ext)) = (image["hash"].as_str(), image["ext"].as_str()) else {
            continue;
        };
        if as_url {
            data.push(json!({"url": format!("/v1/pom/media/{hash}.{ext}")}));
        } else {
            match tokio::fs::read(dir.join(format!("{hash}.{ext}"))).await {
                Ok(bytes) => data.push(json!({
                    "b64_json": base64::engine::general_purpose::STANDARD.encode(bytes)
                })),
                Err(_) => continue,
            }
        }
    }
    axum::Json(json!({"created": chrono::Utc::now().timestamp(), "data": data})).into_response()
}

/// `GET /v1/pom/media/:file` (`<hash>.<ext>` or the bare hash).
async fn media_route(
    auth: crate::auth::ApiKeyAuth,
    state: axum::extract::State<GatewayState>,
    axum::extract::Path(file): axum::extract::Path<String>,
) -> axum::response::Response {
    let hash = file
        .split_once('.')
        .map_or(file.as_str(), |(h, _)| h)
        .to_owned();
    crate::chat_image::image_route(auth, state, axum::extract::Path(hash)).await
}

pub(crate) fn routes() -> axum::Router<GatewayState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/pom/tools", get(tools_route))
        .route(
            "/v1/pom/tools/call",
            post(call_route).layer(axum::extract::DefaultBodyLimit::max(
                crate::chat_image::MAX_RUN_BODY,
            )),
        )
        .route("/v1/pom/media/:file", get(media_route))
        .route("/v1/images/generations", post(images_route))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_services::WorkflowParam;

    fn param(key: &str, kind: &str) -> WorkflowParam {
        WorkflowParam {
            key: key.into(),
            label: key.into(),
            description: None,
            kind: kind.into(),
            default: None,
            min: None,
            max: None,
            options: None,
        }
    }

    fn workflows() -> Vec<Workflow> {
        let mut steps = param("steps", "integer");
        steps.min = Some(1.0);
        steps.max = Some(50.0);
        steps.default = Some(json!(20));
        let mut sampler = param("sampler", "choice");
        sampler.options = Some(vec![json!("euler"), json!("dpmpp_2m")]);
        vec![
            Workflow {
                id: "builtin:img-1".into(),
                name: "Z-Image".into(),
                node_id: "n1".into(),
                deploy_id: Some("img-1".into()),
                inputs: vec!["prompt".into()],
                description: None,
                params: vec![],
            },
            Workflow {
                id: "pub:upscale-4x".into(),
                name: "Upscale 4x".into(),
                node_id: "n2".into(),
                deploy_id: None,
                inputs: vec![],
                description: Some("Makes a photo four times larger".into()),
                params: vec![
                    param("prompt", "text"),
                    steps,
                    sampler,
                    param("sharpen", "boolean"),
                    param("image", "image"),
                ],
            },
            Workflow {
                id: "pub:legacy".into(),
                name: "Legacy".into(),
                node_id: "n2".into(),
                deploy_id: None,
                inputs: vec!["prompt".into()],
                description: None,
                params: vec![],
            },
        ]
    }

    #[test]
    fn definitions_come_from_params() {
        let tools = tool_definitions(&workflows(), false);
        let names: Vec<_> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["generate_image", "wf_upscale_4x"],
            "a workflow without params has no tool"
        );
        let wf = &tools[1]["function"];
        assert_eq!(wf["description"], "Makes a photo four times larger");
        let props = &wf["parameters"]["properties"];
        assert_eq!(props["steps"]["type"], "integer");
        assert_eq!(props["steps"]["maximum"], 50.0);
        assert_eq!(props["steps"]["default"], 20);
        assert_eq!(props["sampler"]["enum"], json!(["euler", "dpmpp_2m"]));
        assert_eq!(props["sharpen"]["type"], "boolean");
        assert_eq!(props["image_url"]["type"], "string");
        assert_eq!(wf["parameters"]["required"], json!(["prompt", "sampler"]));
        assert!(tools[0]["function"]["parameters"]["properties"]["image_url"].is_object());
        let chat = tool_definitions(&workflows(), true);
        assert!(chat[1]["function"]["parameters"]["properties"]["use_attached_image"].is_object());
        assert!(chat[0]["function"]["parameters"]["properties"]["use_attached_image"].is_object());
    }

    #[test]
    fn no_builtin_no_generate_image() {
        let only_published: Vec<_> = workflows().into_iter().skip(1).collect();
        let tools = tool_definitions(&only_published, false);
        assert_eq!(tools.len(), 1);
        assert_ne!(tools[0]["function"]["name"], GENERATE_IMAGE);
    }

    #[test]
    fn a_call_becomes_a_run() {
        let list = workflows();
        let run = call_to_request(
            &list,
            "wf_upscale_4x",
            &json!(r#"{"prompt": "sharper", "steps": 99, "sampler": "euler", "image_url": "data:image/png;base64,AAAA", "other": 1}"#),
            None,
        )
        .unwrap();
        assert_eq!(
            (run.workflow_id.as_str(), run.node_id.as_str()),
            ("pub:upscale-4x", "n2")
        );
        let params = run.params.unwrap();
        assert_eq!(params["steps"], 99, "the plugin clamps");
        assert_eq!(params["image"], "data:image/png;base64,AAAA");
        assert!(params.get("other").is_none());
        let run = call_to_request(
            &list,
            GENERATE_IMAGE,
            &json!({"prompt": "a boat", "aspect": "portrait", "use_attached_image": true}),
            Some("data:image/jpeg;base64,BBBB"),
        )
        .unwrap();
        assert_eq!((run.width, run.height), (Some(832), Some(1216)));
        assert_eq!(run.image.as_deref(), Some("data:image/jpeg;base64,BBBB"));
        assert_eq!(run.workflow_id, "builtin:img-1");
    }

    #[test]
    fn bad_calls_are_refused() {
        let list = workflows();
        assert!(call_to_request(&list, "wf_nope", &json!({}), None).is_err());
        assert!(call_to_request(&list, "wf_legacy", &json!({}), None).is_err());
        assert!(call_to_request(&list, "wf_upscale_4x", &json!("[1,2]"), None).is_err());
        assert!(call_to_request(&list, "wf_upscale_4x", &json!("not json"), None).is_err());
        assert!(call_to_request(&list, GENERATE_IMAGE, &json!({}), None).is_err());
        assert!(call_to_request(
            &list,
            "wf_upscale_4x",
            &json!({"image_url": "http://example.com/a.png"}),
            None
        )
        .is_err());
    }

    #[test]
    fn the_images_api_maps_size_and_n() {
        let list = workflows();
        let run = images_request(
            &list,
            &json!({"prompt": "a fox", "n": 2, "size": "512x768"}),
        )
        .unwrap();
        assert_eq!(
            (run.width, run.height, run.count),
            (Some(512), Some(768), Some(2))
        );
        assert_eq!(run.workflow_id, "builtin:img-1");
        let auto = images_request(&list, &json!({"prompt": "a fox", "size": "auto"})).unwrap();
        assert_eq!((auto.width, auto.count), (None, Some(1)));
        assert!(images_request(&list, &json!({"prompt": "a fox", "n": 9})).is_err());
        assert!(images_request(&list, &json!({"prompt": "a fox", "size": "big"})).is_err());
        assert!(images_request(&list, &json!({"n": 1})).is_err());
        assert!(images_request(&list[1..], &json!({"prompt": "x"})).is_err());
    }

    #[test]
    fn a_result_names_its_urls() {
        let stored = json!({"media": [
            {"hash": "ab", "ext": "png", "kind": "image", "width": 8, "height": 8},
            {"hash": "cd", "ext": "mp4", "kind": "video"}
        ]});
        let result = call_result("Clip", &stored);
        assert_eq!(result["media"][0]["url"], "/v1/pom/media/ab.png");
        assert_eq!(result["media"][1]["kind"], "video");
        assert!(result["media"][1].get("width").is_none());
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("1 image, 1 video"), "{content}");
        let older = call_result(
            "Old",
            &json!({"images": [{"hash": "ef", "ext": "jpg", "width": 1, "height": 1}]}),
        );
        assert_eq!(older["media"][0]["kind"], "image");
    }
}
