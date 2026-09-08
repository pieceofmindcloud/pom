//! The product's identity, in one place.
//!
//! `brand.json` at the repository root is the single source of truth for the
//! product name, the tagline and the logo files. It is embedded at compile time
//! (`include_str!`), so the binary carries the identity and there is no file to
//! ship next to it and no way for a node to start up nameless.
//!
//! Everything that used to hardcode the old project name derives from `slug`
//! now: the config file (`<slug>.json`), the identity files
//! (`.<slug>-node.key`, `.<slug>.key`), the environment variables
//! (`<SLUG>_HOME`, `<SLUG>_CONFIG`, ...), the GGUF metadata prefix (`<slug>.`)
//! and the pkarr TXT record (`_<slug>`). Renaming the product is editing
//! `brand.json` - and, for the executable itself, the `[[bin]]` name in
//! `crates/node/Cargo.toml`, which Cargo needs as a literal.
//!
//! The frontend reads the same file (`apps/frontend/scripts/build-brand.mjs`
//! copies it into the bundle), so the screen and the binary can never disagree
//! about what the product is called.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;

const BRAND_JSON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../brand.json"));

/// Os campos de imagem e a descrição não são lidos pelo binário: quem os usa é
/// a interface, que recebe **este mesmo arquivo** por
/// `apps/frontend/scripts/build-brand.mjs`. Eles ficam aqui porque o struct é o
/// contrato do arquivo - deixá-los de fora faria um `brand.json` com campo
/// errado passar despercebido no lado Rust.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Brand {
    /// Display name, as written on screen ("POM").
    pub name: String,
    /// The line under the name ("Piece Of Mind").
    pub tagline: String,
    /// One sentence about what the product is.
    #[serde(default)]
    pub description: String,
    /// Lowercase identifier used for files, env vars and wire names ("pom").
    pub slug: String,
    /// Lockup completo (marca + letreiro), por tema: `light` e `dark`.
    #[serde(default)]
    pub logo: BTreeMap<String, String>,
    /// Repository-relative path of the mark alone (no wordmark).
    #[serde(default)]
    pub mark: String,
    /// The mark on its rounded plate - what the UI and the tab icon show.
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub favicon: String,
    #[serde(default)]
    pub icons: BTreeMap<String, String>,
    /// Bandeira por locale, usada pelo seletor de idioma da interface.
    #[serde(default)]
    pub flags: BTreeMap<String, String>,
    /// Ícones de aplicativo por plataforma (macOS/Windows/Linux).
    #[serde(default)]
    pub app_icons: BTreeMap<String, String>,
}

/// The parsed brand. Panics on a malformed `brand.json`, which is a build-time
/// mistake in the repository, not a runtime condition a node can recover from.
pub fn brand() -> &'static Brand {
    static BRAND: OnceLock<Brand> = OnceLock::new();
    BRAND.get_or_init(|| {
        serde_json::from_str(BRAND_JSON).expect("brand.json na raiz do repositório está malformado")
    })
}

/// Display name ("POM").
pub fn name() -> &'static str {
    &brand().name
}

/// Tagline ("Piece Of Mind").
pub fn tagline() -> &'static str {
    &brand().tagline
}

/// Lowercase identifier ("pom") - the root of every derived name below.
pub fn slug() -> &'static str {
    &brand().slug
}

/// Name of the config file: `pom.json`.
pub fn config_filename() -> String {
    format!("{}.json", slug())
}

/// Environment variable for this product: `env_var("HOME")` -> `POM_HOME`.
///
/// Derived rather than written out so that renaming the product does not leave
/// half the variables behind - the failure mode would be a node that reads its
/// home from one name and its config from another.
pub fn env_var(suffix: &str) -> String {
    format!("{}_{}", slug().to_uppercase(), suffix)
}

/// Reads `env_var(suffix)` from the environment.
pub fn env(suffix: &str) -> Result<String, std::env::VarError> {
    std::env::var(env_var(suffix))
}

/// Prefix of this product's own GGUF metadata keys: `pom.` in
/// `pom.original_block_count`.
pub fn gguf_key(suffix: &str) -> String {
    format!("{}.{}", slug(), suffix)
}

/// The pkarr TXT record name: `_pom`.
pub fn dns_record() -> String {
    format!("_{}", slug())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the file is that one edit renames everything; a derived
    /// name that stopped deriving would be found only in production, by a node
    /// that cannot find its own config.
    #[test]
    fn every_name_derives_from_the_slug() {
        let s = slug();
        assert_eq!(config_filename(), format!("{s}.json"));
        assert_eq!(env_var("HOME"), format!("{}_HOME", s.to_uppercase()));
        assert_eq!(gguf_key("start_layer"), format!("{s}.start_layer"));
        assert_eq!(dns_record(), format!("_{s}"));
    }

    #[test]
    fn the_brand_file_is_parseable_and_named() {
        assert!(!name().is_empty());
        assert!(!tagline().is_empty());
        assert!(slug().chars().all(|c| c.is_ascii_lowercase() || c == '-'));
    }
}
