//! Interface fornecida por plugins: manifesto JSON, telas e assets.
//!
//! O manifesto e os assets viajam **dentro** do artefato do plugin (o mesmo
//! binário verificado por SHA-256 na instalação): o host do plugin responde as
//! operações `ui.manifest` e `ui.asset` do `query` genérico, então nada muda no
//! servidor de licenças e o manifesto nunca diverge da versão do backend
//! instalado. O `pom` valida tudo aqui antes de entregar ao navegador; o
//! frontend só conhece o contrato `pom-plugin-ui/v1`, nunca um plugin
//! específico. Ver `docs/subsystems/plugins-ui.md`.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const MANIFEST_SCHEMA: &str = "pom-plugin-ui/v1";
const PROFILES: [&str; 3] = ["admin", "node", "infer"];
/// Teto de um asset: telas empacotadas cabem folgadas, e o host limita o frame.
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MenuItem {
    pub id: String,
    /// Rótulo por idioma (`{"en": "...", "pt-BR": "..."}`); o frontend cai em `en`.
    pub label: std::collections::BTreeMap<String, String>,
    /// Nome de ícone da lista conhecida do frontend; desconhecido vira genérico.
    #[serde(default)]
    pub icon: Option<String>,
    pub to: String,
    /// Campo antigo preservado no contrato; o frontend atual não agrupa menus.
    #[serde(default)]
    pub group: Option<String>,
    /// Perfis do novo contrato; ausente admite todos os perfis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
    /// Campo legado. Mantido para manifestos já publicados.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    /// Submenus do item. Com filhos, o item vira uma seção expansível; o `to`
    /// do pai continua obrigatório para que hosts sem suporte a submenus
    /// mostrem ao menos o destino principal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<MenuChild>,
}

/// Um submenu. Mesmas regras de um item de primeiro nível, sem netos.
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
    /// Roles do contrato atual; na ausência, consulta `profiles` legado.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
    /// Campo legado. Mantido para manifestos já publicados.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    /// A tela ocupa a viewport inteira (como Painel/Logs), sem coluna central.
    #[serde(default)]
    pub full_bleed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ScreenSpec {
    /// Módulo ES (asset) cujo export `export` (padrão `default`) é o componente
    /// React da tela; um módulo pode servir várias telas.
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
    /// Descrição curta por idioma (`en` obrigatório quando presente), mostrada
    /// na tela Plugins.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub description: std::collections::BTreeMap<String, String>,
    /// Capturas de tela: caminhos relativos à raiz do repositório GitHub do
    /// plugin ou URLs `https://`. O POM não as serve: o navegador as carrega
    /// direto do GitHub.
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
    /// Catálogos de texto do plugin: idioma -> asset JSON plano
    /// (`{"<plugin_code>.chave": "texto ICU"}`). O plugin é dono dos próprios
    /// textos; o POM só carrega e resolve. Se declarado, `en` é obrigatório.
    #[serde(default)]
    pub i18n: std::collections::BTreeMap<String, String>,
    /// Markdown pages shipped by the plugin and presented with the POM docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<PluginDocumentation>,
    /// Lista fechada do que `/assets/*` pode servir.
    pub assets: Vec<String>,
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

/// Valida o conteúdo de um catálogo: objeto plano de textos, todas as chaves no
/// espaço `<plugin_code>.` (um plugin não sobrescreve texto do POM nem de outro
/// plugin), com tetos de quantidade e tamanho.
pub(crate) fn validate_catalog(plugin_code: &str, value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("catálogo não é um objeto")?;
    if object.len() > MAX_CATALOG_KEYS {
        return Err("catálogo com chaves demais".into());
    }
    let prefix = format!("{plugin_code}.");
    for (key, text) in object {
        if !key.starts_with(&prefix) {
            return Err(format!("chave {key:?} fora do espaço {prefix:?}"));
        }
        match text.as_str() {
            Some(text) if text.len() <= MAX_CATALOG_TEXT => {}
            _ => return Err(format!("texto de {key:?} inválido")),
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
                "pt-BR": format!("Guia {}: {title}", index + 1)
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

/// Caminho relativo seguro dentro do repositório ou URL `https://` sem
/// credenciais. Nada aqui é servido pelo nó.
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
    /// Interpreta e valida o JSON devolvido pelo plugin. Recusa em vez de
    /// corrigir: um manifesto torto não pode desenhar menu nem carregar código.
    pub(crate) fn parse(expected_code: &str, value: Value) -> Result<Self, String> {
        let value = normalize_ui_manifest_documentation(value)?;
        let manifest: Self =
            serde_json::from_value(value).map_err(|e| format!("manifesto ilegível: {e}"))?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(format!("schema {:?} não suportado", manifest.schema));
        }
        if manifest.plugin_code != expected_code {
            return Err("plugin_code do manifesto não é o do plugin instalado".into());
        }
        if let Some(icon) = &manifest.icon_image {
            if !icon.ends_with(".png") || !manifest.assets.iter().any(|asset| asset == icon) {
                return Err("icon_image precisa ser um asset PNG declarado".into());
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
            return Err("description precisa de en e textos curtos por idioma".into());
        }
        if manifest.screenshots.len() > MAX_SCREENSHOTS
            || !manifest
                .screenshots
                .iter()
                .all(|shot| valid_screenshot(shot))
        {
            return Err("screenshots inválidos".into());
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
                return Err(format!("item de menu {:?} inválido", item.id));
            }
            if !item.label.contains_key("en") {
                return Err(format!("item de menu {:?} sem rótulo en", item.id));
            }
            for child in &item.children {
                if child.id.is_empty()
                    || !valid_path(&child.to)
                    || !child.roles.as_deref().is_none_or(valid_roles)
                    || !child.label.contains_key("en")
                    || !menu_ids.insert(child.id.as_str())
                {
                    return Err(format!("submenu {:?} inválido", child.id));
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
                return Err(format!("rota {:?} inválida", route.path));
            }
        }
        if manifest.slots.len() > MAX_SLOTS {
            return Err("slots demais".into());
        }
        for (slot, screen) in &manifest.slots {
            if !valid_slot_id(slot) || !manifest.screens.contains_key(screen) {
                return Err(format!("slot {slot:?} inválido"));
            }
        }
        for asset in &manifest.assets {
            if !valid_asset(asset) {
                return Err(format!("asset {asset:?} inválido"));
            }
        }
        if !manifest.i18n.is_empty() && !manifest.i18n.contains_key("en") {
            return Err("i18n sem o idioma en".into());
        }
        for (locale, asset) in &manifest.i18n {
            if !valid_locale(locale)
                || !asset.ends_with(".json")
                || !manifest.assets.contains(asset)
            {
                return Err(format!("catálogo i18n {locale:?} inválido"));
            }
        }
        if let Some(documentation) = &manifest.documentation {
            for (kind, entries) in [
                ("guia", &documentation.guides),
                ("referência", &documentation.reference),
            ] {
                for entry in entries {
                    if entry.id.is_empty()
                        || !entry.title.contains_key("en")
                        || !entry.title.values().all(|title| !title.is_empty())
                        || !valid_documentation_asset(&entry.asset)
                        || !manifest.assets.iter().any(|asset| asset == &entry.asset)
                    {
                        return Err(format!("{kind} de documentação inválido"));
                    }
                }
            }
        }
        for (name, screen) in &manifest.screens {
            let files = std::iter::once(&screen.module).chain(screen.styles.iter());
            for file in files {
                if !manifest.assets.contains(file) {
                    return Err(format!("tela {name:?} usa {file:?} fora de assets"));
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

/// Decodifica a resposta `ui.asset` do plugin: `{content_type, base64}`.
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
        .ok_or("content_type ausente ou não permitido")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            value
                .get("base64")
                .and_then(Value::as_str)
                .ok_or("asset sem base64")?,
        )
        .map_err(|e| format!("base64 inválido: {e}"))?;
    if bytes.len() > MAX_ASSET_BYTES {
        return Err("asset acima do teto".into());
    }
    Ok((content_type.to_owned(), bytes))
}

#[derive(Serialize)]
struct ActivePluginUi {
    code: String,
    version: Option<String>,
    manifest: PluginUiManifest,
}

/// `GET /api/ui/plugins/ui`: manifestos dos plugins ativos com host rodando.
/// Plugin inativo, ausente ou com manifesto inválido simplesmente não aparece -
/// o frontend então mostra só os fallbacks de propaganda.
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
            });
        }
    }
    axum::Json(json!({ "plugins": out })).into_response()
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
/// `query` as operation `rpc.<op>` with `{params, profile}`; the plugin's
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
        return (StatusCode::FORBIDDEN, axum::Json(json!({ "error": "cross-origin plugin call" }))).into_response();
    }
    if !valid_rpc_op(&op) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "invalid operation" }))).into_response();
    }
    if body.len() > MAX_RPC_BODY {
        return (StatusCode::PAYLOAD_TOO_LARGE, axum::Json(json!({ "error": "body too large" }))).into_response();
    }
    let params: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "body is not JSON" }))).into_response()
            }
        }
    };
    let profile = state.auth.read().await.profile;
    match state
        .plugins
        .query_audit(
            &code,
            &format!("rpc.{op}"),
            json!({ "params": params, "profile": profile.to_string() }),
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

/// `GET /api/ui/plugins/:code/assets/*path`: asset declarado no manifesto.
pub(crate) async fn plugin_asset(
    State(state): State<crate::gateway::GatewayState>,
    Path((code, path)): Path<(String, String)>,
) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "asset não encontrado").into_response();
    let Some(manifest) = state.plugins.ui_manifest(&code).await else {
        return not_found();
    };
    if !manifest.serves(&path) {
        return not_found();
    }
    let Some(reply) = state
        .plugins
        .query_audit(&code, "ui.asset", json!({ "path": path }))
        .await
    else {
        return not_found();
    };
    let decoded = decode_asset(&reply).and_then(|(content_type, bytes)| {
        if manifest.is_icon_image(&path) && content_type != "image/png" {
            return Err("icon_image não foi servido como PNG".into());
        }
        if manifest.is_documentation(&path) && content_type != "text/markdown" {
            return Err("documentação não foi servida como Markdown".into());
        }
        // Catálogo de texto: só sai se o conteúdo obedece ao contrato.
        if manifest.i18n.values().any(|asset| *asset == path) {
            let parsed: Value =
                serde_json::from_slice(&bytes).map_err(|e| format!("catálogo ilegível: {e}"))?;
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
            tracing::warn!(plugin_code = %code, %path, %error, "asset de plugin recusado");
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
            "plugin_code": "enterprise",
            "menu": [{"id":"logs","label":{"en":"Request logs","pt-BR":"Logs"},"icon":"list-tree","to":"/logs","group":"logs","profiles":["admin"]}],
            "routes": [{"path":"/logs","screen":"logs","profiles":["admin","node"],"full_bleed":true}],
            "screens": {"logs": {"module":"ui/logs.js","styles":["ui/logs.css"]}},
            "i18n": {"en":"i18n/en.json","pt-BR":"i18n/pt-BR.json"},
            "assets": ["ui/logs.js","ui/logs.css","i18n/en.json","i18n/pt-BR.json"]
        })
    }

    #[test]
    fn accepts_a_well_formed_manifest() {
        let parsed = PluginUiManifest::parse("enterprise", manifest()).expect("valid");
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
        let parsed = PluginUiManifest::parse("enterprise", value).expect("valid");
        assert!(parsed.is_icon_image("ui/icon.png"));
        assert!(parsed.is_documentation("docs/guides/getting-started.md"));
    }

    #[test]
    fn rejects_icon_or_documentation_outside_declared_assets() {
        let mut value = manifest();
        value["icon_image"] = json!("ui/icon.png");
        assert!(PluginUiManifest::parse("enterprise", value).is_err());

        let mut value = manifest();
        value["documentation"] = json!({
            "guides": [{
                "id": "bad",
                "title": {"en": "Bad"},
                "asset": "ui/bad.md"
            }]
        });
        assert!(PluginUiManifest::parse("enterprise", value).is_err());
    }

    #[test]
    fn accepts_optional_menu_roles_and_legacy_profiles() {
        let mut with_roles = manifest();
        with_roles["menu"][0]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        with_roles["menu"][0]["roles"] = json!(["admin", "infer"]);
        let parsed = PluginUiManifest::parse("enterprise", with_roles).expect("roles valid");
        assert_eq!(
            parsed.menu[0].roles.as_deref().unwrap(),
            ["admin".to_owned(), "infer".to_owned()]
        );
        assert_eq!(parsed.menu[0].profiles, None);

        let mut without_roles = manifest();
        without_roles["menu"][0]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        let parsed = PluginUiManifest::parse("enterprise", without_roles).expect("roles default");
        assert_eq!(parsed.menu[0].roles, None);
        assert_eq!(parsed.menu[0].profiles, None);

        let mut invalid_roles = manifest();
        invalid_roles["menu"][0]["roles"] = json!(["root"]);
        assert!(PluginUiManifest::parse("enterprise", invalid_roles).is_err());
    }

    #[test]
    fn accepts_route_roles_without_legacy_profiles() {
        let mut with_roles = manifest();
        with_roles["routes"][0]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        with_roles["routes"][0]["roles"] = json!(["admin", "infer"]);

        let parsed = PluginUiManifest::parse("enterprise", with_roles).expect("roles valid");
        assert_eq!(
            parsed.routes[0].roles.as_deref().unwrap(),
            ["admin".to_owned(), "infer".to_owned()]
        );
        assert_eq!(parsed.routes[0].profiles, None);

        let mut invalid_roles = manifest();
        invalid_roles["routes"][0]["roles"] = json!(["root"]);
        assert!(PluginUiManifest::parse("enterprise", invalid_roles).is_err());
    }

    #[test]
    fn rejects_unsafe_or_inconsistent_manifests() {
        let bad = |edit: fn(&mut Value)| {
            let mut value = manifest();
            edit(&mut value);
            PluginUiManifest::parse("enterprise", value).is_err()
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
        assert!(PluginUiManifest::parse("enterprise", json!({"schema": 1})).is_err());
    }

    #[test]
    fn accepts_a_catalog_only_inside_the_plugin_namespace() {
        assert!(validate_catalog(
            "enterprise",
            &json!({"enterprise.logs.title":"Logs {n, plural, one {# row} other {# rows}}"})
        )
        .is_ok());
        assert!(validate_catalog("enterprise", &json!({})).is_ok());
        assert!(validate_catalog("enterprise", &json!({"nav.logs":"hijack"})).is_err());
        assert!(validate_catalog("enterprise", &json!({"enterprisex.a":"x"})).is_err());
        assert!(validate_catalog("enterprise", &json!({"enterprise.a":1})).is_err());
        assert!(validate_catalog("enterprise", &json!(["enterprise.a"])).is_err());
        let huge = "x".repeat(MAX_CATALOG_TEXT + 1);
        assert!(validate_catalog("enterprise", &json!({"enterprise.a": huge})).is_err());
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
        // A serialização preserva os filhos para o navegador.
        let served = serde_json::to_value(&parsed).unwrap();
        assert_eq!(served["menu"][0]["children"][1]["id"], "projects");

        let duplicated = json!([{
            "id": "base", "label": { "en": "x" }, "to": "/tutorial",
            "children": [{ "id": "base", "label": { "en": "y" }, "to": "/projects" }]
        }]);
        assert!(PluginUiManifest::parse("base", manifest(duplicated, json!({}))).is_err());
        for extra in [
            json!({ "description": { "pt-BR": "sem en" } }),
            json!({ "screenshots": ["../fora.png"] }),
            json!({ "screenshots": ["docs/script.js"] }),
            json!({ "screenshots": ["http://example.com/a.png"] }),
        ] {
            assert!(PluginUiManifest::parse("base", manifest(menu.clone(), extra)).is_err());
        }
    }

    /// O manifesto que o plugin `enterprise` (pom-plugin-logs) entrega hoje.
    #[test]
    fn accepts_the_manifest_shipped_by_the_enterprise_plugin() {
        let value: Value = serde_json::from_str(r#"{
          "schema":"pom-plugin-ui/v1","plugin_code":"enterprise",
          "menu":[{"id":"request-logs","label":{"en":"Request logs","pt-BR":"Logs de Requisições"},"icon":"list-tree","to":"/logs","group":"logs","profiles":["admin"]},
                  {"id":"statistics","label":{"en":"Statistics","pt-BR":"Estatísticas"},"icon":"chart-no-axes-column","to":"/estatisticas","profiles":["admin"]}],
          "routes":[{"path":"/logs","screen":"logs","profiles":["admin"],"full_bleed":true},
                    {"path":"/estatisticas","screen":"stats","profiles":["admin"],"full_bleed":true}],
          "screens":{"logs":{"module":"ui/screens.js","export":"logs","styles":["ui/plugin.css"]},
                     "stats":{"module":"ui/screens.js","export":"stats","styles":["ui/plugin.css"]}},
          "i18n":{"en":"i18n/en.json","pt-BR":"i18n/pt-BR.json"},
          "assets":["ui/screens.js","ui/plugin.css","i18n/en.json","i18n/pt-BR.json"]}"#).unwrap();
        let parsed = PluginUiManifest::parse("enterprise", value).expect("valid");
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
        let parsed = PluginUiManifest::parse("enterprise", value.clone()).expect("valid slot");
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
            assert!(PluginUiManifest::parse("enterprise", bad).is_err());
        }
    }

    #[test]
    fn manifest_without_i18n_is_still_valid() {
        let mut value = manifest();
        value.as_object_mut().unwrap().remove("i18n");
        assert!(PluginUiManifest::parse("enterprise", value)
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
