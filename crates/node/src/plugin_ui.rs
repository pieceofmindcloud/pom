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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct RouteSpec {
    pub path: String,
    pub screen: String,
    pub profiles: Vec<String>,
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
    #[serde(default)]
    pub menu: Vec<MenuItem>,
    #[serde(default)]
    pub routes: Vec<RouteSpec>,
    pub screens: std::collections::BTreeMap<String, ScreenSpec>,
    /// Catálogos de texto do plugin: idioma -> asset JSON plano
    /// (`{"<plugin_code>.chave": "texto ICU"}`). O plugin é dono dos próprios
    /// textos; o POM só carrega e resolve. Se declarado, `en` é obrigatório.
    #[serde(default)]
    pub i18n: std::collections::BTreeMap<String, String>,
    /// Lista fechada do que `/assets/*` pode servir.
    pub assets: Vec<String>,
}

const MAX_CATALOG_KEYS: usize = 5000;
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
        let manifest: Self =
            serde_json::from_value(value).map_err(|e| format!("manifesto ilegível: {e}"))?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(format!("schema {:?} não suportado", manifest.schema));
        }
        if manifest.plugin_code != expected_code {
            return Err("plugin_code do manifesto não é o do plugin instalado".into());
        }
        for item in &manifest.menu {
            let valid_role_fields = item.roles.as_deref().is_none_or(valid_roles)
                && item.profiles.as_deref().is_none_or(valid_profiles);
            if item.id.is_empty() || !valid_path(&item.to) || !valid_role_fields {
                return Err(format!("item de menu {:?} inválido", item.id));
            }
            if !item.label.contains_key("en") {
                return Err(format!("item de menu {:?} sem rótulo en", item.id));
            }
        }
        for route in &manifest.routes {
            if !valid_path(&route.path)
                || !valid_profiles(&route.profiles)
                || !manifest.screens.contains_key(&route.screen)
            {
                return Err(format!("rota {:?} inválida", route.path));
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
                    | "application/json"
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
        assert!(!parsed.serves("ui/other.js"));
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
