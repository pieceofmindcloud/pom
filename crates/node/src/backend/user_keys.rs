//! A signed-in person's own API keys (`/api/ui/my-keys`), outside demo mode.
//!
//! The keys live in the same `api_keys` table as every other key on this
//! node, with `user_id` set: a request with one carries that person's id
//! (`Caller::user_id`), so their quota and logs follow it. A person lists and
//! revokes only their own keys. Reached only through
//! `crate::user_session::after_gate`, which already refused demo mode.

use axum::{
    extract::Request,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::gateway::GatewayState;
use crate::user_session::UserIdentity;

/// Keys one person may hold at once.
const MAX_KEYS_PER_USER: usize = 20;

#[derive(Deserialize)]
struct NewKey {
    #[serde(default)]
    label: String,
}

fn listed(key: &crate::config::ApiKey) -> serde_json::Value {
    json!({
        "id": key.id,
        "label": key.label,
        "key_hint": key.key_hint,
        "active": key.active,
        "created_at": key.created_at,
    })
}

pub(crate) async fn handle(
    state: &GatewayState,
    user: &UserIdentity,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let own = |state: &GatewayState| -> Vec<crate::config::ApiKey> {
        state
            .database
            .api_keys()
            .unwrap_or_default()
            .into_iter()
            .filter(|key| key.user_id == user.user_id)
            .collect()
    };
    if path == "/api/ui/my-keys" && method == Method::GET {
        let keys = own(state).iter().map(listed).collect::<Vec<_>>();
        return Json(json!({ "keys": keys })).into_response();
    }
    if path == "/api/ui/my-keys" && method == Method::POST {
        if own(state).iter().filter(|key| key.active).count() >= MAX_KEYS_PER_USER {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "key limit reached", "max": MAX_KEYS_PER_USER })),
            )
                .into_response();
        }
        let body = axum::body::to_bytes(request.into_body(), 4096)
            .await
            .unwrap_or_default();
        let wanted = serde_json::from_slice::<NewKey>(&body)
            .map(|new| new.label)
            .unwrap_or_default();
        let wanted = wanted.trim().chars().take(60).collect::<String>();
        let label = if wanted.is_empty() {
            user.label.clone()
        } else {
            format!("{} - {wanted}", user.label)
        };
        return match crate::admin::create_key_for(
            state,
            label,
            state.network.node_id.clone(),
            user.user_id.clone(),
        )
        .await
        {
            Ok(issued) => (
                StatusCode::CREATED,
                Json(json!({ "key": issued.secret, "entry": listed(&issued.key) })),
            )
                .into_response(),
            Err(error) => {
                tracing::warn!("could not mint a user key: {error:#}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "could not mint the key" })),
                )
                    .into_response()
            }
        };
    }
    if method == Method::DELETE {
        let Some(id) = path.strip_prefix("/api/ui/my-keys/") else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !own(state).iter().any(|key| key.id == id) {
            return StatusCode::NOT_FOUND.into_response();
        }
        let revoked_by = format!("user:{}", user.user_id);
        return match state.database.revoke_api_key(id, &revoked_by) {
            Ok(_) => {
                let _ = crate::admin::refresh_api_key_snapshot(state).await;
                StatusCode::NO_CONTENT.into_response()
            }
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
    }
    StatusCode::NOT_FOUND.into_response()
}
