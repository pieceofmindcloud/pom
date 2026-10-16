//! The networks the operator keeps at the top of the Networks screen: the one
//! this machine is in and the ones imported from a trust document.
//!
//! A hidden (private) network is listed nowhere, so the only way to reach it
//! is to be handed its trust document. Importing one stores the document on
//! this machine and shows the network beside the public ones; admission is
//! then asked through the lobby, by the principal's key, like for any other.
//! Nothing here joins a network or changes the trust root.

use super::*;
use crate::gateway::PublicNetworkView;

/// Imported networks kept at once.
const CUSTOM_NETWORKS_MAX: usize = 64;
/// A trust document is a few kilobytes; this bounds what is stored.
const TRUST_DOCUMENT_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Serialize)]
pub struct PinnedNetwork {
    pub network: PublicNetworkView,
    /// `local`, `private` or `public`, as the network declares it.
    pub visibility: crate::config::NetworkVisibility,
}

#[derive(Debug, Deserialize)]
pub struct ImportNetwork {
    pub trust_document: String,
}

/// What a trust document alone says about its network. Everything a
/// principal changes later (description, location, open roles) is unknown
/// here; the lobby is assumed open and the principal refuses when it is not.
pub(crate) fn network_from_trust_document(content: &str) -> anyhow::Result<PinnedNetwork> {
    let document = crate::admin_trust::parse_transported(content)?;
    let publisher_key =
        crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&document.primary.pubkey)
            .context("the trust document's principal key is not valid")?;
    let setup = document.setup.as_ref();
    let name = setup
        .map(|setup| setup.public_name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(&document.anchor.cluster_id)
        .to_string();
    Ok(PinnedNetwork {
        visibility: setup
            .map(|setup| setup.visibility)
            .unwrap_or(crate::config::NetworkVisibility::Private),
        network: PublicNetworkView {
            cluster_id: document.anchor.cluster_id.clone(),
            name,
            fingerprint: crate::admin_trust::anchor_fingerprint(&document),
            publisher_key,
            relay_hints: setup
                .map(|setup| setup.relay_hints.clone())
                .unwrap_or_default(),
            trust_document: serde_json::to_string_pretty(&document)?,
            organization: setup
                .map(|setup| setup.organization.clone())
                .unwrap_or_default(),
            description: String::new(),
            rules: String::new(),
            channels: String::new(),
            country_code: String::new(),
            country_name: String::new(),
            state: String::new(),
            city: String::new(),
            geo_label: String::new(),
            access_policy: setup.map(|setup| setup.access_policy).unwrap_or_default(),
            accepted_enrollments: Default::default(),
            public_stats: false,
            lobby_chat: false,
            location: None,
            discovered_via: "custom".to_string(),
            directory_replicas: setup
                .map(|setup| setup.directory_replicas.clone())
                .unwrap_or_default(),
        },
    })
}

/// The stored documents that still validate, one per network.
pub(super) fn custom_networks(database: &crate::db::PomDb) -> Vec<PinnedNetwork> {
    let mut networks: Vec<PinnedNetwork> = Vec::new();
    for document in database.custom_networks().unwrap_or_default() {
        match network_from_trust_document(&document) {
            Ok(entry) => {
                if !networks
                    .iter()
                    .any(|known| known.network.fingerprint == entry.network.fingerprint)
                {
                    networks.push(entry);
                }
            }
            Err(error) => {
                warn!("an imported network no longer validates and is not listed: {error:#}")
            }
        }
    }
    networks
}

fn refusal(status: StatusCode, reason: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": message.into(), "reason": reason })),
    )
        .into_response()
}

/// `GET /api/ui/networks/pinned` - this machine's network, whatever its
/// visibility, and the imported ones.
pub(super) async fn pinned_networks(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let visibility = state.auth.read().await.network.visibility;
    let own = crate::gateway::local_network_view(&state)
        .await
        .map(|network| PinnedNetwork {
            network,
            visibility,
        });
    let own_fingerprint = own.as_ref().map(|own| own.network.fingerprint.clone());
    let mut custom = custom_networks(&state.database);
    custom.retain(|entry| Some(&entry.network.fingerprint) != own_fingerprint.as_ref());
    // The screen asks how each row can be reached; that route only answers
    // for publishers this node lists.
    let listed: Vec<PublicNetworkView> = custom.iter().map(|entry| entry.network.clone()).collect();
    crate::gateway::remember_listed_publishers(&listed);
    Json(json!({ "own": own, "custom": custom })).into_response()
}

/// Why an import was refused: status, stable reason, sentence.
type Refused = (StatusCode, &'static str, String);

/// Validates `content` and stores it among the imported networks. A newer
/// copy of a known network replaces the stored one.
fn import_document(
    database: &crate::db::PomDb,
    own_fingerprint: Option<&str>,
    content: &str,
) -> Result<PinnedNetwork, Refused> {
    if content.len() > TRUST_DOCUMENT_MAX_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "the file is too large to be a trust document".into(),
        ));
    }
    let entry = network_from_trust_document(content).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            "invalid_document",
            format!("the file is not a valid trust document: {error:#}"),
        )
    })?;
    if own_fingerprint == Some(entry.network.fingerprint.as_str()) {
        return Err((
            StatusCode::CONFLICT,
            "own_network",
            "this is the network this machine is already in".into(),
        ));
    }
    let mut documents: Vec<String> = custom_networks(database)
        .into_iter()
        .filter(|known| known.network.fingerprint != entry.network.fingerprint)
        .map(|known| known.network.trust_document)
        .collect();
    if documents.len() >= CUSTOM_NETWORKS_MAX {
        return Err((
            StatusCode::CONFLICT,
            "too_many",
            format!("at most {CUSTOM_NETWORKS_MAX} imported networks are kept; remove one first"),
        ));
    }
    documents.push(entry.network.trust_document.clone());
    database.save_custom_networks(&documents).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            format!("could not save the imported network: {error:#}"),
        )
    })?;
    Ok(entry)
}

/// `POST /api/ui/networks/custom` - keeps a network from its trust document.
pub(super) async fn import_custom_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(request): Json<ImportNetwork>,
) -> Response {
    let own = state
        .admin_trust
        .is_anchored()
        .then(|| crate::admin_trust::anchor_fingerprint(&state.admin_trust.document()));
    match import_document(&state.database, own.as_deref(), &request.trust_document) {
        Ok(entry) => Json(entry).into_response(),
        Err((status, reason, message)) => refusal(status, reason, message),
    }
}

/// `DELETE /api/ui/networks/custom/:fingerprint` - forgets an imported network.
pub(super) async fn remove_custom_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    axum::extract::Path(fingerprint): axum::extract::Path<String>,
) -> Response {
    let known = custom_networks(&state.database);
    let documents: Vec<String> = known
        .iter()
        .filter(|entry| entry.network.fingerprint != fingerprint)
        .map(|entry| entry.network.trust_document.clone())
        .collect();
    if documents.len() == known.len() {
        return refusal(
            StatusCode::NOT_FOUND,
            "unknown_network",
            "no imported network has that fingerprint",
        );
    }
    if let Err(error) = state.database.save_custom_networks(&documents) {
        return refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            format!("could not remove the imported network: {error:#}"),
        );
    }
    Json(json!({ "removed": fingerprint })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::testkit;

    #[tokio::test]
    async fn an_imported_network_is_kept_once() {
        let other = testkit::state_with("other-network", vec![]).await;
        let here = testkit::state_with("this-machine", vec![]).await;
        let document = serde_json::to_string(&other.admin_trust.document()).unwrap();
        let fingerprint = crate::admin_trust::anchor_fingerprint(&other.admin_trust.document());

        let entry = import_document(&here.database, None, &document).unwrap();
        assert_eq!(entry.network.fingerprint, fingerprint);
        assert_eq!(entry.network.discovered_via, "custom");
        assert!(entry.network.publisher_key.starts_with("pk:"));
        // The same file again replaces the entry instead of repeating it.
        import_document(&here.database, None, &document).unwrap();
        let listed = custom_networks(&here.database);
        assert_eq!(listed.len(), 1);
        // What is stored validates like any listed network.
        assert!(crate::gateway::validated_listed_network(listed[0].network.clone()).is_some());

        here.database.save_custom_networks(&[]).unwrap();
        assert!(custom_networks(&here.database).is_empty());
    }

    #[tokio::test]
    async fn a_broken_file_and_the_machines_own_network_are_refused() {
        let here = testkit::state_with("this-machine", vec![]).await;
        let own = serde_json::to_string(&here.admin_trust.document()).unwrap();
        let fingerprint = crate::admin_trust::anchor_fingerprint(&here.admin_trust.document());

        let refused = import_document(&here.database, Some(&fingerprint), &own).unwrap_err();
        assert_eq!(
            (refused.0, refused.1),
            (StatusCode::CONFLICT, "own_network")
        );
        let refused = import_document(&here.database, None, "{\"anchor\": 1}").unwrap_err();
        assert_eq!(
            (refused.0, refused.1),
            (StatusCode::BAD_REQUEST, "invalid_document")
        );
        // A document edited after it was signed no longer validates.
        let tampered = own.replacen(
            &here.admin_trust.document().anchor.cluster_id,
            "another-name",
            1,
        );
        assert!(import_document(&here.database, None, &tampered).is_err());
        assert!(custom_networks(&here.database).is_empty());
    }
}
