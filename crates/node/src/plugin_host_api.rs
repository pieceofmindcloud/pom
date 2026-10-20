//! The loopback API a running plugin calls back into its own node with.
//!
//! The node reaches a plugin through its `query`; until now nothing went the
//! other way. A plugin that takes part in the network needs a little of the
//! node: who this machine is, which machines are alive, a call to its own
//! plugin on another machine, and a signature by the node key over one kind
//! of statement. `host.configure` hands each plugin host its own secret:
//!
//! ```json
//! {"host_api": {"base_url": "http://127.0.0.1:8080/api/plugin-host", "token": "<hex>"}}
//! ```
//!
//! Every route takes it in `x-pom-plugin-host-token`. The token names the
//! plugin, so a plugin only ever speaks as itself: a peer call reaches the
//! same `plugin_code` on the other machine, through the signed peer route
//! (`crate::plugin_services`), never another plugin.
//!
//! - `GET /api/plugin-host/node`: this node's `node_id`, name, Ed25519 public key
//!   (hex and `did:key`), its role in the trust chain and the principal's
//!   `node_id`.
//! - `GET /api/plugin-host/peers`: the live peers, with whether each is the
//!   principal or a visitor.
//! - `POST /api/plugin-host/peer`: `{node_id, op, params}` runs `peer.<op>` of
//!   this plugin on that machine and answers `{reply}`.
//! - `POST /api/plugin-host/sign`: `{statement}` is signed with the node key
//!   only when it is an external identity statement in which this node's
//!   `did:key` vouches for a key (see [`signable_statement`]). The node key
//!   also signs announces, control envelopes, grants and the pkarr record, so
//!   it never signs bytes it did not recognize.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::time::Duration;

use crate::gateway::GatewayState;

/// Header that carries the plugin's own secret from `host.configure`.
pub(crate) const HOST_API_TOKEN_HEADER: &str = "x-pom-plugin-host-token";
/// Path the routes hang under; `host.configure` hands it over as `base_url`.
pub(crate) const HOST_API_PREFIX: &str = "/api/plugin-host";
/// A peer call waits this long for the other machine's plugin.
const PEER_CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The longest statement the node signs.
const MAX_STATEMENT: usize = 1024;
/// The external identity statements the node signs, by their fixed opening.
/// `{did}` is replaced by this node's `did:key`: the statement can only say
/// that this node vouches for something, never that another identity does.
/// Ghostly's WISP 300 binding statement, with WISP 311's `did` provider.
const IDENTITY_STATEMENTS: &[&str] =
    &["Ghostly identity proof v1: I control did:{did} and authorize the Ghostly key "];

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// The plugin the request speaks for, from its token.
fn caller_code(state: &GatewayState, headers: &HeaderMap) -> Result<String, Response> {
    let token = headers
        .get(HOST_API_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    state
        .plugins
        .code_for_host_api_token(token)
        .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "unknown plugin host token"))
}

/// `did:key` of an Ed25519 public key: multibase base58btc of the multicodec
/// prefix `0xed 0x01` and the key.
pub(crate) fn ed25519_did_key(public_key: &[u8; 32]) -> String {
    let mut bytes = Vec::with_capacity(34);
    bytes.extend_from_slice(&[0xed, 0x01]);
    bytes.extend_from_slice(public_key);
    format!("did:key:z{}", base58btc(&bytes))
}

fn base58btc(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let zeros = bytes.iter().take_while(|byte| **byte == 0).count();
    // Little-endian digits in base 58.
    let mut digits: Vec<u8> = Vec::with_capacity(bytes.len() * 2);
    for byte in &bytes[zeros..] {
        let mut carry = u32::from(*byte);
        for digit in &mut digits {
            carry += u32::from(*digit) << 8;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut encoded = String::with_capacity(zeros + digits.len());
    encoded.extend(std::iter::repeat('1').take(zeros));
    encoded.extend(
        digits
            .iter()
            .rev()
            .map(|digit| ALPHABET[*digit as usize] as char),
    );
    encoded
}

/// Whether the node signs `statement`: one line of printable ASCII, short, and
/// opening with one of [`IDENTITY_STATEMENTS`] for this node's own `did:key`.
/// None of the node's own signed messages is printable text that opens like
/// this, so a signature made here cannot stand for one of them.
pub(crate) fn signable_statement(statement: &str, did_key: &str) -> bool {
    statement.len() <= MAX_STATEMENT
        && statement.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        && IDENTITY_STATEMENTS.iter().any(|opening| {
            let opening = opening.replace("{did}", did_key);
            statement.len() > opening.len() && statement.starts_with(&opening)
        })
}

fn node_public_key(state: &GatewayState) -> [u8; 32] {
    state.authority_signing_key.verifying_key().to_bytes()
}

/// `GET /api/plugin-host/node`.
pub(crate) async fn node(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let public_key = node_public_key(&state);
    let document = state.admin_trust.document();
    Json(json!({
        "node_id": state.network.node_id,
        "hostname": state.network.hostname(),
        "public_key": hex::encode(public_key),
        "did_key": ed25519_did_key(&public_key),
        "role": crate::plugin_services::local_role(&state).await,
        "principal_node_id": document.primary.node_id,
    }))
    .into_response()
}

/// `GET /api/plugin-host/peers`.
pub(crate) async fn peers(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let principal = state.admin_trust.document().primary.node_id;
    let peers: Vec<Value> = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .filter(|peer| peer.node_id != state.network.node_id)
        .map(|peer| {
            json!({
                "node_id": peer.node_id,
                "hostname": peer.hostname,
                "principal": peer.node_id == principal,
                "visitor": crate::gateway::peer_is_visitor(&state, &peer),
            })
        })
        .collect();
    Json(json!({ "peers": peers })).into_response()
}

/// `POST /api/plugin-host/peer`.
pub(crate) async fn peer_call(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let code = match caller_code(&state, &headers) {
        Ok(code) => code,
        Err(response) => return response,
    };
    let (Some(node_id), Some(op)) = (body["node_id"].as_str(), body["op"].as_str()) else {
        return refuse(StatusCode::BAD_REQUEST, "node_id and op are required");
    };
    if !crate::plugin_services::valid_peer_op(op) {
        return refuse(StatusCode::BAD_REQUEST, "invalid operation");
    }
    let params = body.get("params").cloned().unwrap_or(Value::Null);
    if serde_json::to_vec(&params).map_or(true, |bytes| {
        bytes.len() > crate::plugin_services::MAX_PEER_BODY
    }) {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "params too large");
    }
    let reply = if node_id == state.network.node_id {
        let caller = json!({
            "node_id": state.network.node_id,
            "head": false,
            "local": true,
            "role": crate::plugin_services::local_role(&state).await,
        });
        state
            .plugins
            .query_audit(
                &code,
                &format!("peer.{op}"),
                json!({ "params": params, "caller": caller }),
            )
            .await
    } else {
        let Some(peer) = state
            .network
            .alive_peers()
            .await
            .into_iter()
            .find(|peer| peer.node_id == node_id)
        else {
            return refuse(StatusCode::NOT_FOUND, "the node is not a live peer");
        };
        let path = format!("/api/internal/plugins/{code}/peer/{op}");
        match state
            .control_post_json_routed(&peer, &path, &params, PEER_CALL_TIMEOUT)
            .await
        {
            Ok(response) if response.status().is_success() => response.json().await.ok(),
            _ => None,
        }
    };
    match reply {
        Some(reply) => Json(json!({ "reply": reply })).into_response(),
        None => refuse(
            StatusCode::BAD_GATEWAY,
            "plugin unavailable or the operation was refused",
        ),
    }
}

/// `POST /api/plugin-host/sign`.
pub(crate) async fn sign(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    use ed25519_dalek::Signer;
    let code = match caller_code(&state, &headers) {
        Ok(code) => code,
        Err(response) => return response,
    };
    let public_key = node_public_key(&state);
    let did_key = ed25519_did_key(&public_key);
    let Some(statement) = body["statement"].as_str() else {
        return refuse(StatusCode::BAD_REQUEST, "statement is required");
    };
    if !signable_statement(statement, &did_key) {
        return refuse(
            StatusCode::FORBIDDEN,
            "the node signs only an identity statement for its own did:key",
        );
    }
    let signature = state.authority_signing_key.sign(statement.as_bytes());
    tracing::info!(plugin_code = %code, "node key signed an identity statement for a plugin");
    Json(json!({
        "signature": hex::encode(signature.to_bytes()),
        "public_key": hex::encode(public_key),
        "did_key": did_key,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A node whose plugin manager knows `ghostly`, and that plugin's token.
    async fn state_with_plugin() -> (GatewayState, String) {
        let mut state = crate::gateway::testkit::state_with("node", vec![]).await;
        let service = Arc::new(crate::plugins::tests_support::MockLicenseService::new());
        state.plugins = crate::plugins::PluginManager::test_manager(&["ghostly"], service);
        let token = state.plugins.host_api_token_for_test("ghostly");
        (state, token)
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HOST_API_TOKEN_HEADER, token.parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn every_route_needs_the_plugin_token() {
        let (state, token) = state_with_plugin().await;
        assert_eq!(
            state.plugins.code_for_host_api_token(&token).as_deref(),
            Some("ghostly")
        );
        assert!(state.plugins.code_for_host_api_token("").is_none());
        for headers in [HeaderMap::new(), headers("not-the-token")] {
            let response = node(State(state.clone()), headers.clone()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = sign(
                State(state.clone()),
                headers,
                Json(json!({"statement": "x"})),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = node(State(state.clone()), headers(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_node_signs_only_its_own_identity_statement() {
        use ed25519_dalek::Verifier;
        let (state, token) = state_with_plugin().await;
        let public_key = node_public_key(&state);
        let did = ed25519_did_key(&public_key);
        let statement = format!(
            "Ghostly identity proof v1: I control did:{did} and authorize the Ghostly key k \
             to present it to contacts I choose. Nonce: n"
        );
        let refused = sign(
            State(state.clone()),
            headers(&token),
            Json(json!({"statement": statement.replace(&did, "did:key:z6Mkother")})),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let response = sign(
            State(state.clone()),
            headers(&token),
            Json(json!({"statement": statement})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let signature: [u8; 64] = hex::decode(body["signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        ed25519_dalek::VerifyingKey::from_bytes(&public_key)
            .unwrap()
            .verify(
                statement.as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&signature),
            )
            .expect("the node key signed the statement");
        assert_eq!(body["did_key"], did);
    }

    #[test]
    fn did_key_matches_the_w3c_ed25519_vector() {
        // did:key test vector of the W3C CCG did:key spec (Ed25519 2020).
        let public_key: [u8; 32] =
            hex::decode("2e6fcce36701dc791488e0d0b1745cc1e33a4c1c9fcc41c63bd343dbbe0970e6")
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            ed25519_did_key(&public_key),
            "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK"
        );
    }

    #[test]
    fn base58_keeps_leading_zeros() {
        assert_eq!(base58btc(&[0, 0, 1]), "112");
        assert_eq!(base58btc(&[]), "");
    }

    #[test]
    fn only_an_identity_statement_for_this_node_is_signable() {
        let did = "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK";
        let statement = format!(
            "Ghostly identity proof v1: I control did:{did} and authorize the Ghostly key \
             ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u to present it to contacts I \
             choose, from 2026-10-19T00:00:00Z until 2027-10-19T00:00:00Z. Nonce: AAAAAAAAAAAAAAAAAAAAAA"
        );
        assert!(signable_statement(&statement, did));
        // Another identity, another provider, nothing after the opening.
        let other = statement.replace("z6Mkha", "z6Mkhb");
        assert!(!signable_statement(&other, did));
        assert!(!signable_statement(
            &statement.replace("did:did:", "ssh:did:"),
            did
        ));
        let opening = IDENTITY_STATEMENTS[0].replace("{did}", did);
        assert!(!signable_statement(&opening, did));
        // One line of printable ASCII, and short.
        assert!(!signable_statement(&format!("{statement}\n"), did));
        assert!(!signable_statement(&format!("{statement}é"), did));
        assert!(!signable_statement(
            &format!("{statement}{}", "x".repeat(MAX_STATEMENT)),
            did
        ));
        // Bytes the node signs for itself never qualify.
        assert!(!signable_statement("{\"node_id\":\"abc\"}", did));
    }
}
