//! The interface's side of the local CA (`web_ca.rs`, `trust_store.rs`):
//! whether this machine trusts it, its `.crt` to install by hand on any
//! machine, and the button that asks this machine's system to trust it.
//!
//! - `GET /api/ui/tls/ca` - fingerprint, trust state, whether the person
//!   refused it, and whether this request may install it.
//! - `GET /api/ui/tls/ca.crt` - the CA certificate (public, no key).
//! - `POST /api/ui/tls/ca/install` - only from this machine itself
//!   (`serve::PeerAddr` on loopback): the trust store is the one of the
//!   machine the node runs on, so a browser elsewhere would answer a dialog
//!   it cannot see.

use super::*;
use axum::Extension;

#[derive(Serialize)]
struct CaView {
    fingerprint: String,
    trust: crate::trust_store::TrustState,
    declined: bool,
    /// The node can ask this machine's system to trust the CA, and the
    /// request comes from this machine.
    can_install: bool,
    filename: String,
}

fn is_loopback(peer: Option<&Extension<crate::serve::PeerAddr>>) -> bool {
    peer.is_some_and(|Extension(peer)| crate::auth::is_loopback_addr(peer.0))
}

/// Name of the downloaded file: the product, the host and the start of the
/// fingerprint, so CAs of different machines never overwrite one another.
fn download_name(ca: &crate::web_ca::LocalCa) -> String {
    let host = crate::web_ca::host_names()
        .into_iter()
        .find(|name| name != "localhost")
        .unwrap_or_default();
    let short: String = ca
        .fingerprint_sha256()
        .replace(':', "")
        .chars()
        .take(8)
        .collect();
    let mut name = format!("{}-{host}-ca-{}.crt", crate::brand::slug(), short.to_ascii_lowercase());
    name.retain(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    name.replace("--", "-")
}

fn unavailable(error: anyhow::Error) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": format!("local CA unavailable: {error:#}") })),
    )
        .into_response()
}

pub(super) async fn ca_status(peer: Option<Extension<crate::serve::PeerAddr>>) -> Response {
    let loopback = is_loopback(peer.as_ref());
    let view = tokio::task::spawn_blocking(move || -> anyhow::Result<CaView> {
        let ca = crate::web_ca::LocalCa::load_or_create()?;
        Ok(CaView {
            fingerprint: ca.fingerprint_sha256(),
            trust: crate::trust_store::state(&ca),
            declined: crate::trust_store::declined(&ca),
            can_install: loopback && crate::trust_store::supported(),
            filename: download_name(&ca),
        })
    })
    .await;
    match view {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(error)) => unavailable(error),
        Err(error) => unavailable(error.into()),
    }
}

pub(super) async fn ca_certificate() -> Response {
    match tokio::task::spawn_blocking(crate::web_ca::LocalCa::load_or_create).await {
        Ok(Ok(ca)) => (
            [
                (header::CONTENT_TYPE, "application/x-x509-ca-cert".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", download_name(&ca)),
                ),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            ca.cert_pem().to_string(),
        )
            .into_response(),
        Ok(Err(error)) => unavailable(error),
        Err(error) => unavailable(error.into()),
    }
}

pub(super) async fn ca_install(peer: Option<Extension<crate::serve::PeerAddr>>) -> Response {
    if !is_loopback(peer.as_ref()) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "the CA is installed from this machine itself; elsewhere, download the .crt",
                "reason": "not_loopback",
            })),
        )
            .into_response();
    }
    let report = tokio::task::spawn_blocking(|| -> anyhow::Result<crate::trust_store::InstallReport> {
        let ca = crate::web_ca::LocalCa::load_or_create()?;
        Ok(crate::trust_store::install(&ca))
    })
    .await;
    match report {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(error)) => unavailable(error),
        Err(error) => unavailable(error.into()),
    }
}
