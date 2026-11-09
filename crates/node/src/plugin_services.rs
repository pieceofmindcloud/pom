//! Services a plugin offers to the cluster, and the authenticated path a peer
//! uses to reach a plugin on another machine.
//!
//! A plugin is local to its node, but what it runs can matter to everyone: a
//! vector database that is the retrieval source of the pipeline, for example.
//! Two generic pieces make that visible and reachable without the node knowing
//! any plugin:
//!
//! - `host.services`: a running plugin may describe what it serves. The node
//!   validates the answer and publishes it in `NodeInfo.plugin_services`, which
//!   is how the dashboard draws the service next to its machine.
//! - `POST /api/internal/plugins/:code/peer/:op`: a peer calls the plugin's
//!   `peer.<op>` operation. The call must carry the signed control envelope, so
//!   the plugin receives **who** is asking - the caller's `node_id` and whether
//!   it serves layer 0 of a model - and decides what to hand over. This is how
//!   a plugin shares a secret with the heads of the network and nobody else.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_SERVICES_PER_PLUGIN: usize = 8;
const MAX_TEXT: usize = 200;
pub(crate) const MAX_PEER_BODY: usize = 256 * 1024;
/// `/api/ui/node` is polled by every dashboard; the plugins are asked at most
/// this often.
const SERVICES_TTL: Duration = Duration::from_secs(5);

/// One service of a plugin, as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PluginService {
    pub plugin_code: String,
    /// Stable within the plugin: `rag`.
    pub id: String,
    /// What the service is for, from a short open vocabulary: `rag`.
    pub kind: String,
    /// Product name shown to the operator: `Qdrant`.
    pub name: String,
    /// `ready`, `starting`, `installing`, `waiting` or `error`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// For a service of kind `inference`: the GGUF architectures its engine
    /// runs (`general.architecture`), which the deploy screen uses to offer
    /// only the models that engine can serve.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub architectures: Vec<String>,
    /// For a service of kind `inference`: the settings its engine takes,
    /// which the deploy inspector draws and `engine.load` receives back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<EngineSetting>,
    /// For a service of kind `inference`: its engine also runs POM layer
    /// slices, one stage per machine, linked through the nodes
    /// (`plugin_engine`, the stage link).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stages: bool,
    /// For a service with `stages`: the architectures whose models it runs as
    /// layer slices, when not all of them. A model of another architecture of
    /// `architectures` runs on that engine as the whole model only. Empty: every
    /// architecture it runs also runs in slices.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stage_architectures: Vec<String>,
    /// For a service with `stages`: the architectures whose stage it can also
    /// cut from the whole model at run time (`range` in `engine.load`), so the
    /// machine keeps the whole file and writes no slice. Empty: none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stage_ranges: Vec<String>,
    /// Kind `inference`: the model architectures whose drafter file its engine takes from the deployment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub draft_architectures: Vec<String>,
    /// For a service of kind `inference`: its engine reads images through the
    /// deployment's vision projector (`mmproj_path`), so the deploy screen
    /// offers the projector for it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub vision: bool,
    /// For a service of kind `inference`: the POM deployment settings that
    /// apply to its engine (`POM_SETTINGS`), which the deploy inspector draws
    /// in their own card, apart from the engine's settings.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pom_settings: Vec<String>,
    /// For a service of kind `image`: the image model components it serves
    /// now (`components.rs`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ServedComponent>,
    /// For a service of kind `image`: the workflows the Chat may run on this
    /// machine (`chat_image.rs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<ImageWorkflow>,
}

/// A ComfyUI workflow the Chat can run: a deployment's built-in text to
/// image (`builtin:<deploy_id>`) or one an admin published (`pub:<slug>`).
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub(crate) struct ImageWorkflow {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_id: Option<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    /// What the workflow does (the Chat's model reads it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The values a published workflow takes; empty for a built-in one and
    /// for a plugin that lists none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<WorkflowParam>,
}

/// One value a published workflow takes. The graph's node and input stay in
/// the plugin.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub(crate) struct WorkflowParam {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `text`, `number`, `integer`, `boolean`, `choice` or `image`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<Value>>,
}

const MAX_WORKFLOWS: usize = 32;
pub(crate) const MAX_PARAMS: usize = 24;
const PARAM_TYPES: [&str; 6] = ["text", "number", "integer", "boolean", "choice", "image"];
const MAX_PARAM_OPTIONS: usize = 200;

/// A scalar a parameter may hold: a short string, a number or a boolean.
fn scalar(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(_) | Value::Number(_) => Some(value.clone()),
        Value::String(text) if text.len() <= 8000 => Some(value.clone()),
        _ => None,
    }
}

fn parse_param(entry: &Value) -> Option<WorkflowParam> {
    let key = entry["key"].as_str().filter(|key| {
        !key.is_empty()
            && key.len() <= 40
            && key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })?;
    let kind = entry["type"].as_str().filter(|k| PARAM_TYPES.contains(k))?;
    let options = match kind {
        "choice" => {
            let list: Vec<Value> = entry["options"]
                .as_array()?
                .iter()
                .filter_map(scalar)
                .take(MAX_PARAM_OPTIONS)
                .collect();
            if list.is_empty() {
                return None;
            }
            Some(list)
        }
        _ => None,
    };
    let bound = |name: &str| {
        matches!(kind, "number" | "integer")
            .then(|| entry[name].as_f64())
            .flatten()
    };
    Some(WorkflowParam {
        key: key.to_owned(),
        label: text(&entry["label"]).unwrap_or_else(|| key.to_owned()),
        description: short_text(&entry["description"], 300),
        kind: kind.to_owned(),
        default: (kind != "image")
            .then(|| scalar(&entry["default"]))
            .flatten(),
        min: bound("min"),
        max: bound("max"),
        options,
    })
}

/// A trimmed text of at most `limit` bytes without control characters.
fn short_text(value: &Value, limit: usize) -> Option<String> {
    let text = value.as_str()?.trim();
    (!text.is_empty() && text.len() <= limit && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

fn workflow_id(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    (!text.is_empty()
        && text.len() <= 96
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b':')
        }))
    .then(|| text.to_owned())
}

fn parse_workflows(kind: &str, value: &Value) -> Vec<ImageWorkflow> {
    if kind != "image" {
        return Vec::new();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_WORKFLOWS)
        .filter_map(|entry| {
            Some(ImageWorkflow {
                id: workflow_id(&entry["id"])?,
                name: text(&entry["name"])?,
                deploy_id: slug(&entry["deploy_id"]),
                inputs: entry["inputs"]
                    .as_array()
                    .map(|list| list.iter().filter_map(slug).take(MAX_PARAMS).collect())
                    .unwrap_or_default(),
                description: short_text(&entry["description"], 500),
                params: entry["params"]
                    .as_array()
                    .map(|list| {
                        let mut seen = std::collections::HashSet::new();
                        list.iter()
                            .filter_map(parse_param)
                            .filter(|p| seen.insert(p.key.clone()))
                            .take(MAX_PARAMS)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// One image model component a plugin serves: a part of an image deployment.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub(crate) struct ServedComponent {
    pub deploy_id: String,
    pub name: String,
    /// `diffusion`, `text_encoder` or `vae`.
    pub component: String,
    /// File names (never paths).
    pub files: Vec<String>,
    /// `ready`, `loading` or `error`.
    pub status: String,
    /// Calls being served now, when the plugin counts them.
    #[serde(default)]
    pub in_flight: Option<u32>,
}

const MAX_COMPONENTS: usize = 64;

fn parse_components(kind: &str, value: &Value) -> Vec<ServedComponent> {
    if kind != "image" {
        return Vec::new();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_COMPONENTS)
        .filter_map(|entry| {
            Some(ServedComponent {
                deploy_id: slug(&entry["deploy_id"])?,
                name: text(&entry["name"])?,
                component: slug(&entry["component"])?,
                files: entry["files"]
                    .as_array()
                    .map(|files| files.iter().filter_map(text).take(8).collect())
                    .unwrap_or_default(),
                status: entry["status"]
                    .as_str()
                    .filter(|s| ["ready", "loading", "error"].contains(s))?
                    .to_owned(),
                in_flight: entry["in_flight"]
                    .as_u64()
                    .map(|n| n.min(u64::from(u16::MAX)) as u32),
            })
        })
        .collect()
}

/// The POM deployment settings an inference service may declare, by their
/// `RuntimeParams` name. The first two act on the hidden state the nodes carry
/// between the stages of a model split across machines; `inference_role` is a
/// whole-model machine's part in a prefill/decode split (`plugin_engine`), and
/// `prefill_min_tokens` the split's threshold.
const POM_SETTINGS: [&str; 4] = [
    "prefill_max_inflight_chunks",
    "hidden_state_dtype",
    "inference_role",
    "prefill_min_tokens",
];

fn parse_pom_settings(kind: &str, value: &Value) -> Vec<String> {
    let mut settings: Vec<String> = Vec::new();
    if kind != "inference" {
        return settings;
    }
    for name in value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if POM_SETTINGS.contains(&name) && !settings.iter().any(|known| known == name) {
            settings.push(name.to_owned());
        }
    }
    settings
}

const MAX_SETTINGS: usize = 32;
const MAX_OPTIONS: usize = 32;
const SETTING_KINDS: [&str; 5] = ["number", "integer", "boolean", "enum", "list"];

/// One setting an inference service's engine takes. The node only checks the
/// shape; the plugin validates the values when it loads the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EngineSetting {
    /// Lowercase words separated by dots or underscores: `sampling.top_k`.
    pub key: String,
    /// `number`, `integer`, `boolean`, `enum` or `list` (of names).
    #[serde(rename = "type")]
    pub kind: String,
    /// Per language; `en` is always present.
    pub label: std::collections::BTreeMap<String, String>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub description: std::collections::BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// `pom`: the deployment's POM settings card draws it, whatever the slices; absent: the engine's card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
}

fn setting_key(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    (!text.is_empty()
        && text.len() <= 64
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
        })
        && !text.starts_with('.')
        && !text.ends_with('.'))
    .then(|| text.to_owned())
}

/// Texts per language: `en` required for a label; languages are short tags.
fn localized(value: &Value, max_len: usize) -> std::collections::BTreeMap<String, String> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(lang, _)| {
                    lang.len() <= 8 && lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .filter_map(|(lang, text)| {
                    let text = text.as_str()?.trim();
                    (!text.is_empty()
                        && text.len() <= max_len
                        && !text.chars().any(char::is_control))
                    .then(|| (lang.clone(), text.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_setting(entry: &Value) -> Option<EngineSetting> {
    let key = setting_key(&entry["key"])?;
    let kind = entry["type"]
        .as_str()
        .filter(|kind| SETTING_KINDS.contains(kind))?
        .to_owned();
    let label = localized(&entry["label"], MAX_TEXT);
    if !label.contains_key("en") {
        return None;
    }
    let options: Vec<String> = if kind == "enum" {
        let options: Vec<String> = entry["options"]
            .as_array()?
            .iter()
            .filter_map(|option| text(option).filter(|option| option.len() <= 64))
            .take(MAX_OPTIONS)
            .collect();
        if options.is_empty() {
            return None;
        }
        options
    } else {
        Vec::new()
    };
    let bound = |field: &str| entry[field].as_f64().filter(|value| value.is_finite());
    Some(EngineSetting {
        key,
        kind,
        label,
        description: localized(&entry["description"], 600),
        min: bound("min"),
        max: bound("max"),
        options,
        placement: (entry["placement"].as_str() == Some("pom")).then(|| "pom".to_owned()),
    })
}

fn parse_settings(value: &Value) -> Vec<EngineSetting> {
    let mut settings: Vec<EngineSetting> = Vec::new();
    for setting in value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_setting)
    {
        if settings.len() == MAX_SETTINGS {
            break;
        }
        if !settings.iter().any(|known| known.key == setting.key) {
            settings.push(setting);
        }
    }
    settings
}

const MAX_ARCHITECTURES: usize = 16;

const STATUSES: [&str; 5] = ["ready", "starting", "installing", "waiting", "error"];

fn slug(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    (!text.is_empty()
        && text.len() <= 64
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        }))
    .then(|| text.to_owned())
}

fn text(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    (!text.is_empty() && text.len() <= MAX_TEXT && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

/// Reads a `host.services` answer: `{"services": [{id, kind, name, status,
/// version?, detail?}]}`. An entry that does not follow the contract is
/// dropped; the rest of the answer still counts.
pub(crate) fn parse_services(plugin_code: &str, answer: &Value) -> Vec<PluginService> {
    let Some(entries) = answer.get("services").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut services: Vec<PluginService> = Vec::new();
    for entry in entries.iter().take(MAX_SERVICES_PER_PLUGIN) {
        let (Some(id), Some(kind), Some(name)) = (
            slug(&entry["id"]),
            slug(&entry["kind"]),
            text(&entry["name"]),
        ) else {
            continue;
        };
        let Some(status) = entry["status"]
            .as_str()
            .filter(|status| STATUSES.contains(status))
        else {
            continue;
        };
        if services.iter().any(|service| service.id == id) {
            continue;
        }
        let stages = kind == "inference" && entry["stages"] == true;
        let vision = kind == "inference" && entry["vision"] == true;
        let pom_settings = parse_pom_settings(&kind, &entry["pom_settings"]);
        let components = parse_components(&kind, &entry["components"]);
        let workflows = parse_workflows(&kind, &entry["workflows"]);
        services.push(PluginService {
            plugin_code: plugin_code.to_owned(),
            id,
            kind,
            name,
            status: status.to_owned(),
            version: text(&entry["version"]),
            detail: text(&entry["detail"]),
            architectures: entry["architectures"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(slug)
                        .take(MAX_ARCHITECTURES)
                        .collect()
                })
                .unwrap_or_default(),
            settings: parse_settings(&entry["settings"]),
            draft_architectures: entry["draft_architectures"]
                .as_array()
                .map(|list| list.iter().filter_map(slug).take(MAX_ARCHITECTURES).collect())
                .unwrap_or_default(),
            stage_ranges: if stages {
                entry["stage_ranges"]
                    .as_array()
                    .map(|list| list.iter().filter_map(slug).take(MAX_ARCHITECTURES).collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            stage_architectures: if stages {
                entry["stage_architectures"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(slug)
                            .take(MAX_ARCHITECTURES)
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            stages,
            vision,
            pom_settings,
            components,
            workflows,
        });
    }
    services
}

fn cache() -> &'static Mutex<Option<(Instant, Vec<PluginService>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<PluginService>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The services of every running plugin of this node, for `NodeInfo`.
pub(crate) async fn node_services(plugins: &crate::plugins::PluginManager) -> Vec<PluginService> {
    if let Some((at, services)) = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        if at.elapsed() < SERVICES_TTL {
            return services.clone();
        }
    }
    let services: Vec<PluginService> = plugins
        .query_running("host.services")
        .await
        .iter()
        .flat_map(|(code, answer)| parse_services(code, answer))
        .collect();
    *cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Instant::now(), services.clone()));
    services
}

/// An operation name of the peer API: lowercase words separated by dots,
/// dashes or underscores.
pub(crate) fn valid_peer_op(op: &str) -> bool {
    !op.is_empty()
        && op.len() <= 64
        && op.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && !op.starts_with('.')
        && !op.ends_with('.')
}

/// What the plugin is told about the peer that is calling it.
pub(crate) fn caller_of(peer: &crate::network::PeerInfo, role: &str) -> Value {
    let visitor = role == "visitor";
    json!({
        "node_id": peer.node_id,
        // Layer 0 is where every prompt enters a model: its head. An
        // inference node serves no layer, whatever it announces.
        "head": !visitor && peer.total_layers > 0 && peer.start_layer == 0,
        "model": peer.model,
        "role": role,
    })
}

/// This machine's own role, as the trust chain proves it: what a plugin is
/// told when the caller is its own node.
pub(crate) async fn local_role(state: &crate::gateway::GatewayState) -> &'static str {
    let profile = state.auth.read().await.profile;
    match state
        .admin_trust
        .effective_authority(&state.authority_principal, profile)
    {
        Some(crate::admin_trust::EffectiveAuthority::Principal) => "principal",
        Some(_) => "admin",
        None if profile == crate::profile::NodeProfile::Visitor => "visitor",
        None => "member",
    }
}

/// The caller's role as the trust chain proves it, for a plugin that shares
/// something only with the admins: `principal`, `admin`, `visitor` or
/// `member`. The announced profile alone never makes a caller an admin.
pub(crate) fn caller_role(
    state: &crate::gateway::GatewayState,
    issuer: &crate::admin_trust::AuthorityPrincipal,
    peer: &crate::network::PeerInfo,
) -> &'static str {
    if crate::gateway::peer_is_visitor(state, peer) {
        "visitor"
    } else if crate::gateway::issuer_is_principal(state, issuer) {
        "principal"
    } else if crate::admin_trust::control_issuer_is_admin(&state.admin_trust.document(), issuer) {
        "admin"
    } else {
        "member"
    }
}

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

/// `POST /api/internal/plugins/:code/peer/:op`: a peer asks this node's plugin
/// for something. Only a signed call from a live peer of this network gets
/// through; the plugin sees the caller and answers or refuses.
pub(crate) async fn plugin_peer_call(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Path((code, op)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let Some(principal) = auth.principal() else {
        // A shared key proves membership, not identity: the plugin could not
        // tell a head from any other machine.
        return refuse(StatusCode::FORBIDDEN, "a signed peer call is required");
    };
    if !valid_peer_op(&op) {
        return refuse(StatusCode::BAD_REQUEST, "invalid operation");
    }
    if body.len() > MAX_PEER_BODY {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    }
    let params: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return refuse(StatusCode::BAD_REQUEST, "body is not JSON"),
        }
    };
    let Some(peer) = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == principal.node_id)
    else {
        return refuse(StatusCode::FORBIDDEN, "the caller is not a live peer");
    };
    // The metering ledger's node operations are answered by the node itself,
    // whether or not it runs the plugin (`crate::metering::node_op`).
    if crate::metering::is_node_op(&code, &op) {
        let role = caller_role(&state, principal, &peer);
        // A remote caller is never local, whatever its body says.
        let mut params = params;
        if let Some(object) = params.as_object_mut() {
            object.remove("local");
        }
        return match crate::metering::node_op(&state, role, &op, &params).await {
            Ok(reply) => axum::Json(reply).into_response(),
            Err(error) => refuse(StatusCode::FORBIDDEN, &error),
        };
    }
    // Told apart from a failure below: the caller skips a node that does not
    // run the plugin, but a node that runs it and did not answer may hold the
    // data it asked for (the quota lookup, `backend/quota.rs`).
    if !state.plugins.status(&code).await.is_some_and(|status| status.active) {
        return refuse(StatusCode::NOT_FOUND, "the plugin does not run on this node");
    }
    match state
        .plugins
        .query_audit(
            &code,
            &format!("peer.{op}"),
            json!({
                "params": params,
                "caller": caller_of(&peer, caller_role(&state, principal, &peer)),
            }),
        )
        .await
    {
        Some(reply) => axum::Json(reply).into_response(),
        None => refuse(
            StatusCode::BAD_GATEWAY,
            "plugin unavailable or the operation was refused",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_parsed_from_an_image_service() {
        let answer = json!({"services": [
            {"id": "comfyui-image", "kind": "image", "name": "ComfyUI", "status": "ready",
             "components": [
                {"deploy_id": "img-1", "name": "Z-Image", "component": "vae",
                 "files": ["ae.safetensors"], "status": "ready"},
                {"deploy_id": "img-1", "name": "Z-Image", "component": "diffusion",
                 "files": ["z.safetensors"], "status": "loading"},
                {"deploy_id": "", "component": "vae"}
             ]},
            {"id": "comfyui", "kind": "app", "name": "ComfyUI", "status": "ready"}
        ]});
        let services = parse_services("comfyui", &answer);
        assert_eq!(services[0].components.len(), 2);
        assert_eq!(services[0].components[0].component, "vae");
        assert_eq!(services[0].components[1].status, "loading");
        assert!(services[1].components.is_empty());
    }

    /// A plugin learns the caller's proven role, and an inference node is
    /// never a head even when it announces layer 0.
    #[test]
    fn the_caller_carries_its_role_and_an_inference_node_is_no_head() {
        let mut peer = crate::gateway::testkit::peer("n", "n", 8080);
        peer.start_layer = 0;
        peer.total_layers = 32;
        assert_eq!(caller_of(&peer, "member")["head"], true);
        assert_eq!(caller_of(&peer, "admin")["role"], "admin");
        let visitor = caller_of(&peer, "visitor");
        assert_eq!(visitor["head"], false);
        assert_eq!(visitor["role"], "visitor");
    }

    #[test]
    fn a_setting_placed_in_the_pom_card_keeps_its_placement() {
        let answer = json!({"services": [
            {"id": "e", "kind": "inference", "name": "Engine", "status": "ready", "settings": [
                {"key": "sampling.top_k", "type": "integer", "label": {"en": "Top k"}},
                {"key": "session_save", "type": "boolean", "placement": "pom", "label": {"en": "Save the KV cache"}},
                {"key": "other", "type": "boolean", "placement": "engine", "label": {"en": "Other"}}
            ]}
        ]});
        let services = parse_services("engine", &answer);
        let settings = &services[0].settings;
        assert_eq!(settings[0].placement, None);
        assert_eq!(settings[1].placement.as_deref(), Some("pom"));
        assert_eq!(settings[2].placement, None);
        let published = serde_json::to_value(&services[0]).unwrap();
        assert!(published["settings"][0].get("placement").is_none());
        assert_eq!(published["settings"][1]["placement"], "pom");
    }

    #[test]
    fn a_service_answer_is_validated_entry_by_entry() {
        let answer = json!({"services": [
            {"id": "rag", "kind": "rag", "name": "Qdrant", "status": "ready", "version": "v1.19.1", "detail": "2 collections"},
            {"id": "rag", "kind": "rag", "name": "Duplicate", "status": "ready"},
            {"id": "Bad Id", "kind": "rag", "name": "X", "status": "ready"},
            {"id": "other", "kind": "rag", "name": "X", "status": "sleeping"},
            {"id": "noname", "kind": "rag", "status": "ready"},
            {"id": "plain", "kind": "search", "name": "Plain", "status": "starting", "version": 7}
        ]});
        let services = parse_services("qdrant", &answer);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].plugin_code, "qdrant");
        assert_eq!(services[0].name, "Qdrant");
        assert_eq!(services[0].version.as_deref(), Some("v1.19.1"));
        assert_eq!(services[0].detail.as_deref(), Some("2 collections"));
        assert_eq!(services[1].id, "plain");
        assert_eq!(services[1].version, None);
        assert!(services[0].architectures.is_empty());
        assert!(serde_json::to_value(&services[1])
            .unwrap()
            .get("version")
            .is_none());
    }

    #[test]
    fn an_inference_service_names_the_architectures_it_runs() {
        let answer = json!({"services": [
            {"id": "strata", "kind": "inference", "name": "Strata", "status": "ready",
             "architectures": ["qwen4exp", "Bad Arch", 7]}
        ]});
        let services = parse_services("strata", &answer);
        assert_eq!(services[0].architectures, vec!["qwen4exp".to_string()]);
        assert!(!services[0].stages);
        let staged = parse_services(
            "strata",
            &json!({"services": [{"id": "s", "kind": "inference", "name": "S", "status": "ready", "stages": true},
                                 {"id": "r", "kind": "rag", "name": "R", "status": "ready", "stages": true}]}),
        );
        assert!(staged[0].stages && !staged[1].stages);
        assert_eq!(serde_json::to_value(&staged[0]).unwrap()["stages"], true);
        // vision, too, only for an inference service
        let seeing = parse_services(
            "strata",
            &json!({"services": [{"id": "s", "kind": "inference", "name": "S", "status": "ready", "vision": true},
                                 {"id": "r", "kind": "rag", "name": "R", "status": "ready", "vision": true}]}),
        );
        assert!(seeing[0].vision && !seeing[1].vision);
        assert_eq!(serde_json::to_value(&seeing[0]).unwrap()["vision"], true);

        // Only known POM settings, once each, and only for an inference service.
        let declared = parse_services(
            "p",
            &json!({"services": [
                {"id": "s", "kind": "inference", "name": "S", "status": "ready",
                 "pom_settings": ["hidden_state_dtype", "n_gpu_layers", "hidden_state_dtype", 7]},
                {"id": "r", "kind": "rag", "name": "R", "status": "ready",
                 "pom_settings": ["hidden_state_dtype"]}]}),
        );
        assert_eq!(declared[0].pom_settings, ["hidden_state_dtype"]);
        assert!(declared[1].pom_settings.is_empty());
        assert!(serde_json::to_value(&declared[1])
            .unwrap()
            .get("pom_settings")
            .is_none());
        assert_eq!(
            serde_json::to_value(&services[0]).unwrap()["architectures"],
            json!(["qwen4exp"])
        );
    }

    #[test]
    fn an_inference_service_publishes_its_engine_settings_validated() {
        let answer = json!({"services": [
            {"id": "e", "kind": "inference", "name": "Engine", "status": "ready", "settings": [
                {"key": "sampling.top_k", "type": "integer", "min": 1, "max": 64,
                 "label": {"en": "Top k", "pt-BR": "Top k"}, "description": {"en": "Default top_k"}},
                {"key": "mode", "type": "enum", "options": ["a", "b"], "label": {"en": "Mode"}},
                {"key": "mode", "type": "boolean", "label": {"en": "Duplicate"}},
                {"key": "Bad Key", "type": "boolean", "label": {"en": "x"}},
                {"key": "nolabel", "type": "boolean", "label": {"pt-BR": "x"}},
                {"key": "kind", "type": "date", "label": {"en": "x"}},
                {"key": "empty_enum", "type": "enum", "options": [], "label": {"en": "x"}}
            ]}
        ]});
        let services = parse_services("engine", &answer);
        let settings = &services[0].settings;
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[0].key, "sampling.top_k");
        assert_eq!(settings[0].max, Some(64.0));
        assert_eq!(settings[1].options, vec!["a".to_string(), "b".to_string()]);
        let published = serde_json::to_value(&services[0]).unwrap();
        assert_eq!(published["settings"][0]["type"], "integer");
        assert_eq!(published["settings"][0]["label"]["pt-BR"], "Top k");
    }

    #[test]
    fn answers_without_a_service_list_publish_nothing() {
        for answer in [
            json!({}),
            json!({"services": "rag"}),
            json!(null),
            json!([]),
        ] {
            assert!(parse_services("qdrant", &answer).is_empty());
        }
    }

    #[test]
    fn text_is_bounded_and_carries_no_control_characters() {
        let long = "x".repeat(MAX_TEXT + 1);
        let answer = json!({"services": [
            {"id": "a", "kind": "rag", "name": long, "status": "ready"},
            {"id": "b", "kind": "rag", "name": "ok", "status": "ready", "detail": "line\nbreak"}
        ]});
        let services = parse_services("qdrant", &answer);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].id, "b");
        assert_eq!(services[0].detail, None);
    }

    #[test]
    fn peer_operation_names_are_plain() {
        for good in ["connection", "search.v1", "a-b_c"] {
            assert!(valid_peer_op(good), "{good}");
        }
        for bad in ["", ".a", "a.", "A", "a/b", "a b", &"x".repeat(65)] {
            assert!(!valid_peer_op(bad), "{bad}");
        }
    }

    #[test]
    fn workflow_params_are_parsed_and_bounded() {
        let mut params: Vec<Value> = vec![
            json!({"key": "prompt", "label": "Prompt", "node": "6", "input": "text", "type": "text",
                   "description": "What to draw"}),
            json!({"key": "steps", "label": "Steps", "node": "3", "input": "steps", "type": "integer",
                   "min": 1, "max": 50, "default": 20}),
            json!({"key": "sampler", "label": "Sampler", "node": "3", "input": "sampler_name",
                   "type": "choice", "options": ["euler", "dpmpp_2m"], "default": "euler"}),
            json!({"key": "Bad Key", "label": "x", "type": "text"}),
            json!({"key": "odd", "label": "x", "type": "video"}),
            json!({"key": "pick", "label": "x", "type": "choice"}),
        ];
        params.extend((0..30).map(|i| json!({"key": format!("k{i}"), "label": "K", "type": "boolean"})));
        let answer = json!({"services": [
            {"id": "comfyui-image", "kind": "image", "name": "ComfyUI", "status": "ready",
             "workflows": [{"id": "pub:typed", "name": "Typed", "description": "Paints a picture",
                            "inputs": ["prompt", "steps"], "params": params}]}
        ]});
        let workflow = &parse_services("comfyui", &answer)[0].workflows[0];
        assert_eq!(workflow.description.as_deref(), Some("Paints a picture"));
        assert_eq!(workflow.params.len(), MAX_PARAMS, "at most 24 parameters");
        let keys: Vec<_> = workflow.params.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(&keys[..4], ["prompt", "steps", "sampler", "k0"], "bad entries are dropped");
        assert_eq!(workflow.params[0].description.as_deref(), Some("What to draw"));
        assert_eq!(workflow.params[1].kind, "integer");
        assert_eq!((workflow.params[1].min, workflow.params[1].max), (Some(1.0), Some(50.0)));
        assert_eq!(workflow.params[2].options.as_ref().unwrap().len(), 2);
        assert_eq!(workflow.params[2].default, Some(json!("euler")));
        let older = json!({"services": [
            {"id": "comfyui-image", "kind": "image", "name": "ComfyUI", "status": "ready",
             "workflows": [{"id": "pub:old", "name": "Old", "inputs": ["prompt"]}]}
        ]});
        let workflow = &parse_services("comfyui", &older)[0].workflows[0];
        assert!(workflow.params.is_empty() && workflow.description.is_none());
    }
}
