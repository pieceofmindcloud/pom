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
const MAX_PEER_BODY: usize = 256 * 1024;
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
            stages,
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
fn valid_peer_op(op: &str) -> bool {
    !op.is_empty()
        && op.len() <= 64
        && op.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && !op.starts_with('.')
        && !op.ends_with('.')
}

/// What the plugin is told about the peer that is calling it.
pub(crate) fn caller_of(peer: &crate::network::PeerInfo) -> Value {
    json!({
        "node_id": peer.node_id,
        // Layer 0 is where every prompt enters a model: its head.
        "head": peer.total_layers > 0 && peer.start_layer == 0,
        "model": peer.model,
    })
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
    match state
        .plugins
        .query_audit(
            &code,
            &format!("peer.{op}"),
            json!({ "params": params, "caller": caller_of(&peer) }),
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
}
