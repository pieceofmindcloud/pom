//! The stage link of a plugin engine split across machines.
//!
//! An inference service that publishes `stages: true` also runs POM layer
//! slices: each machine of the deployment loads its slice in the plugin's
//! engine, the earlier stage hands the later one the hidden state of every
//! token, and the last stage (the head, the sampling) serves the requests -
//! the gateway sends the prompt there ([`crate::plugin_engine::plugin_of_plan`]).
//!
//! The engines never listen on the network. The node carries their link:
//!
//! - **The later stage's machine** ([`prepare`]) finds the machine of the same
//!   deployment whose slice ends where its own starts, takes that stage's link
//!   secret from it (`GET /api/internal/plugins/:code/engine/link/:deploy`) and
//!   opens a relay on loopback. `engine.load` receives
//!   `link: {token, prev_port}`; the engine connects to `127.0.0.1:prev_port`.
//! - **The relay** reads the engine's frames - a little-endian `u32` length,
//!   then that many bytes, the same framing both ways - and posts each one,
//!   signed, to `POST /api/internal/plugins/:code/engine/link/:deploy/:conn`
//!   on the earlier stage's machine, along the route already measured to it.
//!   The answer's body is the answer frame. An empty body closes `conn`.
//! - **The earlier stage's machine** ([`link_frame_route`]) keeps one loopback
//!   connection per `conn` to the port its plugin gives in `engine.link`
//!   (`{status, port, token}`) and passes the frames through. Its own
//!   `engine.load` gets `link: {}`: the plugin makes the secret.
//!
//! The node reads nothing of a frame but its length. Only a live peer serving
//! the next slice of the same deployment gets the secret or a frame through.

use crate::gateway::GatewayState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};

/// The largest frame either way: a prompt piece's hidden states.
pub(crate) const MAX_FRAME: usize = 64 << 20;
/// One frame may run a long prompt piece through the earlier stage.
const FRAME_TIMEOUT: Duration = Duration::from_secs(900);
/// How long a later stage waits for the earlier one to load its slice.
const WAIT_EARLIER: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(3);
/// Loopback connections the earlier stage's machine keeps open at once.
const MAX_CONNECTIONS: usize = 16;

static GATEWAY: OnceLock<GatewayState> = OnceLock::new();

/// The gateway state, for the runtime supervisor that starts the relay.
pub(crate) fn install(state: GatewayState) {
    let _ = GATEWAY.set(state);
}

/// Whether this deployment is one stage of a model split across machines.
pub(crate) fn is_stage(cfg: &crate::runtime::NodeConfig) -> bool {
    cfg.total_layers > 0 && (cfg.start_layer > 0 || cfg.end_layer + 1 < cfg.total_layers)
}

/// A slug the node accepts as a deployment or connection id in a path.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The earlier stage of `cfg` among the live peers: same deployment, the
/// slice that ends just before this one, served by the same plugin.
async fn earlier_peer(
    state: &GatewayState,
    cfg: &crate::runtime::NodeConfig,
    code: &str,
) -> Option<crate::network::PeerInfo> {
    state.all_peers().await.into_iter().find(|peer| {
        peer.node_id != state.network.node_id
            && peer.deploy_id == cfg.deploy_id
            && peer.end_layer + 1 == cfg.start_layer
            && crate::plugin_engine::plugin_of_capabilities(&peer.inference_frame_capabilities)
                == Some(code)
    })
}

/// The loopback relay of a later stage; it ends when dropped.
pub(crate) struct Relay {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The `link` that `engine.load` receives, and for a later stage the relay
/// behind its `prev_port`. Waits for the earlier stage's machine; `stopped`
/// ends the wait.
pub(crate) async fn prepare(
    cfg: &crate::runtime::NodeConfig,
    code: &str,
    stopped: impl Fn() -> bool,
) -> anyhow::Result<(Value, Option<Relay>)> {
    if cfg.start_layer == 0 {
        return Ok((json!({}), None));
    }
    let state = GATEWAY
        .get()
        .ok_or_else(|| anyhow::anyhow!("the node's gateway is not up"))?
        .clone();
    if !valid_id(&cfg.deploy_id) {
        anyhow::bail!("a split model needs its deployment id");
    }
    let deadline = Instant::now() + WAIT_EARLIER;
    let mut announced = false;
    let token = loop {
        if stopped() {
            anyhow::bail!("stopped while waiting for the earlier stage");
        }
        if let Some(peer) = earlier_peer(&state, cfg, code).await {
            match link_secret(&state, &peer, code, &cfg.deploy_id).await {
                Ok(token) => break token,
                Err(error) if !announced => {
                    info!(plugin = %code, peer = %peer.node_id, %error, "waiting for the earlier stage's link");
                    announced = true;
                }
                Err(_) => {}
            }
        } else if !announced {
            info!(plugin = %code, start_layer = cfg.start_layer, "waiting for the machine of the earlier stage");
            announced = true;
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "no machine of this deployment serves layers up to {} with the plugin '{code}'",
                cfg.start_layer.saturating_sub(1)
            );
        }
        tokio::time::sleep(RETRY).await;
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let relay_cfg = cfg.clone();
    let relay_code = code.to_owned();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let _ = stream.set_nodelay(true);
            tokio::spawn(relay_connection(
                state.clone(),
                relay_cfg.clone(),
                relay_code.clone(),
                stream,
            ));
        }
    });
    Ok((
        json!({ "token": token, "prev_port": port }),
        Some(Relay { task }),
    ))
}

async fn link_secret(
    state: &GatewayState,
    peer: &crate::network::PeerInfo,
    code: &str,
    deploy_id: &str,
) -> Result<String, String> {
    let response = state
        .control_get(
            peer,
            &format!("/api/internal/plugins/{code}/engine/link/{deploy_id}"),
            Duration::from_secs(10),
        )
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(body["error"].as_str().unwrap_or("refused").to_owned());
    }
    body["token"]
        .as_str()
        .filter(|token| valid_token(token))
        .map(str::to_owned)
        .ok_or_else(|| "the earlier stage gave no link secret".into())
}

fn valid_token(token: &str) -> bool {
    (16..=256).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic())
}

async fn read_frame(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    match stream.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

/// One engine connection of a later stage: every frame goes to the earlier
/// stage's machine and its answer comes back. A frame the earlier machine
/// cannot take yet (its stage still loading) is sent again.
async fn relay_connection(
    state: GatewayState,
    cfg: crate::runtime::NodeConfig,
    code: String,
    mut stream: TcpStream,
) {
    let conn = uuid::Uuid::new_v4().simple().to_string();
    let path = format!(
        "/api/internal/plugins/{code}/engine/link/{}/{conn}",
        cfg.deploy_id
    );
    let mut peer: Option<crate::network::PeerInfo> = None;
    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                warn!(plugin = %code, %error, "stage link: the engine's frame");
                break;
            }
        };
        let deadline = Instant::now() + WAIT_EARLIER;
        let answer = loop {
            if peer.is_none() {
                peer = earlier_peer(&state, &cfg, &code).await;
            }
            let attempt = match &peer {
                None => Err("the earlier stage's machine is not in the network".to_owned()),
                Some(target) => send(&state, target, &path, frame.clone()).await,
            };
            match attempt {
                Ok(answer) => break Some(answer),
                Err(error) if Instant::now() < deadline => {
                    warn!(plugin = %code, %error, "stage link: trying again");
                    peer = None;
                    tokio::time::sleep(RETRY).await;
                }
                Err(error) => {
                    warn!(plugin = %code, %error, "stage link: giving up");
                    break None;
                }
            }
        };
        let Some(answer) = answer else { break };
        if write_frame(&mut stream, &answer).await.is_err() {
            break;
        }
    }
    if let Some(target) = &peer {
        let _ = send(&state, target, &path, Vec::new()).await;
    }
}

/// A frame that the earlier machine answered, or why not. A refusal of the
/// frame itself (`4xx` other than a wait) is not tried again by the caller
/// any differently: the engine sees the connection fail and starts over.
async fn send(
    state: &GatewayState,
    peer: &crate::network::PeerInfo,
    path: &str,
    frame: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let response = state
        .control_post_bytes_routed(peer, path, frame, FRAME_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    if status.is_success() {
        return Ok(body.to_vec());
    }
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| status.to_string());
    Err(message)
}

// ── The earlier stage's machine ──────────────────────────────────────────────

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

/// Whether the signed caller may use this machine's stage link: this machine
/// serves `deploy` with plugin `code`, and the caller is a live peer serving
/// the slice that starts just after this one.
async fn authorize(
    state: &GatewayState,
    auth: &crate::auth::InternalNodeAuth,
    code: &str,
    deploy: &str,
) -> Result<String, Response> {
    let Some(principal) = auth.principal() else {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "a signed peer call is required",
        ));
    };
    if !crate::plugin_engine::valid_code(code) || !valid_id(deploy) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "invalid plugin or deployment",
        ));
    }
    let Some(cfg) = state.runtime.config().await else {
        return Err(refuse(StatusCode::CONFLICT, "this machine serves no model"));
    };
    if cfg.inference_service.as_deref() != Some(code) || cfg.deploy_id != deploy {
        return Err(refuse(
            StatusCode::CONFLICT,
            "this machine does not serve that deployment with that inference service",
        ));
    }
    let next = state.network.alive_peers().await.into_iter().any(|peer| {
        peer.node_id == principal.node_id
            && peer.deploy_id == deploy
            && peer.start_layer == cfg.end_layer + 1
    });
    if !next {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "the caller does not serve the next stage of this deployment",
        ));
    }
    Ok(principal.node_id.clone())
}

async fn engine_link(state: &GatewayState, code: &str) -> Result<Value, Response> {
    state
        .plugins
        .query_audit(code, "engine.link", json!({}))
        .await
        .ok_or_else(|| {
            refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "the plugin is not active on this machine",
            )
        })
}

/// `GET /api/internal/plugins/:code/engine/link/:deploy`: the link secret of
/// this machine's earlier stage, for the machine of the next stage.
pub(crate) async fn link_secret_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path((code, deploy)): Path<(String, String)>,
) -> Response {
    if let Err(refusal) = authorize(&state, &auth, &code, &deploy).await {
        return refusal;
    }
    let link = match engine_link(&state, &code).await {
        Ok(link) => link,
        Err(refusal) => return refusal,
    };
    match link["token"].as_str().filter(|token| valid_token(token)) {
        Some(token) => axum::Json(json!({ "token": token })).into_response(),
        None => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "the earlier stage is not loading a slice yet",
        ),
    }
}

type Connection = Arc<tokio::sync::Mutex<TcpStream>>;

fn connections() -> &'static Mutex<HashMap<(String, String), (Instant, Connection)>> {
    static CONNECTIONS: OnceLock<Mutex<HashMap<(String, String), (Instant, Connection)>>> =
        OnceLock::new();
    CONNECTIONS.get_or_init(Default::default)
}

fn take_connection(key: &(String, String)) -> Option<Connection> {
    let mut map = connections()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    map.get_mut(key).map(|(used, connection)| {
        *used = Instant::now();
        Arc::clone(connection)
    })
}

fn keep_connection(key: (String, String), connection: Connection) {
    let mut map = connections()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    while map.len() >= MAX_CONNECTIONS {
        let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (used, _))| *used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        map.remove(&oldest);
    }
    map.insert(key, (Instant::now(), connection));
}

fn drop_connection(key: &(String, String)) {
    connections()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(key);
}

/// `POST /api/internal/plugins/:code/engine/link/:deploy/:conn`: one frame of
/// the next stage's engine for this machine's earlier stage; the answer frame
/// is the body. An empty body closes the connection.
pub(crate) async fn link_frame_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path((code, deploy, conn)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    let caller = match authorize(&state, &auth, &code, &deploy).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    if !valid_id(&conn) {
        return refuse(StatusCode::BAD_REQUEST, "invalid connection");
    }
    if body.len() > MAX_FRAME {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "frame too large");
    }
    let key = (caller, conn);
    if body.is_empty() {
        drop_connection(&key);
        return StatusCode::OK.into_response();
    }
    let connection = match take_connection(&key) {
        Some(connection) => connection,
        None => {
            let link = match engine_link(&state, &code).await {
                Ok(link) => link,
                Err(refusal) => return refusal,
            };
            let port = link["port"]
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0);
            let (Some("ready"), Some(port)) = (link["status"].as_str(), port) else {
                return refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    link["error"]
                        .as_str()
                        .unwrap_or("the earlier stage is still loading"),
                );
            };
            let stream = match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => stream,
                Err(error) => {
                    return refuse(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &format!("the earlier stage does not answer: {error}"),
                    )
                }
            };
            let _ = stream.set_nodelay(true);
            let connection = Arc::new(tokio::sync::Mutex::new(stream));
            keep_connection(key.clone(), Arc::clone(&connection));
            connection
        }
    };
    let mut stream = connection.lock().await;
    let exchanged = tokio::time::timeout(FRAME_TIMEOUT, async {
        write_frame(&mut stream, &body).await?;
        read_frame(&mut stream).await
    })
    .await;
    match exchanged {
        Ok(Ok(Some(answer))) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            answer,
        )
            .into_response(),
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
            drop(stream);
            drop_connection(&key);
            refuse(StatusCode::BAD_GATEWAY, "the earlier stage closed the link")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_is_a_stage_and_a_whole_model_is_not() {
        let base =
            crate::runtime::NodeConfig::from_params(&crate::webui::RuntimeParams::default(), true);
        let whole = crate::runtime::NodeConfig {
            total_layers: 48,
            start_layer: 0,
            end_layer: 47,
            ..base.clone()
        };
        assert!(!is_stage(&whole));
        let head = crate::runtime::NodeConfig {
            end_layer: 26,
            ..whole.clone()
        };
        let tail = crate::runtime::NodeConfig {
            start_layer: 27,
            ..whole.clone()
        };
        assert!(is_stage(&head) && is_stage(&tail));
    }

    #[test]
    fn ids_and_tokens_are_checked() {
        assert!(valid_id("d-f151dfc9") && valid_id("0123abcd"));
        for bad in ["", "a/b", "a b", "..", &"x".repeat(129)] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(valid_token(&"k".repeat(32)));
        assert!(!valid_token("short") && !valid_token(&format!("{} x", "k".repeat(20))));
    }

    #[tokio::test]
    async fn frames_round_trip_with_their_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(Some(frame)) = read_frame(&mut stream).await {
                write_frame(&mut stream, &frame).await.unwrap();
            }
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        for payload in [b"W\x01\x02".to_vec(), vec![7u8; 100_000]] {
            write_frame(&mut client, &payload).await.unwrap();
            assert_eq!(read_frame(&mut client).await.unwrap().unwrap(), payload);
        }
        drop(client);
        echo.await.unwrap();
    }
}
