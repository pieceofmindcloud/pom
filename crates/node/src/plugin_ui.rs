//! Interface provided by plugins: JSON manifest, screens and assets.
//!
//! The manifest and the assets travel **inside** the plugin artifact (the same
//! binary verified by SHA-256 at install time): the plugin host answers the
//! `ui.manifest` and `ui.asset` operations of the generic `query`, so nothing
//! changes on the license server and the manifest never diverges from the
//! installed backend version. `pom` validates everything here before handing it
//! to the browser; the frontend only knows the `pom-plugin-ui/v1` contract,
//! never a specific plugin. See `docs/subsystems/plugins-ui.md`.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const MANIFEST_SCHEMA: &str = "pom-plugin-ui/v1";
const PROFILES: [&str; 3] = ["admin", "worker", "visitor"];
/// Ceiling of one asset: bundled screens fit comfortably, and the host limits the frame.
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MenuItem {
    pub id: String,
    /// Label per language (`{"en": "...", "pt-BR": "..."}`); the frontend falls back to `en`.
    pub label: std::collections::BTreeMap<String, String>,
    /// Icon name from the frontend's known list; an unknown one becomes generic.
    #[serde(default)]
    pub icon: Option<String>,
    pub to: String,
    /// Old field kept in the contract; the current frontend does not group menus.
    #[serde(default)]
    pub group: Option<String>,
    /// Profiles of the new contract; absent admits every profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
    /// Legacy field. Kept for manifests already published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    /// Submenus of the item. With children, the item becomes an expandable
    /// section; the parent's `to` stays required so that hosts without submenu
    /// support show at least the main destination.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<MenuChild>,
}

/// A submenu. Same rules as a first-level item, with no grandchildren.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MenuChild {
    pub id: String,
    pub label: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct RouteSpec {
    pub path: String,
    pub screen: String,
    /// Roles of the current contract; when absent, falls back to the legacy `profiles`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
    /// Legacy field. Kept for manifests already published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    /// The screen takes the whole viewport (like Dashboard/Logs), with no central column.
    #[serde(default)]
    pub full_bleed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ScreenSpec {
    /// ES module (asset) whose `export` export (default `default`) is the screen's
    /// React component; one module can serve several screens.
    pub module: String,
    #[serde(default)]
    pub export: Option<String>,
    #[serde(default)]
    pub styles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PluginUiManifest {
    pub schema: String,
    pub plugin_code: String,
    /// Optional PNG served by the plugin and used in the navigation menu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_image: Option<String>,
    /// Short description per language (`en` required when present), shown on
    /// the Plugins screen.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub description: std::collections::BTreeMap<String, String>,
    /// Screenshots: paths relative to the root of the plugin's GitHub repository
    /// or `https://` URLs. The POM does not serve them: the browser loads them
    /// straight from GitHub.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<String>,
    #[serde(default)]
    pub menu: Vec<MenuItem>,
    #[serde(default)]
    pub routes: Vec<RouteSpec>,
    pub screens: std::collections::BTreeMap<String, ScreenSpec>,
    /// Extension slots inside native POM screens: slot id -> key of `screens`.
    /// The host mounts that component in place of (or next to) its own basic
    /// content, e.g. `chat.telemetry`. Unknown slot ids are accepted and
    /// ignored by hosts that do not offer them.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub slots: std::collections::BTreeMap<String, String>,
    /// Plugin text catalogs: language -> flat JSON asset
    /// (`{"<plugin_code>.key": "ICU text"}`). The plugin owns its own texts; the
    /// POM only loads and resolves them. If declared, `en` is required.
    #[serde(default)]
    pub i18n: std::collections::BTreeMap<String, String>,
    /// Markdown pages shipped by the plugin and presented with the POM docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<PluginDocumentation>,
    /// Closed list of what `/assets/*` may serve.
    pub assets: Vec<String>,
    /// Lifecycle of the plugin process. `on_demand` starts the host only
    /// when the operator opens the plugin from the menu, and stops it with the X badge.
    #[serde(default, skip_serializing_if = "Lifecycle::is_resident")]
    pub lifecycle: Lifecycle,
}

/// How the plugin process is kept running.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Lifecycle {
    /// Starts with `pom` and is restarted by the heartbeat (default).
    #[default]
    Resident,
    /// Starts only when opened and can be stopped at any time.
    OnDemand,
}

impl Lifecycle {
    fn is_resident(&self) -> bool {
        *self == Lifecycle::Resident
    }

    pub(crate) fn is_on_demand(self) -> bool {
        self == Lifecycle::OnDemand
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PluginDocumentation {
    #[serde(default)]
    pub guides: Vec<PluginDocumentationEntry>,
    #[serde(default)]
    pub reference: Vec<PluginDocumentationEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PluginDocumentationEntry {
    pub id: String,
    pub title: std::collections::BTreeMap<String, String>,
    pub asset: String,
}

const MAX_CATALOG_KEYS: usize = 5000;
const MAX_SLOTS: usize = 16;

/// A slot id is a short dotted name such as `chat.telemetry`.
fn valid_slot_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
}
const MAX_CATALOG_TEXT: usize = 8 * 1024;

fn valid_locale(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let language = parts.next().unwrap_or("");
    (2..=3).contains(&language.len())
        && language.chars().all(|c| c.is_ascii_lowercase())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Validates the content of a catalog: a flat object of texts, every key in the
/// `<plugin_code>.` namespace (a plugin does not overwrite text of the POM or of
/// another plugin), with ceilings on count and size.
pub(crate) fn validate_catalog(plugin_code: &str, value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("catalog is not an object")?;
    if object.len() > MAX_CATALOG_KEYS {
        return Err("catalog with too many keys".into());
    }
    let prefix = format!("{plugin_code}.");
    for (key, text) in object {
        if !key.starts_with(&prefix) {
            return Err(format!("key {key:?} outside the namespace {prefix:?}"));
        }
        match text.as_str() {
            Some(text) if text.len() <= MAX_CATALOG_TEXT => {}
            _ => return Err(format!("invalid text for {key:?}")),
        }
    }
    Ok(())
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.starts_with('/')
        && !path.contains("//")
        && !path.contains("..")
        && !path.contains('\\')
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c))
}

fn valid_asset(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains("..")
        && !path.contains("//")
        && !path.contains('\\')
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c))
}

fn valid_documentation_asset(path: &str) -> bool {
    valid_asset(path) && path.starts_with("docs/") && path.ends_with(".md")
}

fn normalize_ui_manifest_documentation(value: Value) -> Result<Value, String> {
    let Some(paths) = value.get("documentation").and_then(Value::as_array) else {
        return Ok(value);
    };
    if paths.len() > 64 {
        return Err("documentation path list has too many entries".into());
    }
    let mut guides = Vec::with_capacity(paths.len());
    for (index, item) in paths.iter().enumerate() {
        let asset = item
            .as_str()
            .filter(|path| valid_documentation_asset(path))
            .ok_or_else(|| "documentation entries must be declared docs/*.md assets".to_owned())?;
        let file_name = asset.rsplit('/').next().unwrap_or(asset);
        let stem = file_name.strip_suffix(".md").unwrap_or(file_name);
        let title = stem.replace(['-', '_'], " ");
        guides.push(serde_json::json!({
            "id": format!("legacy-guide-{}", index + 1),
            "title": {
                "en": format!("Guide {}: {title}", index + 1),
                "pt-BR": format!("Guide {}: {title}", index + 1)
            },
            "asset": asset
        }));
    }
    let mut value = value;
    value["documentation"] = serde_json::json!({ "guides": guides, "reference": [] });
    Ok(value)
}

const MAX_SCREENSHOTS: usize = 8;
const MAX_DESCRIPTION_TEXT: usize = 2000;

/// Safe relative path inside the repository or `https://` URL without
/// credentials. Nothing here is served by the node.
pub(crate) fn valid_screenshot(value: &str) -> bool {
    if value.len() > 1024 {
        return false;
    }
    if let Some(rest) = value.strip_prefix("https://") {
        return !rest.is_empty()
            && !rest.contains('@')
            && !rest.contains(char::is_whitespace)
            && reqwest::Url::parse(value).is_ok();
    }
    valid_asset(value)
        && [".png", ".jpg", ".jpeg", ".webp", ".gif"]
            .iter()
            .any(|extension| value.to_ascii_lowercase().ends_with(extension))
}

fn valid_profiles(profiles: &[String]) -> bool {
    !profiles.is_empty() && profiles.iter().all(|p| PROFILES.contains(&p.as_str()))
}

fn valid_roles(roles: &[String]) -> bool {
    roles.iter().all(|role| PROFILES.contains(&role.as_str()))
}

impl PluginUiManifest {
    /// Parses and validates the JSON returned by the plugin. Refuses instead of
    /// fixing: a crooked manifest cannot draw a menu or load code.
    pub(crate) fn parse(expected_code: &str, value: Value) -> Result<Self, String> {
        let value = normalize_ui_manifest_documentation(value)?;
        let manifest: Self =
            serde_json::from_value(value).map_err(|e| format!("unreadable manifest: {e}"))?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(format!("schema {:?} not supported", manifest.schema));
        }
        if manifest.plugin_code != expected_code {
            return Err("manifest plugin_code is not that of the installed plugin".into());
        }
        if let Some(icon) = &manifest.icon_image {
            if !icon.ends_with(".png") || !manifest.assets.iter().any(|asset| asset == icon) {
                return Err("icon_image must be a declared PNG asset".into());
            }
        }
        if !manifest.description.is_empty()
            && (!manifest.description.contains_key("en")
                || manifest.description.iter().any(|(locale, text)| {
                    !valid_locale(locale)
                        || text.trim().is_empty()
                        || text.len() > MAX_DESCRIPTION_TEXT
                }))
        {
            return Err("description needs en and short texts per language".into());
        }
        if manifest.screenshots.len() > MAX_SCREENSHOTS
            || !manifest
                .screenshots
                .iter()
                .all(|shot| valid_screenshot(shot))
        {
            return Err("invalid screenshots".into());
        }
        let mut menu_ids = std::collections::HashSet::new();
        for item in &manifest.menu {
            let valid_role_fields = item.roles.as_deref().is_none_or(valid_roles)
                && item.profiles.as_deref().is_none_or(valid_profiles);
            if item.id.is_empty()
                || !valid_path(&item.to)
                || !valid_role_fields
                || !menu_ids.insert(item.id.as_str())
            {
                return Err(format!("invalid menu item {:?}", item.id));
            }
            if !item.label.contains_key("en") {
                return Err(format!("menu item {:?} without an en label", item.id));
            }
            for child in &item.children {
                if child.id.is_empty()
                    || !valid_path(&child.to)
                    || !child.roles.as_deref().is_none_or(valid_roles)
                    || !child.label.contains_key("en")
                    || !menu_ids.insert(child.id.as_str())
                {
                    return Err(format!("invalid submenu {:?}", child.id));
                }
            }
        }
        for route in &manifest.routes {
            let valid_role_fields = route.roles.as_deref().is_none_or(valid_roles)
                && route.profiles.as_deref().is_none_or(valid_profiles);
            if !valid_path(&route.path)
                || !valid_role_fields
                || !manifest.screens.contains_key(&route.screen)
            {
                return Err(format!("invalid route {:?}", route.path));
            }
        }
        if manifest.slots.len() > MAX_SLOTS {
            return Err("too many slots".into());
        }
        for (slot, screen) in &manifest.slots {
            if !valid_slot_id(slot) || !manifest.screens.contains_key(screen) {
                return Err(format!("invalid slot {slot:?}"));
            }
        }
        for asset in &manifest.assets {
            if !valid_asset(asset) {
                return Err(format!("invalid asset {asset:?}"));
            }
        }
        if !manifest.i18n.is_empty() && !manifest.i18n.contains_key("en") {
            return Err("i18n without the en language".into());
        }
        for (locale, asset) in &manifest.i18n {
            if !valid_locale(locale)
                || !asset.ends_with(".json")
                || !manifest.assets.contains(asset)
            {
                return Err(format!("invalid i18n catalog {locale:?}"));
            }
        }
        if let Some(documentation) = &manifest.documentation {
            for (kind, entries) in [
                ("guide", &documentation.guides),
                ("reference", &documentation.reference),
            ] {
                for entry in entries {
                    if entry.id.is_empty()
                        || !entry.title.contains_key("en")
                        || !entry.title.values().all(|title| !title.is_empty())
                        || !valid_documentation_asset(&entry.asset)
                        || !manifest.assets.iter().any(|asset| asset == &entry.asset)
                    {
                        return Err(format!("invalid documentation {kind}"));
                    }
                }
            }
        }
        for (name, screen) in &manifest.screens {
            let files = std::iter::once(&screen.module).chain(screen.styles.iter());
            for file in files {
                if !manifest.assets.contains(file) {
                    return Err(format!("screen {name:?} uses {file:?} outside assets"));
                }
            }
        }
        Ok(manifest)
    }

    pub(crate) fn serves(&self, asset: &str) -> bool {
        self.assets.iter().any(|a| a == asset)
    }

    pub(crate) fn is_icon_image(&self, asset: &str) -> bool {
        self.icon_image.as_deref() == Some(asset)
    }

    /// What the shell loads for an installed plugin even while its process is
    /// stopped: the i18n catalogs and the menu icon. The node keeps a copy
    /// (`PluginRuntime::shell_asset`).
    pub(crate) fn shell_assets(&self) -> Vec<String> {
        let mut assets: Vec<String> = self.i18n.values().cloned().collect();
        assets.extend(self.icon_image.clone());
        assets.sort();
        assets.dedup();
        assets
    }

    pub(crate) fn is_shell_asset(&self, asset: &str) -> bool {
        self.is_icon_image(asset) || self.i18n.values().any(|path| path == asset)
    }

    pub(crate) fn is_documentation(&self, asset: &str) -> bool {
        self.documentation.as_ref().is_some_and(|documentation| {
            documentation
                .guides
                .iter()
                .chain(documentation.reference.iter())
                .any(|entry| entry.asset == asset)
        })
    }
}

/// Decodes the plugin's `ui.asset` response: `{content_type, base64}`.
pub(crate) fn decode_asset(value: &Value) -> Result<(String, Vec<u8>), String> {
    let content_type = value
        .get("content_type")
        .and_then(Value::as_str)
        .filter(|t| {
            matches!(
                *t,
                "text/javascript"
                    | "text/css"
                    | "image/svg+xml"
                    | "image/png"
                    | "application/json"
                    | "text/markdown"
                    | "font/woff2"
            )
        })
        .ok_or("content_type missing or not allowed")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            value
                .get("base64")
                .and_then(Value::as_str)
                .ok_or("asset without base64")?,
        )
        .map_err(|e| format!("invalid base64: {e}"))?;
    if bytes.len() > MAX_ASSET_BYTES {
        return Err("asset above the cap".into());
    }
    Ok((content_type.to_owned(), bytes))
}

#[derive(Serialize)]
struct ActivePluginUi {
    code: String,
    version: Option<String>,
    manifest: PluginUiManifest,
    /// On-demand plugin: the menu shows the X when `running` and opens the process on click.
    on_demand: bool,
    running: bool,
}

/// `GET /api/ui/plugins/ui`: manifests of the active plugins. An on-demand plugin
/// whose host is stopped stays listed with its saved manifest, so the menu can
/// open it. An inactive or absent plugin, or one with an invalid manifest, does
/// not appear - the frontend then shows only the promotional fallbacks.
pub(crate) async fn list_plugin_ui(State(state): State<crate::gateway::GatewayState>) -> Response {
    let mut out = Vec::new();
    for status in state.plugins.all_statuses().await {
        if !status.active {
            continue;
        }
        if let Some(manifest) = state.plugins.ui_manifest(&status.code).await {
            out.push(ActivePluginUi {
                code: status.code.clone(),
                version: status.plugin_version.clone(),
                manifest,
                on_demand: status.on_demand,
                running: status.running,
            });
        }
    }
    axum::Json(json!({ "plugins": out })).into_response()
}

/// `POST /api/ui/plugins/:code/open`: starts the process of an on-demand plugin.
/// Called by the menu click; the license is not touched.
pub(crate) async fn plugin_open(
    State(state): State<crate::gateway::GatewayState>,
    Path(code): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    plugin_lifecycle(&state, &code, &headers, true).await
}

/// `POST /api/ui/plugins/:code/close`: stops the process of an on-demand plugin
/// (the X badge). The plugin stays installed and comes back with `open`.
pub(crate) async fn plugin_close(
    State(state): State<crate::gateway::GatewayState>,
    Path(code): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    plugin_lifecycle(&state, &code, &headers, false).await
}

async fn plugin_lifecycle(
    state: &crate::gateway::GatewayState,
    code: &str,
    headers: &axum::http::HeaderMap,
    open: bool,
) -> Response {
    if !crate::plugin_proxy::same_origin(headers) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "error": "cross-origin plugin call" })),
        )
            .into_response();
    }
    let status = state.plugins.status(code).await;
    if !status.as_ref().is_some_and(|status| status.on_demand) {
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "plugin is not on demand" })),
        )
            .into_response();
    }
    let result = if open {
        state.plugins.open_on_demand(code).await
    } else {
        state.plugins.close_on_demand(code).await
    };
    match result {
        Ok(()) => axum::Json(json!({ "code": code, "running": open })).into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

/// Largest JSON body a plugin RPC accepts.
const MAX_RPC_BODY: usize = 256 * 1024;

/// An RPC operation name: `directory.set_node`, `insights`.
fn valid_rpc_op(op: &str) -> bool {
    (1..=64).contains(&op.len())
        && op.starts_with(|c: char| c.is_ascii_lowercase())
        && op
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// `POST /api/ui/plugins/:code/rpc/:op`: a plugin's own API for its screens.
///
/// The body (a JSON value, at most 256 KiB) reaches the plugin's generic
/// `query` as operation `rpc.<op>` with `{params, profile, node_id}`; the plugin's
/// JSON reply is returned as is. The node knows no operation: the plugin owns
/// its API and authorizes by `profile`. Only active plugins answer, and the
/// request must come from the same origin as the UI.
pub(crate) async fn plugin_rpc(
    State(state): State<crate::gateway::GatewayState>,
    Path((code, op)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !crate::plugin_proxy::same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "error": "cross-origin plugin call" })),
        )
            .into_response();
    }
    if !valid_rpc_op(&op) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid operation" })),
        )
            .into_response();
    }
    if body.len() > MAX_RPC_BODY {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(json!({ "error": "body too large" })),
        )
            .into_response();
    }
    let params: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "body is not JSON" })),
                )
                    .into_response()
            }
        }
    };
    let profile = state.auth.read().await.profile;
    // The node's own id lets the plugin keep a non-admin to the requests this
    // node received: rows from before it lost the admin role stay in its store.
    match state
        .plugins
        .query_audit(
            &code,
            &format!("rpc.{op}"),
            json!({
                "params": params,
                "profile": profile.to_string(),
                "node_id": state.network.node_id,
            }),
        )
        .await
    {
        Some(reply) => axum::Json(reply).into_response(),
        None => (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({ "error": "plugin unavailable or the operation failed" })),
        )
            .into_response(),
    }
}

/// `GET /api/ui/plugins/:code/assets/*path`: asset declared in the manifest.
pub(crate) async fn plugin_asset(
    State(state): State<crate::gateway::GatewayState>,
    Path((code, path)): Path<(String, String)>,
) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "asset not found").into_response();
    let Some(manifest) = state.plugins.ui_manifest(&code).await else {
        return not_found();
    };
    if !manifest.serves(&path) {
        return not_found();
    }
    let shell = manifest.is_shell_asset(&path);
    let reply = match state
        .plugins
        .query_audit(&code, "ui.asset", json!({ "path": path }))
        .await
    {
        Some(reply) => {
            if shell && decode_asset(&reply).is_ok() {
                state.plugins.keep_shell_asset(&code, &path, &reply);
            }
            Some(reply)
        }
        // A stopped on-demand plugin keeps its menu: its labels and icon come
        // from the copy kept while it ran, checked below like a live answer.
        None if shell => state.plugins.shell_asset(&code, &path),
        None => None,
    };
    let Some(reply) = reply else {
        return not_found();
    };
    let decoded = decode_asset(&reply).and_then(|(content_type, bytes)| {
        if manifest.is_icon_image(&path) && content_type != "image/png" {
            return Err("icon_image was not served as PNG".into());
        }
        if manifest.is_documentation(&path) && content_type != "text/markdown" {
            return Err("documentation was not served as Markdown".into());
        }
        // Text catalog: only served if the content obeys the contract.
        if manifest.i18n.values().any(|asset| *asset == path) {
            let parsed: Value =
                serde_json::from_slice(&bytes).map_err(|e| format!("unreadable catalog: {e}"))?;
            validate_catalog(&code, &parsed)?;
        }
        Ok((content_type, bytes))
    });
    match decoded {
        Ok((content_type, bytes)) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache".to_owned()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(plugin_code = %code, %path, %error, "plugin asset refused");
            not_found()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Value {
        json!({
            "schema": "pom-plugin-ui/v1",
            "plugin_code": "logs",
            "menu": [{"id":"logs","label":{"en":"Request logs","pt-BR":"Logs"},"icon":"list-tree","to":"/logs","group":"logs","profiles":["admin"]}],
            "routes": [{"path":"/logs","screen":"logs","profiles":["admin","worker"],"full_bleed":true}],
            "screens": {"logs": {"module":"ui/logs.js","styles":["ui/logs.css"]}},
            "i18n": {"en":"i18n/en.json","pt-BR":"i18n/pt-BR.json"},
            "assets": ["ui/logs.js","ui/logs.css","i18n/en.json","i18n/pt-BR.json"]
        })
    }

    #[test]
    fn the_shell_assets_are_the_catalogs_and_the_menu_icon() {
        let mut value = manifest();
        value["icon_image"] = json!("ui/icon.png");
        value["assets"].as_array_mut().unwrap().push(json!("ui/icon.png"));
        let parsed = PluginUiManifest::parse("logs", value).expect("valid");
        assert_eq!(parsed.shell_assets(), ["i18n/en.json", "i18n/pt-BR.json", "ui/icon.png"]);
        assert!(parsed.is_shell_asset("i18n/pt-BR.json"));
        assert!(!parsed.is_shell_asset("ui/logs.js"));
    }

    #[test]
    fn accepts_a_well_formed_manifest() {
        let parsed = PluginUiManifest::parse("logs", manifest()).expect("valid");
        assert!(parsed.serves("ui/logs.js"));
        assert_eq!(parsed.menu[0].icon.as_deref(), Some("list-tree"));
        assert!(!parsed.serves("ui/other.js"));
    }

    #[test]
    fn accepts_plugin_icon_and_documentation_assets() {
        let mut value = manifest();
        value["icon_image"] = json!("ui/icon.png");
        value["documentation"] = json!({
            "guides": [{
                "id": "getting-started",
                "title": {"en": "Getting started", "pt-BR": "Primeiros passos"},
                "asset": "docs/guides/getting-started.md"
            }],
            "reference": [{
                "id": "configuration",
                "title": {"en": "Configuration"},
                "asset": "docs/reference/configuration.md"
            }]
        });
        value["assets"].as_array_mut().unwrap().extend([
            json!("ui/icon.png"),
            json!("docs/guides/getting-started.md"),
            json!("docs/reference/configuration.md"),
        ]);
        let parsed = PluginUiManifest::parse("logs", value).expect("valid");
        assert!(parsed.is_icon_image("ui/icon.png"));
        assert!(parsed.is_documentation("docs/guides/getting-started.md"));
    }

    #[test]
    fn rejects_icon_or_documentation_outside_declared_assets() {
        let mut value = manifest();
        value["icon_image"] = json!("ui/icon.png");
        assert!(PluginUiManifest::parse("logs", value).is_err());

        let mut value = manifest();
        value["documentation"] = json!({
            "guides": [{
                "id": "bad",
                "title": {"en": "Bad"},
                "asset": "ui/bad.md"
            }]
        });
        assert!(PluginUiManifest::parse("logs", value).is_err());
    }

    #[test]
    fn accepts_optional_menu_roles_and_legacy_profiles() {
        let mut with_roles = manifest();
        with_roles["menu"][0]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        with_roles["menu"][0]["roles"] = json!(["admin", "visitor"]);
        let parsed = PluginUiManifest::parse("logs", with_roles).expect("roles valid");
        assert_eq!(
            parsed.menu[0].roles.as_deref().unwrap(),
            ["admin".to_owned(), "visitor".to_owned()]
        );
        assert_eq!(parsed.menu[0].profiles, None);

        let mut without_roles = manifest();
        without_roles["menu"][0]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        let parsed = PluginUiManifest::parse("logs", without_roles).expect("roles default");
        assert_eq!(parsed.menu[0].roles, None);
        assert_eq!(parsed.menu[0].profiles, None);

        let mut invalid_roles = manifest();
        invalid_roles["menu"][0]["roles"] = json!(["root"]);
        assert!(PluginUiManifest::parse("logs", invalid_roles).is_err());
    }

    #[test]
    fn accepts_route_roles_without_legacy_profiles() {
        let mut with_roles = manifest();
        with_roles["routes"][0]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        with_roles["routes"][0]["roles"] = json!(["admin", "visitor"]);

        let parsed = PluginUiManifest::parse("logs", with_roles).expect("roles valid");
        assert_eq!(
            parsed.routes[0].roles.as_deref().unwrap(),
            ["admin".to_owned(), "visitor".to_owned()]
        );
        assert_eq!(parsed.routes[0].profiles, None);

        let mut invalid_roles = manifest();
        invalid_roles["routes"][0]["roles"] = json!(["root"]);
        assert!(PluginUiManifest::parse("logs", invalid_roles).is_err());
    }

    #[test]
    fn rejects_unsafe_or_inconsistent_manifests() {
        let bad = |edit: fn(&mut Value)| {
            let mut value = manifest();
            edit(&mut value);
            PluginUiManifest::parse("logs", value).is_err()
        };
        assert!(bad(|v| v["schema"] = json!("pom-plugin-ui/v0")));
        assert!(bad(|v| v["plugin_code"] = json!("other")));
        assert!(bad(|v| v["routes"][0]["profiles"] = json!(["root"])));
        assert!(bad(|v| v["routes"][0]["profiles"] = json!([])));
        assert!(bad(|v| v["routes"][0]["screen"] = json!("missing")));
        assert!(bad(|v| v["routes"][0]["path"] = json!("/a/../b")));
        assert!(bad(|v| v["menu"][0]["to"] = json!("logs")));
        assert!(bad(|v| v["menu"][0]["label"] = json!({"pt-BR":"x"})));
        assert!(bad(
            |v| v["assets"] = json!(["../etc/passwd", "ui/logs.js", "ui/logs.css"])
        ));
        assert!(bad(|v| v["assets"] = json!(["ui/logs.js"])));
        assert!(bad(|v| v["i18n"] = json!({"pt-BR":"i18n/pt-BR.json"})));
        assert!(bad(|v| v["i18n"] = json!({"en":"i18n/missing.json"})));
        assert!(bad(|v| v["i18n"] = json!({"en":"ui/logs.js"})));
        assert!(bad(
            |v| v["i18n"] = json!({"en":"i18n/en.json","xx_YY":"i18n/pt-BR.json"})
        ));
        assert!(PluginUiManifest::parse("logs", json!({"schema": 1})).is_err());
    }

    #[test]
    fn accepts_a_catalog_only_inside_the_plugin_namespace() {
        assert!(validate_catalog(
            "logs",
            &json!({"logs.title":"Logs {n, plural, one {# row} other {# rows}}"})
        )
        .is_ok());
        assert!(validate_catalog("logs", &json!({})).is_ok());
        assert!(validate_catalog("logs", &json!({"nav.logs":"hijack"})).is_err());
        assert!(validate_catalog("logs", &json!({"logsx.a":"x"})).is_err());
        assert!(validate_catalog("logs", &json!({"logs.a":1})).is_err());
        assert!(validate_catalog("logs", &json!(["logs.a"])).is_err());
        let huge = "x".repeat(MAX_CATALOG_TEXT + 1);
        assert!(validate_catalog("logs", &json!({"logs.a": huge})).is_err());
    }

    /// `lifecycle` is optional: absent means `resident`; `on_demand` comes from the plugin's own
    /// manifest, and any other value is refused.
    #[test]
    fn lifecycle_comes_from_the_manifest_and_defaults_to_resident() {
        let base = json!({
            "schema": "pom-plugin-ui/v1",
            "plugin_code": "harness",
            "screens": {},
            "assets": []
        });
        let resident = PluginUiManifest::parse("harness", base.clone()).expect("no lifecycle");
        assert_eq!(resident.lifecycle, Lifecycle::Resident);

        let mut on_demand = base.clone();
        on_demand["lifecycle"] = json!("on_demand");
        let parsed = PluginUiManifest::parse("harness", on_demand).expect("on_demand");
        assert!(parsed.lifecycle.is_on_demand());

        let mut unknown = base;
        unknown["lifecycle"] = json!("always");
        assert!(PluginUiManifest::parse("harness", unknown).is_err());
    }

    /// The public pom-plugin-base release currently ships this UI manifest shape.
    #[test]
    fn accepts_the_ui_manifest_format_shipped_by_pom_plugin_base() {
        let value = json!({
            "schema": "pom-plugin-ui/v1",
            "plugin_code": "base",
            "icon_image": "ui/icon.png",
            "documentation": [
                "docs/README.md",
                "docs/guia-desenvolvimento.md",
                "docs/guia-workspace.md"
            ],
            "menu": [
                { "id": "base", "label": { "en": "Base Plugin", "pt-BR": "Plugin de Base" }, "icon": "layout-template", "to": "/plugin-de-base" },
                { "id": "example", "label": { "en": "Example", "pt-BR": "Exemplo" }, "icon": "file-code-2", "to": "/example" }
            ],
            "routes": [
                { "path": "/plugin-de-base", "screen": "base", "full_bleed": true, "roles": ["admin"] },
                { "path": "/example", "screen": "example", "full_bleed": true }
            ],
            "screens": {
                "base": { "module": "ui/screens.js", "export": "base", "styles": ["ui/plugin.css"] },
                "example": { "module": "ui/screens.js", "export": "example", "styles": ["ui/plugin.css"] }
            },
            "i18n": { "en": "i18n/en.json", "pt-BR": "i18n/pt-BR.json" },
            "assets": [
                "ui/screens.js", "ui/plugin.css", "ui/icon.png", "i18n/en.json",
                "i18n/pt-BR.json", "docs/README.md", "docs/guia-desenvolvimento.md",
                "docs/guia-workspace.md"
            ]
        });
        let parsed = PluginUiManifest::parse("base", value).expect("valid published manifest");
        assert_eq!(parsed.menu[0].label["en"], "Base Plugin");
        assert_eq!(parsed.documentation.as_ref().unwrap().guides.len(), 3);
        assert!(parsed.is_documentation("docs/guia-workspace.md"));
    }

    #[test]
    fn menu_children_description_and_screenshots_are_validated() {
        let manifest = |menu: Value, extra: Value| {
            let mut value = json!({
                "schema": "pom-plugin-ui/v1",
                "plugin_code": "base",
                "menu": menu,
                "routes": [
                    { "path": "/tutorial", "screen": "tutorial" },
                    { "path": "/projects", "screen": "tutorial" }
                ],
                "screens": { "tutorial": { "module": "ui/screens.js" } },
                "assets": ["ui/screens.js"]
            });
            for (key, item) in extra.as_object().unwrap() {
                value[key] = item.clone();
            }
            value
        };
        let menu = json!([{
            "id": "base", "label": { "en": "POM - Plugins" }, "to": "/tutorial",
            "children": [
                { "id": "tutorial", "label": { "en": "Plugin" }, "to": "/tutorial" },
                { "id": "projects", "label": { "en": "Projects" }, "to": "/projects", "roles": ["admin"] }
            ]
        }]);
        let parsed = PluginUiManifest::parse(
            "base",
            manifest(
                menu.clone(),
                json!({
                    "description": { "en": "Tutorial", "pt-BR": "Tutorial" },
                    "screenshots": ["docs/shot.png", "https://example.com/a.webp"]
                }),
            ),
        )
        .expect("valid manifest with children");
        assert_eq!(parsed.menu[0].children.len(), 2);
        assert_eq!(parsed.screenshots.len(), 2);
        // Serialization preserves the children for the browser.
        let served = serde_json::to_value(&parsed).unwrap();
        assert_eq!(served["menu"][0]["children"][1]["id"], "projects");

        let duplicated = json!([{
            "id": "base", "label": { "en": "x" }, "to": "/tutorial",
            "children": [{ "id": "base", "label": { "en": "y" }, "to": "/projects" }]
        }]);
        assert!(PluginUiManifest::parse("base", manifest(duplicated, json!({}))).is_err());
        for extra in [
            json!({ "description": { "pt-BR": "without en" } }),
            json!({ "screenshots": ["../fora.png"] }),
            json!({ "screenshots": ["docs/script.js"] }),
            json!({ "screenshots": ["http://example.com/a.png"] }),
        ] {
            assert!(PluginUiManifest::parse("base", manifest(menu.clone(), extra)).is_err());
        }
    }

    /// The manifest the `logs` plugin (pom-plugin-logs) ships today.
    #[test]
    fn accepts_the_manifest_shipped_by_the_logs_plugin() {
        let value: Value = serde_json::from_str(r#"{
          "schema":"pom-plugin-ui/v1","plugin_code":"logs",
          "menu":[{"id":"request-logs","label":{"en":"Request logs","pt-BR":"Logs de Requisições"},"icon":"list-tree","to":"/logs","group":"logs","profiles":["admin"]},
                  {"id":"statistics","label":{"en":"Statistics","pt-BR":"Estatísticas"},"icon":"chart-no-axes-column","to":"/statistics","profiles":["admin"]}],
          "routes":[{"path":"/logs","screen":"logs","profiles":["admin"],"full_bleed":true},
                    {"path":"/statistics","screen":"stats","profiles":["admin"],"full_bleed":true}],
          "screens":{"logs":{"module":"ui/screens.js","export":"logs","styles":["ui/plugin.css"]},
                     "stats":{"module":"ui/screens.js","export":"stats","styles":["ui/plugin.css"]}},
          "i18n":{"en":"i18n/en.json","pt-BR":"i18n/pt-BR.json"},
          "assets":["ui/screens.js","ui/plugin.css","i18n/en.json","i18n/pt-BR.json"]}"#).unwrap();
        let parsed = PluginUiManifest::parse("logs", value).expect("valid");
        assert_eq!(parsed.screens["stats"].export.as_deref(), Some("stats"));
        assert_eq!(parsed.i18n.len(), 2);
    }

    #[test]
    fn rpc_operation_names_are_short_dotted_lowercase() {
        for op in ["insights", "directory.set_node", "projects.rules-v2"] {
            assert!(valid_rpc_op(op), "{op}");
        }
        for op in ["", "Insights", "../x", "a/b", "rpc op", &"a".repeat(65)] {
            assert!(!valid_rpc_op(op), "{op}");
        }
    }

    #[test]
    fn slots_point_at_declared_screens() {
        let mut value = manifest();
        value["slots"] = json!({ "chat.telemetry": "logs" });
        let parsed = PluginUiManifest::parse("logs", value.clone()).expect("valid slot");
        assert_eq!(parsed.slots["chat.telemetry"], "logs");
        let served = serde_json::to_value(&parsed).unwrap();
        assert_eq!(served["slots"]["chat.telemetry"], "logs");

        for slots in [
            json!({ "chat.telemetry": "missing" }),
            json!({ "Chat.Telemetry": "logs" }),
            json!({ "": "logs" }),
            json!({ "chat/telemetry": "logs" }),
        ] {
            let mut bad = value.clone();
            bad["slots"] = slots;
            assert!(PluginUiManifest::parse("logs", bad).is_err());
        }
    }

    #[test]
    fn manifest_without_i18n_is_still_valid() {
        let mut value = manifest();
        value.as_object_mut().unwrap().remove("i18n");
        assert!(PluginUiManifest::parse("logs", value)
            .unwrap()
            .i18n
            .is_empty());
    }

    #[test]
    fn decodes_only_allowed_asset_replies() {
        let ok = json!({"content_type":"text/javascript","base64":"YWxlcnQoMSk="});
        assert_eq!(
            decode_asset(&ok).unwrap(),
            ("text/javascript".into(), b"alert(1)".to_vec())
        );
        assert!(decode_asset(&json!({"content_type":"text/html","base64":"YQ=="})).is_err());
        assert!(decode_asset(&json!({"content_type":"text/css","base64":"***"})).is_err());
        assert!(decode_asset(&json!({"content_type":"text/css"})).is_err());
    }
}
