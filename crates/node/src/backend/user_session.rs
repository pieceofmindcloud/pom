//! People on a node: sign-in through a plugin's provider, the operator path
//! and demo mode.
//!
//! The node knows no identity provider and no OAuth protocol. A plugin that
//! offers sign-in answers `auth.providers` and serves everything under
//! `/auth/<code>/` through the query `auth.http`; when a sign-in completes it
//! answers with a `session`, and the node opens a session of kind `User` for
//! that person (`AdminSessionStore::issue_user`). A user session reaches only
//! the user surface (`user_may_call`), never sees the connection key, and its
//! inference runs with a session chat token that carries `Caller::user_id`.
//!
//! Once a provider is offered, the node password opens a session only at the
//! operator path (`operator_path`), which nothing on the login screen or in the
//! API names. Demo mode (`DEMO_MODE=1`) additionally removes a user's own API
//! keys and redacts addresses from what a user session reads.
//!
//! Design: `docs/superpowers/specs/2026-11-07-demo-mode-and-user-sign-in-design.md`.

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::gateway::GatewayState;

/// Who a browser session is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionPrincipal {
    /// The node's operator: the password, the loopback in local mode, or a
    /// node-to-node request. Everything that existed before user sign-in.
    Operator,
    /// A person who signed in through a plugin's provider.
    User(UserIdentity),
}

impl SessionPrincipal {
    pub fn is_user(&self) -> bool {
        matches!(self, SessionPrincipal::User(_))
    }

    pub fn user(&self) -> Option<&UserIdentity> {
        match self {
            SessionPrincipal::User(user) => Some(user),
            SessionPrincipal::Operator => None,
        }
    }
}

/// The person, as the plugin that signed them in describes them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserIdentity {
    /// Stable id the plugin chose (its own user table's key). Requests,
    /// quotas and logs are scoped by it.
    pub user_id: String,
    /// Name shown on screen and recorded as the key label.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Provider id inside the plugin (`google`, `github`, ...).
    #[serde(default)]
    pub provider: String,
    /// `plugin_code` of the plugin that signed the person in.
    #[serde(default)]
    pub plugin: String,
}

// ── Demo mode ──────────────────────────────────────────────────────────────

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `DEMO_MODE=1` (or the brand's `POM_DEMO_MODE`), read once per process.
pub fn demo_mode() -> bool {
    static DEMO: OnceLock<bool> = OnceLock::new();
    *DEMO.get_or_init(|| {
        let on = std::env::var("DEMO_MODE")
            .ok()
            .or_else(|| crate::brand::env("DEMO_MODE").ok())
            .is_some_and(|value| truthy(&value));
        if on {
            tracing::info!("demo mode: signed-in users see a redacted, read-only surface");
        }
        on
    })
}

// ── Providers offered by plugins ─────────────────────────────────────────────

static SIGN_IN_OFFERED: AtomicBool = AtomicBool::new(false);
const PROVIDERS_TTL: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ProvidersCache {
    at: Option<Instant>,
    providers: Vec<Value>,
    /// Plugins that offer sign-in: the only ones a user session reaches.
    plugins: BTreeSet<String>,
}

fn providers_cache() -> &'static Mutex<ProvidersCache> {
    static CACHE: OnceLock<Mutex<ProvidersCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Whether some plugin offers sign-in. While it does, the password opens a
/// session only at the operator path.
pub fn sign_in_offered() -> bool {
    SIGN_IN_OFFERED.load(Ordering::Relaxed)
}

/// `DEMO_PLUGINS`: the plugin codes a person reaches on a demonstration
/// node besides the sign-in plugin (comma separated). Unset, every active
/// plugin is reachable.
fn demo_plugin_allowlist() -> &'static Option<BTreeSet<String>> {
    static LIST: OnceLock<Option<BTreeSet<String>>> = OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var("DEMO_PLUGINS")
            .ok()
            .or_else(|| crate::brand::env("DEMO_PLUGINS").ok())
            .map(|raw| {
                raw.split(',')
                    .map(|code| code.trim().to_owned())
                    .filter(|code| !code.is_empty())
                    .collect()
            })
    })
}

/// Whether a person reaches plugin `code`: the plugins that sign people in
/// always, and on a demonstration node the allowed ones as well.
fn user_reaches_plugin(code: &str, demo: bool) -> bool {
    if user_plugins().contains(code) {
        return true;
    }
    demo && demo_plugin_allowlist()
        .as_ref()
        .is_none_or(|list| list.contains(code))
}

/// Menu entries and routes of a plugin a person never sees: the sign-in
/// plugin's provider administration.
const OPERATOR_ONLY_PLUGIN_SCREENS: &[&str] = &["auth"];

fn user_plugins() -> BTreeSet<String> {
    providers_cache()
        .lock()
        .map(|cache| cache.plugins.clone())
        .unwrap_or_default()
}

/// Asks every running plugin `auth.providers` (cached `PROVIDERS_TTL`) and
/// returns `[{plugin, id, label, icon, start_url}]` for the login screen.
pub async fn auth_providers(state: &GatewayState) -> Vec<Value> {
    if let Ok(cache) = providers_cache().lock() {
        if cache.at.is_some_and(|at| at.elapsed() < PROVIDERS_TTL) {
            return cache.providers.clone();
        }
    }
    let answers = state.plugins.query_running("auth.providers").await;
    let (providers, plugins) = collect_providers(&answers);
    SIGN_IN_OFFERED.store(!providers.is_empty(), Ordering::Relaxed);
    if let Ok(mut cache) = providers_cache().lock() {
        cache.at = Some(Instant::now());
        cache.providers = providers.clone();
        cache.plugins = plugins;
    }
    providers
}

fn safe_segment(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn collect_providers(answers: &[(String, Value)]) -> (Vec<Value>, BTreeSet<String>) {
    let mut providers = Vec::new();
    let mut plugins = BTreeSet::new();
    for (code, answer) in answers {
        let Some(list) = answer.get("providers").and_then(Value::as_array) else {
            continue;
        };
        for provider in list {
            let Some(id) = provider.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !safe_segment(id) || !safe_segment(code) {
                continue;
            }
            let label = provider
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .chars()
                .take(64)
                .collect::<String>();
            let icon = provider
                .get("icon")
                .and_then(Value::as_str)
                .filter(|icon| safe_segment(icon))
                .unwrap_or("key");
            providers.push(json!({
                "plugin": code,
                "id": id,
                "label": label,
                "icon": icon,
                "start_url": format!("/auth/{code}/oauth/{id}/start"),
            }));
            plugins.insert(code.clone());
        }
    }
    (providers, plugins)
}

// ── Operator path ────────────────────────────────────────────────────────────

fn normalize_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let path = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    let reserved = ["/api", "/v1", "/auth", "/admin", "/admin-ui", "/health", "/peers"];
    (path.len() >= 4
        && path[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        && !reserved.iter().any(|r| path.eq_ignore_ascii_case(r)))
    .then_some(path)
}

/// Where the operator signs in with the node password: `POM_OPERATOR_PATH`,
/// or a random path made on the first start and kept in the project root
/// (`.<slug>-operator-path`). Printed once in the start-up log.
pub fn operator_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Some(path) = crate::brand::env("OPERATOR_PATH")
            .ok()
            .and_then(|raw| normalize_path(&raw))
        {
            return path;
        }
        let file = crate::config::project_root()
            .join(format!(".{}-operator-path", crate::brand::slug()));
        if let Some(path) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| normalize_path(&raw))
        {
            return path;
        }
        let path = format!("/_{}", &uuid::Uuid::new_v4().simple().to_string()[..20]);
        if let Err(error) = std::fs::write(&file, &path) {
            tracing::warn!(%error, "could not keep the operator path; a new one is made on each start");
        }
        path
    })
}

/// Logs the operator path once, so whoever runs the container can find it.
pub fn announce_operator_path() {
    tracing::info!(
        path = operator_path(),
        "operator sign-in path (set {} to choose it)",
        crate::brand::env_var("OPERATOR_PATH")
    );
}

/// Idle limit of a session opened at the operator path.
const OPERATOR_IDLE: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct Throttle {
    per_ip: HashMap<IpAddr, (u32, Instant)>,
    window_start: Option<Instant>,
    window_failures: u32,
}

fn throttle() -> &'static Mutex<Throttle> {
    static THROTTLE: OnceLock<Mutex<Throttle>> = OnceLock::new();
    THROTTLE.get_or_init(Default::default)
}

const FREE_ATTEMPTS: u32 = 3;
const MAX_LOCK: Duration = Duration::from_secs(15 * 60);
const GLOBAL_FAILURES_PER_MINUTE: u32 = 30;

/// Seconds the client must still wait, if any. After `FREE_ATTEMPTS` the
/// lock doubles per failure up to `MAX_LOCK`; past
/// `GLOBAL_FAILURES_PER_MINUTE` failures from anyone, everyone waits a minute.
fn throttled(ip: IpAddr, now: Instant) -> Option<u64> {
    let throttle = throttle().lock().ok()?;
    if throttle.window_failures >= GLOBAL_FAILURES_PER_MINUTE {
        if let Some(start) = throttle.window_start {
            let until = start + Duration::from_secs(60);
            if until > now {
                return Some((until - now).as_secs().max(1));
            }
        }
    }
    let (failures, last) = throttle.per_ip.get(&ip)?;
    if *failures < FREE_ATTEMPTS {
        return None;
    }
    let lock = Duration::from_secs(1u64 << (failures - FREE_ATTEMPTS).min(10)).min(MAX_LOCK);
    let until = *last + lock;
    (until > now).then(|| (until - now).as_secs().max(1))
}

fn note_failure(ip: IpAddr, now: Instant) {
    let Ok(mut throttle) = throttle().lock() else {
        return;
    };
    let entry = throttle.per_ip.entry(ip).or_insert((0, now));
    entry.0 = entry.0.saturating_add(1);
    entry.1 = now;
    if throttle
        .window_start
        .is_none_or(|start| now.duration_since(start) > Duration::from_secs(60))
    {
        throttle.window_start = Some(now);
        throttle.window_failures = 0;
    }
    throttle.window_failures += 1;
    if throttle.per_ip.len() > 10_000 {
        throttle
            .per_ip
            .retain(|_, (_, last)| now.duration_since(*last) < MAX_LOCK);
    }
}

fn note_success(ip: IpAddr) {
    if let Ok(mut throttle) = throttle().lock() {
        throttle.per_ip.remove(&ip);
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

fn is_secure(request: &Request) -> bool {
    request
        .extensions()
        .get::<crate::serve::TransportSecurity>()
        .is_some_and(|transport| transport.0)
}

fn with_session_cookies(
    mut response: Response,
    headers: &axum::http::HeaderMap,
    secure: bool,
    session_id: &str,
) -> Response {
    let name = crate::auth::admin_session_cookie_name(headers, secure);
    for path in crate::auth::ADMIN_SESSION_COOKIE_PATHS {
        let value = crate::auth::session_cookie_header(&name, session_id, secure, path);
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

fn clear_session_cookies(mut response: Response, headers: &axum::http::HeaderMap, secure: bool) -> Response {
    let name = crate::auth::admin_session_cookie_name(headers, secure);
    for path in crate::auth::ADMIN_SESSION_COOKIE_PATHS {
        let mut value = format!("{name}=; Max-Age=0; Path={path}; HttpOnly; SameSite=Strict");
        if secure {
            value.push_str("; Secure");
        }
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

#[derive(Deserialize)]
struct OperatorLogin {
    password: String,
}

/// `GET <operator_path>/login` answers 204 (the interface's probe that it is
/// on the operator page) and `POST` with `{password}` opens an operator
/// session. Every other path returns `None` and goes on.
async fn operator_login(state: &GatewayState, request: Request) -> Result<Response, Request> {
    let login_path = format!("{}/login", operator_path());
    if request.uri().path() != login_path {
        return Err(request);
    }
    let method = request.method().clone();
    if method == Method::GET || method == Method::HEAD {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if method != Method::POST {
        return Ok(not_found());
    }
    let Some(peer) = request.extensions().get::<crate::serve::PeerAddr>().copied() else {
        return Ok(not_found());
    };
    let ip = peer.0.ip();
    let now = Instant::now();
    if let Some(wait) = throttled(ip, now) {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, wait.to_string())],
            Json(json!({ "error": "too many attempts", "retry_after_secs": wait })),
        )
            .into_response());
    }
    if !crate::plugin_proxy::same_origin(request.headers()) {
        return Ok((StatusCode::FORBIDDEN, Json(json!({ "error": "cross-origin" }))).into_response());
    }
    let secure = is_secure(&request);
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 4096).await else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    let Ok(login) = serde_json::from_slice::<OperatorLogin>(&bytes) else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    let hash = state
        .auth
        .read()
        .await
        .admin_access_token
        .clone()
        .unwrap_or_default();
    if hash.is_empty() || !crate::auth::verify_admin_password(&hash, &login.password) {
        note_failure(ip, now);
        tracing::warn!(%ip, "operator sign-in refused");
        return Ok((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "invalid administrative access password" })),
        )
            .into_response());
    }
    note_success(ip);
    tracing::info!(%ip, "operator signed in at the operator path");
    let session_id = state
        .admin_sessions
        .lock()
        .await
        .issue_operator_idle(&hash, OPERATOR_IDLE);
    Ok(with_session_cookies(
        StatusCode::NO_CONTENT.into_response(),
        &parts.headers,
        secure,
        &session_id,
    ))
}

// ── Plugin sign-in routes (`/auth/<code>/...`) ───────────────────────────────

/// Public prefix the plugins' sign-in flows live under.
pub const AUTH_PREFIX: &str = "/auth/";
const AUTH_BODY_LIMIT: usize = 64 * 1024;

fn redirect_base(headers: &axum::http::HeaderMap, secure: bool) -> String {
    if let Ok(base) = crate::brand::env("OAUTH_REDIRECT_BASE") {
        let base = base.trim().trim_end_matches('/');
        if !base.is_empty() {
            return base.to_owned();
        }
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(if secure { "https" } else { "http" });
    format!("{proto}://{host}")
}

/// Same-site relative path, or `/`.
fn safe_redirect(target: Option<&str>) -> String {
    match target {
        Some(target)
            if target.starts_with('/')
                && !target.starts_with("//")
                && !target.contains('\\')
                && target.len() < 2048 =>
        {
            target.to_owned()
        }
        _ => "/".into(),
    }
}

/// Every method under `/auth/<code>/`: forwarded to plugin `<code>` as the
/// query `auth.http`, and its answer turned into a response. A `session` in
/// the answer opens a user session for that person.
pub(crate) async fn auth_http(
    axum::extract::State(state): axum::extract::State<GatewayState>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some(rest) = path.strip_prefix(AUTH_PREFIX) else {
        return not_found();
    };
    let (code, sub_path) = rest.split_once('/').unwrap_or((rest, ""));
    if !safe_segment(code) || !crate::plugins::is_valid_plugin_code(code) {
        return not_found();
    }
    let code = code.to_owned();
    let sub_path = format!("/{sub_path}");
    let secure = is_secure(&request);
    let method = request.method().clone();
    let query = request.uri().query().unwrap_or("").to_owned();
    let peer_ip = request
        .extensions()
        .get::<crate::serve::PeerAddr>()
        .map(|peer| peer.0.ip().to_string());
    let (parts, body) = request.into_parts();
    let base = redirect_base(&parts.headers, secure);
    let Ok(bytes) = axum::body::to_bytes(body, AUTH_BODY_LIMIT).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let form = String::from_utf8_lossy(&bytes).into_owned();
    let Some(reply) = state
        .plugins
        .query_audit(
            &code,
            "auth.http",
            json!({
                "method": method.as_str(),
                "path": sub_path,
                "query": query,
                "body": form,
                "redirect_base": base,
                "auth_base": format!("{base}{AUTH_PREFIX}{code}"),
                "client_ip": peer_ip,
                "demo_mode": demo_mode(),
            }),
        )
        .await
    else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "sign-in unavailable" })),
        )
            .into_response();
    };
    plugin_auth_response(&state, &code, reply, &parts.headers, secure).await
}

async fn plugin_auth_response(
    state: &GatewayState,
    code: &str,
    reply: Value,
    headers: &axum::http::HeaderMap,
    secure: bool,
) -> Response {
    if let Some(session) = reply.get("session") {
        let Ok(mut user) = serde_json::from_value::<UserIdentity>(session.clone()) else {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "the plugin answered an invalid session" })),
            )
                .into_response();
        };
        if user.user_id.trim().is_empty() {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        user.plugin = code.to_owned();
        user.label = user.label.chars().take(120).collect();
        tracing::info!(user = %user.user_id, provider = %user.provider, plugin = %code, "user signed in");
        let session_id = state.admin_sessions.lock().await.issue_user(user);
        let target = safe_redirect(reply.get("redirect").and_then(Value::as_str));
        let response = (StatusCode::SEE_OTHER, [(header::LOCATION, target)]).into_response();
        return with_session_cookies(response, headers, secure, &session_id);
    }
    let status = reply
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| StatusCode::from_u16(status as u16).ok())
        .unwrap_or(StatusCode::OK);
    if let Some(target) = reply.get("redirect").and_then(Value::as_str) {
        // A sign-in starts by sending the browser to the provider: the only
        // place an absolute address is allowed, and only over HTTPS (or to
        // this node's own pages).
        let external = target.starts_with("https://");
        let target = if external {
            target.to_owned()
        } else {
            safe_redirect(Some(target))
        };
        return (
            StatusCode::FOUND,
            [
                (header::LOCATION, target),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
        )
            .into_response();
    }
    if let Some(html) = reply.get("html").and_then(Value::as_str) {
        return (
            status,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; style-src 'unsafe-inline'; img-src data:",
                ),
            ],
            html.to_owned(),
        )
            .into_response();
    }
    (status, Json(reply.get("json").cloned().unwrap_or(reply))).into_response()
}

// ── Session chat tokens ──────────────────────────────────────────────────────

struct ChatToken {
    caller: crate::reqlog::Caller,
    expires_at: Instant,
}

fn chat_tokens() -> &'static Mutex<HashMap<[u8; 32], ChatToken>> {
    static TOKENS: OnceLock<Mutex<HashMap<[u8; 32], ChatToken>>> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The caller behind a session chat token, while it lives.
pub fn chat_token_caller(token: &str) -> Option<crate::reqlog::Caller> {
    if !token.starts_with("sk-pom-u-") {
        return None;
    }
    let mut tokens = chat_tokens().lock().ok()?;
    let now = Instant::now();
    tokens.retain(|_, token| token.expires_at > now);
    tokens.get(&token_digest(token)).map(|token| token.caller.clone())
}

pub fn caller_for(user: &UserIdentity) -> crate::reqlog::Caller {
    crate::reqlog::Caller {
        key_id: format!("user:{}", user.user_id),
        key_label: user.label.clone(),
        key_node_id: None,
        user_id: Some(user.user_id.clone()),
    }
}

/// A fresh inference token for this person's session. It lives in memory
/// only, as long as a session, and carries the person's id.
fn issue_chat_token(user: &UserIdentity) -> String {
    let token = format!(
        "sk-pom-u-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    if let Ok(mut tokens) = chat_tokens().lock() {
        let now = Instant::now();
        tokens.retain(|_, token| token.expires_at > now);
        tokens.insert(
            token_digest(&token),
            ChatToken {
                caller: caller_for(user),
                expires_at: now + crate::auth::ADMIN_SESSION_TTL,
            },
        );
    }
    token
}

// ── The user surface ─────────────────────────────────────────────────────────

/// What a user session may call. Everything else answers 404.
pub fn user_may_call(method: &Method, path: &str, demo: bool) -> bool {
    let get = method == Method::GET || method == Method::HEAD;
    let post = method == Method::POST;
    let exact_get = [
        "/api/ui/node",
        "/api/ui/cluster",
        "/api/ui/cluster/models",
        "/api/ui/deployments",
        "/api/ui/chat/capabilities",
        "/api/ui/infer/heads",
        "/api/ui/requests",
        "/api/ui/requests/stats",
        "/api/ui/plugins/ui",
        "/api/ui/me",
        "/api/ui/guide",
    ];
    if get && exact_get.contains(&path) {
        return true;
    }
    if get && path.starts_with("/api/ui/guide/") {
        return true;
    }
    if (get || post) && path == "/api/ui/chat/telemetry" {
        return true;
    }
    if post && matches!(path, "/api/ui/chat-key" | "/api/ui/logout") {
        return true;
    }
    // A demonstration node shows, read-only, what the operator has: the
    // installed plugins and the catalog, the models and slices, the
    // network it is in, and the person's own budget.
    if demo && get {
        let demo_get = [
            "/api/ui/plugins",
            "/api/ui/plugins/github-repositories",
            "/api/ui/plugin-catalog",
            "/api/ui/models",
            "/api/ui/network/info",
            "/api/ui/quota",
            "/api/ui/demo/session",
        ];
        if demo_get.contains(&path) || path.starts_with("/api/ui/plugin-catalog/") {
            return true;
        }
    }
    // Previewing a slicing plan only reads GGUF headers: the Slice screen
    // shows it, while starting the job stays refused.
    if demo && post && path == "/api/ui/slice/plan" {
        return true;
    }
    if path == "/api/ui/demo/session" && get {
        return demo;
    }
    if let Some(rest) = path.strip_prefix("/api/ui/plugins/") {
        let (code, rest) = rest.split_once('/').unwrap_or((rest, ""));
        let reaches = user_reaches_plugin(code, demo);
        if get && reaches && rest.starts_with("assets/") {
            return true;
        }
        if post && reaches && rest.starts_with("rpc/") {
            // The sign-in plugin's provider administration is the operator's.
            return !rest.starts_with("rpc/auth.");
        }
        // A plugin's own web interface (the editor, a harness): only on a
        // demonstration node, where each person gets their own folder.
        if demo && reaches && (rest == "proxy" || rest.starts_with("proxy/")) {
            return true;
        }
        return false;
    }
    if !demo && path == "/api/ui/my-keys" && (get || post) {
        return true;
    }
    if !demo && method == Method::DELETE && path.starts_with("/api/ui/my-keys/") {
        return true;
    }
    false
}

/// Paths whose JSON a user session reads only through `redact`. In demo
/// mode the request log and the plugins' answers too: their hops and
/// telemetry name the machines.
fn redacted_path(path: &str, demo: bool) -> bool {
    if demo && (path.starts_with("/api/ui/requests") || path.contains("/rpc/")) {
        return true;
    }
    matches!(
        path,
        "/api/ui/node"
            | "/api/ui/network/info"
            | "/api/ui/cluster"
            | "/api/ui/cluster/models"
            | "/api/ui/deployments"
            | "/api/ui/infer/heads"
            | "/api/ui/chat/telemetry"
            | "/api/ui/plugins/ui"
    )
}

/// Keys a user session never reads: network credentials.
const SECRET_KEYS: &[&str] = &[
    "connection_key",
    "connection_keys",
    "previous_connection_key",
    "admin_access_token",
    "membership_token",
    "membership",
    "api_keys",
    "secret",
    "password",
    "token",
    "relay_pass",
];

fn address_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "ip"
        || key.ends_with("_ip")
        || key.ends_with("_ips")
        || key == "ips"
        || key.contains("addr")
        || key.contains("host")
        || key.contains("endpoint")
        || key.contains("relay")
        || key.contains("url")
        || key.contains("pkarr")
        || key.contains("ticket")
        || key == "latitude"
        || key == "longitude"
        || key == "lat"
        || key == "lon"
}

fn looks_like_address(value: &str) -> bool {
    let value = value.trim();
    value.parse::<IpAddr>().is_ok()
        || value.parse::<std::net::SocketAddr>().is_ok()
        || value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("quic://")
}

fn looks_like_node_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A stable short alias of a node id, the same on every read of this process.
fn alias(node_id: &str) -> String {
    static SALT: OnceLock<String> = OnceLock::new();
    let salt = SALT.get_or_init(|| uuid::Uuid::new_v4().simple().to_string());
    let digest = Sha256::digest(format!("{salt}{node_id}").as_bytes());
    format!(
        "node-{}",
        digest[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// Removes credentials from what a user session reads, and in demo mode
/// every address and node id as well.
pub fn redact(value: &mut Value, demo: bool) {
    match value {
        Value::Object(map) => {
            let keys = map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let lower = key.to_ascii_lowercase();
                if SECRET_KEYS.contains(&lower.as_str()) {
                    map.remove(&key);
                    continue;
                }
                // An address keeps its key and its type, so the interface
                // (which reads `hostname.trim()`, `addrs.length`, ...) still
                // draws: text becomes "hidden", a list empty, anything else null.
                if demo && address_key(&lower) {
                    if let Some(slot) = map.get_mut(&key) {
                        *slot = match slot {
                            Value::String(_) => Value::String("hidden".into()),
                            Value::Array(_) => Value::Array(Vec::new()),
                            _ => Value::Null,
                        };
                    }
                    continue;
                }
                let Some(mut inner) = map.remove(&key) else {
                    continue;
                };
                redact(&mut inner, demo);
                let key = if demo && looks_like_node_id(&key) {
                    alias(&key)
                } else {
                    key
                };
                map.insert(key, inner);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, demo)),
        Value::String(text) if demo => {
            if looks_like_node_id(text) {
                *text = alias(text);
            } else if looks_like_address(text) {
                *text = "hidden".into();
            }
        }
        _ => {}
    }
}

/// Only the plugins a person reaches show their screens to them, without the
/// operator-only screens.
fn keep_user_plugins(value: &mut Value, demo: bool) {
    let Some(list) = value.get_mut("plugins").and_then(Value::as_array_mut) else {
        return;
    };
    list.retain(|plugin| {
        plugin
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| user_reaches_plugin(code, demo))
    });
    for plugin in list.iter_mut() {
        let Some(manifest) = plugin.get_mut("manifest") else {
            continue;
        };
        if let Some(menu) = manifest.get_mut("menu").and_then(Value::as_array_mut) {
            menu.retain(|item| {
                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                !OPERATOR_ONLY_PLUGIN_SCREENS.contains(&id)
            });
        }
        if let Some(routes) = manifest.get_mut("routes").and_then(Value::as_array_mut) {
            routes.retain(|route| {
                let screen = route.get("screen").and_then(Value::as_str).unwrap_or("");
                !OPERATOR_ONLY_PLUGIN_SCREENS.contains(&screen)
            });
        }
    }
}

const REDACT_BODY_LIMIT: usize = 32 * 1024 * 1024;

async fn redact_response(response: Response, path: &str, demo: bool) -> Response {
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, REDACT_BODY_LIMIT).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if path == "/api/ui/plugins/ui" {
        keep_user_plugins(&mut value, demo);
        // Manifests name no machine, and their screenshots are https URLs.
        redact(&mut value, false);
    } else {
        redact(&mut value, demo);
    }
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ETAG);
    Response::from_parts(parts, Body::from(bytes))
}

/// `GET /api/ui/me`. On a demonstration node a person's answer carries their
/// seat (`demo`), read by `seat_of`.
fn me(principal: &SessionPrincipal, seat: Option<Value>) -> Response {
    let demo = demo_mode();
    match principal {
        SessionPrincipal::Operator => Json(json!({ "kind": "operator", "demo_mode": demo })),
        SessionPrincipal::User(user) => {
            let mut body = json!({
                "kind": "user",
                "demo_mode": demo,
                "can_mint_keys": !demo,
                "user": user,
                "workspace": user_folder(user),
            });
            if let Some(seat) = seat {
                body["demo"] = seat;
            }
            Json(body)
        }
    }
    .into_response()
}

/// A person's own folder name inside the workspace: a hash of who they are,
/// so the name says nothing about them and two people never share one.
pub fn user_folder(user: &UserIdentity) -> String {
    let digest = Sha256::digest(format!("{}:{}", user.plugin, user.user_id).as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The absolute folder of a person inside the workspace
/// (`<workspace_root>/users/<hash>`), created on first use. Plugins that work
/// on files (the editor, a harness) receive it and keep to it.
pub async fn user_workspace(state: &GatewayState, user: &UserIdentity) -> std::path::PathBuf {
    let root = state.auth.read().await.workspace_root_path();
    let folder = root.join("users").join(user_folder(user));
    if let Err(error) = std::fs::create_dir_all(&folder) {
        tracing::warn!(%error, "could not create a person's workspace folder");
    }
    folder
}

/// The seat of this session on a demonstration node. Seats that expired in
/// this sweep lose their session and their chat tokens.
async fn seat_of(
    state: &GatewayState,
    user: &UserIdentity,
    headers: &axum::http::HeaderMap,
    secure: bool,
) -> crate::demo_slots::SeatStatus {
    let Some(session) = crate::auth::session_id_from_cookie(headers, secure).map(str::to_owned) else {
        return crate::demo_slots::SeatStatus::Expired;
    };
    let (status, expired) = crate::demo_slots::status(&session, &user.user_id);
    if !expired.is_empty() {
        let mut sessions = state.admin_sessions.lock().await;
        for (session, user_id) in &expired {
            sessions.revoke(session);
            forget_chat_tokens(user_id);
            tracing::info!(user = %user_id, "demo seat expired");
        }
    }
    status
}

fn forget_chat_tokens(user_id: &str) {
    if let Ok(mut tokens) = chat_tokens().lock() {
        tokens.retain(|_, token| token.caller.user_id.as_deref() != Some(user_id));
    }
}

/// `GET /api/ui/quota` for a person: their own budget, from the plugin that
/// signed them in (`rpc.me`).
async fn user_quota(state: &GatewayState, user: &UserIdentity) -> Response {
    let reply = state
        .plugins
        .query_audit(
            &user.plugin,
            "rpc.me",
            json!({ "profile": "user", "user_id": user.user_id, "params": {} }),
        )
        .await;
    match reply.and_then(|reply| reply.get("quota").cloned()).filter(|q| q.is_object()) {
        Some(mut quota) => {
            quota["available"] = json!(true);
            if let Some(map) = quota.as_object_mut() {
                map.remove("node_id");
            }
            Json(quota).into_response()
        }
        None => Json(json!({ "available": false, "configured": false })).into_response(),
    }
}

async fn logout(state: &GatewayState, headers: axum::http::HeaderMap, secure: bool) -> Response {
    if let Some(session_id) = crate::auth::session_id_from_cookie(&headers, secure).map(str::to_owned) {
        state.admin_sessions.lock().await.revoke(&session_id);
        crate::demo_slots::release(&session_id);
    }
    clear_session_cookies(StatusCode::NO_CONTENT.into_response(), &headers, secure)
}

/// Handles the operator path before any other check. `Err` gives the request
/// back to the gate.
pub(crate) async fn before_gate(state: &GatewayState, request: Request) -> Result<Response, Request> {
    operator_login(state, request).await
}

/// What a session reaches once the gate has authenticated it.
pub(crate) async fn after_gate(
    state: &GatewayState,
    principal: SessionPrincipal,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    if path == "/api/ui/logout" && method == Method::POST {
        let secure = is_secure(&request);
        return logout(state, request.headers().clone(), secure).await;
    }
    let SessionPrincipal::User(user) = &principal else {
        if path == "/api/ui/me" && method == Method::GET {
            return me(&principal, None);
        }
        request.extensions_mut().insert(principal);
        return next.run(request).await;
    };
    let demo = demo_mode();
    // A demonstration node opens to a few people at a time: past the seats,
    // a person waits in the queue and reaches only who they are and the
    // queue itself.
    let mut seat_json = None;
    if demo {
        let secure = is_secure(&request);
        let seat = seat_of(state, user, request.headers(), secure).await;
        let now = Instant::now();
        let body = seat.to_json(now);
        match seat {
            crate::demo_slots::SeatStatus::Expired => {
                let response = (
                    StatusCode::UNAUTHORIZED,
                    [("x-pom-admin-error", "demo_session_expired")],
                    Json(json!({ "error": "demo_session_expired" })),
                )
                    .into_response();
                return clear_session_cookies(response, request.headers(), secure);
            }
            crate::demo_slots::SeatStatus::Queued { .. } => {
                if path != "/api/ui/me" && path != "/api/ui/demo/session" {
                    return (
                        StatusCode::LOCKED,
                        Json(json!({ "error": "demo_queue", "demo": body })),
                    )
                        .into_response();
                }
            }
            crate::demo_slots::SeatStatus::Active { .. } => {}
        }
        seat_json = Some(body);
    }
    if path == "/api/ui/me" && method == Method::GET {
        return me(&principal, seat_json);
    }
    if path == "/api/ui/demo/session" && method == Method::GET {
        return match seat_json {
            Some(body) => Json(body).into_response(),
            None => not_found(),
        };
    }
    // The plugin list may have moved on since the cache was filled.
    auth_providers(state).await;
    if !user_may_call(&method, &path, demo) {
        return not_found();
    }
    if path == "/api/ui/quota" {
        return user_quota(state, user).await;
    }
    if path == "/api/ui/chat-key" {
        return (StatusCode::OK, Json(json!({ "key": issue_chat_token(user) }))).into_response();
    }
    if path.starts_with("/api/ui/my-keys") {
        return crate::user_keys::handle(state, user, request).await;
    }
    let user = user.clone();
    request.extensions_mut().insert(principal.clone());
    request.extensions_mut().insert(user);
    let response = next.run(request).await;
    if redacted_path(&path, demo) {
        return redact_response(response, &path, demo).await;
    }
    response
}

/// The caller of a plugin RPC or a request-log query, for the plugin: a
/// user session's `{profile: "user", user_id}`.
pub(crate) fn user_of(extensions: &axum::http::Extensions) -> Option<&UserIdentity> {
    extensions.get::<UserIdentity>()
}

#[allow(dead_code)]
pub(crate) async fn passthrough(request: Request, next: Next) -> Response {
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_reaches_only_the_user_surface() {
        assert!(user_may_call(&Method::GET, "/api/ui/cluster", false));
        assert!(user_may_call(&Method::POST, "/api/ui/chat-key", true));
        assert!(!user_may_call(&Method::GET, "/api/ui/connection", false));
        assert!(!user_may_call(&Method::POST, "/api/ui/leave", true));
        assert!(!user_may_call(&Method::GET, "/admin/api-keys", false));
        assert!(!user_may_call(&Method::POST, "/api/ui/deploy", false));
        assert!(!user_may_call(&Method::POST, "/api/ui/cluster", false));
        assert!(!user_may_call(&Method::GET, "/api/ui/network/relay", false));
        assert!(!user_may_call(&Method::POST, "/api/ui/plugins/base/rpc/x", false));
    }

    #[test]
    fn demo_mode_removes_own_api_keys() {
        assert!(user_may_call(&Method::GET, "/api/ui/my-keys", false));
        assert!(!user_may_call(&Method::GET, "/api/ui/my-keys", true));
        assert!(!user_may_call(&Method::POST, "/api/ui/my-keys", true));
        assert!(!user_may_call(&Method::DELETE, "/api/ui/my-keys/k1", true));
    }

    #[test]
    fn demo_shows_the_catalog_read_only() {
        for path in ["/api/ui/plugins", "/api/ui/models", "/api/ui/network/info", "/api/ui/quota"] {
            assert!(user_may_call(&Method::GET, path, true), "{path}");
            assert!(!user_may_call(&Method::GET, path, false), "{path}");
        }
        assert!(!user_may_call(&Method::POST, "/api/ui/plugins/activate", true));
        assert!(!user_may_call(&Method::POST, "/api/ui/plugins/github-repositories", true));
        assert!(!user_may_call(&Method::POST, "/api/ui/slice", true));
        assert!(!user_may_call(&Method::POST, "/api/transfer/send", true));
        assert!(!user_may_call(&Method::POST, "/api/ui/network/settings", true));
        assert!(!user_may_call(&Method::GET, "/api/ui/network-chat", true));
        assert!(!user_may_call(&Method::GET, "/api/ui/cluster-logs", true));
        assert!(user_may_call(&Method::GET, "/api/ui/demo/session", true));
        assert!(!user_may_call(&Method::GET, "/api/ui/demo/session", false));
    }

    #[test]
    fn the_sign_in_plugins_provider_screens_stay_with_the_operator() {
        let mut value = json!({"plugins": [{"code": "logs", "manifest": {
            "menu": [{"id": "statistics"}, {"id": "auth"}],
            "routes": [{"path": "/statistics", "screen": "stats"}, {"path": "/auth", "screen": "auth"}],
        }}]});
        if let Ok(mut cache) = providers_cache().lock() {
            cache.plugins.insert("logs".into());
        }
        keep_user_plugins(&mut value, true);
        let manifest = &value["plugins"][0]["manifest"];
        assert_eq!(manifest["menu"], json!([{"id": "statistics"}]));
        assert_eq!(manifest["routes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn redaction_always_drops_credentials_and_in_demo_addresses() {
        let node = "a".repeat(64);
        let original = json!({
            "connection_key": "k",
            "node_id": node,
            "peers": [{
                "node_id": node,
                "advertise_addr": "203.0.113.4:42777",
                "relay_url": "https://relay.example",
                "name": "gpu box",
                "note": "198.51.100.7",
                "layers": [0, 31],
            }],
            "by_node": { node.clone(): 3 },
        });
        let mut plain = original.clone();
        redact(&mut plain, false);
        assert!(plain.get("connection_key").is_none());
        assert_eq!(plain["peers"][0]["advertise_addr"], "203.0.113.4:42777");

        let mut demo = original.clone();
        redact(&mut demo, true);
        let text = demo.to_string();
        assert!(!text.contains("203.0.113.4"));
        assert!(!text.contains("198.51.100.7"));
        assert!(!text.contains("relay.example"));
        assert!(!text.contains(&node));
        assert_eq!(demo["peers"][0]["name"], "gpu box");
        assert_eq!(demo["peers"][0]["layers"], json!([0, 31]));
        assert_eq!(demo["node_id"], demo["peers"][0]["node_id"]);
        // The shape stays: the interface reads these fields without guards.
        assert_eq!(demo["peers"][0]["advertise_addr"], "hidden");
        let mut node = json!({"hostname": "box", "listen_addrs": ["10.0.0.1:1"], "lat": 1.5});
        redact(&mut node, true);
        assert_eq!(node, json!({"hostname": "hidden", "listen_addrs": [], "lat": null}));
    }

    #[test]
    fn providers_are_collected_with_a_start_url_and_unsafe_ids_dropped() {
        let answers = vec![(
            "logs".to_owned(),
            json!({"providers": [
                {"id": "google", "label": "Google", "icon": "google"},
                {"id": "../evil", "label": "x"},
            ]}),
        )];
        let (providers, plugins) = collect_providers(&answers);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["start_url"], "/auth/logs/oauth/google/start");
        assert!(plugins.contains("logs"));
    }

    #[test]
    fn operator_paths_are_normalized_and_reserved_ones_refused() {
        assert_eq!(normalize_path("_k7q-console/").as_deref(), Some("/_k7q-console"));
        assert_eq!(normalize_path("/api"), None);
        assert_eq!(normalize_path("/a/b"), None);
        assert_eq!(normalize_path("/x"), None);
    }

    #[test]
    fn redirects_after_sign_in_stay_on_this_site() {
        assert_eq!(safe_redirect(Some("/chat")), "/chat");
        assert_eq!(safe_redirect(Some("//evil.example")), "/");
        assert_eq!(safe_redirect(Some("https://evil.example")), "/");
        assert_eq!(safe_redirect(None), "/");
    }

    #[test]
    fn the_password_throttle_locks_after_free_attempts() {
        let ip: IpAddr = "192.0.2.10".parse().unwrap();
        let now = Instant::now();
        for _ in 0..FREE_ATTEMPTS {
            assert!(throttled(ip, now).is_none());
            note_failure(ip, now);
        }
        assert!(throttled(ip, now).is_some());
        note_success(ip);
        assert!(throttled(ip, now).is_none());
    }

    #[test]
    fn session_chat_tokens_carry_the_person() {
        let user = UserIdentity {
            user_id: "u1".into(),
            label: "Ana".into(),
            email: None,
            provider: "google".into(),
            plugin: "logs".into(),
        };
        let token = issue_chat_token(&user);
        let caller = chat_token_caller(&token).unwrap();
        assert_eq!(caller.user_id.as_deref(), Some("u1"));
        assert!(chat_token_caller("sk-pom-u-unknown").is_none());
    }

    mod gate {
        use super::super::*;
        use crate::gateway::testkit::{body_of, state_with};
        use axum::http::Request as HttpRequest;
        use axum::Router;
        use tower::ServiceExt;

        fn app(state: GatewayState) -> Router {
            Router::new()
                .merge(crate::webui::routes())
                .with_state(state.clone())
                .layer(axum::middleware::from_fn_with_state(
                    state,
                    crate::gateway::require_admin_for_surface,
                ))
        }

        async fn network_state() -> (GatewayState, String) {
            let state = state_with("gate-user", vec![]).await;
            {
                let mut config = state.auth.write().await;
                config.admin_exposure = crate::config::AdminExposure::Network;
                config.admin_access_token =
                    Some(crate::auth::hash_admin_password("operator-pass").unwrap());
                config.connection_key = "the-network-key".into();
            }
            let session = state.admin_sessions.lock().await.issue_user(UserIdentity {
                user_id: "u1".into(),
                label: "Ana".into(),
                email: None,
                provider: "google".into(),
                plugin: "logs".into(),
            });
            (state, session)
        }

        fn request(method: Method, path: &str, session: &str, peer: &str) -> HttpRequest<Body> {
            let cookie = format!("pom_admin_session_http={session}");
            let mut request = HttpRequest::builder()
                .method(method)
                .uri(path)
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr(peer.parse().unwrap()));
            request
        }

        #[tokio::test]
        async fn a_user_session_gets_its_identity_and_never_the_operator_surface() {
            let (state, session) = network_state().await;
            let peer = "198.51.100.20:5000";
            let me = app(state.clone())
                .oneshot(request(Method::GET, "/api/ui/me", &session, peer))
                .await
                .unwrap();
            assert_eq!(me.status(), StatusCode::OK);
            let body = body_of(me).await;
            assert_eq!(body["kind"], "user");
            assert_eq!(body["user"]["user_id"], "u1");

            for path in ["/api/ui/connection", "/api/ui/network/relay", "/admin/api-keys"] {
                let refused = app(state.clone())
                    .oneshot(request(Method::GET, path, &session, peer))
                    .await
                    .unwrap();
                assert_eq!(refused.status(), StatusCode::NOT_FOUND, "{path}");
            }

            let key = app(state.clone())
                .oneshot(request(Method::POST, "/api/ui/chat-key", &session, peer))
                .await
                .unwrap();
            let token = body_of(key).await["key"].as_str().unwrap().to_owned();
            assert_eq!(chat_token_caller(&token).unwrap().user_id.as_deref(), Some("u1"));

            let node = app(state.clone())
                .oneshot(request(Method::GET, "/api/ui/node", &session, peer))
                .await
                .unwrap();
            assert_eq!(node.status(), StatusCode::OK);
            assert!(!body_of(node).await.to_string().contains("the-network-key"));
        }

        #[tokio::test]
        async fn the_operator_path_opens_a_session_and_throttles_wrong_passwords() {
            let (state, _) = network_state().await;
            let login = format!("{}/login", operator_path());
            let post = |password: &str, peer: &str| {
                let mut request = HttpRequest::post(&login)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "password": password }).to_string()))
                    .unwrap();
                request
                    .extensions_mut()
                    .insert(crate::serve::PeerAddr(peer.parse().unwrap()));
                request
            };
            let ok = app(state.clone())
                .oneshot(post("operator-pass", "203.0.113.50:1"))
                .await
                .unwrap();
            assert_eq!(ok.status(), StatusCode::NO_CONTENT);
            assert!(ok.headers().get(header::SET_COOKIE).is_some());
            let mut last = StatusCode::OK;
            for _ in 0..(FREE_ATTEMPTS + 1) {
                last = app(state.clone())
                    .oneshot(post("wrong", "203.0.113.51:1"))
                    .await
                    .unwrap()
                    .status();
            }
            assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
        }
    }
}
