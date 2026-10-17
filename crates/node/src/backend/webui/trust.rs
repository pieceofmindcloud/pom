//! The two sides of anchoring admin trust, as seen by the interface.
//!
//! **Where this lives and why.** The route is `CommonOrInternal`, outside the
//! `AdminOnly` scope, because before anchoring a node with `profile: admin` has **no**
//! effective admin capability: `require_admin_for_surface` refuses `AdminOnly`
//! in exactly that state. Putting the screen behind the administrator profile
//! would make it unreachable at the only moment when it is of any use.
//! For the same reason the node that receives the anchor may be `node` or `infer`.
//!
//! **What is safe to show.** The fingerprint is public: it is the value the
//! operator carries with them to check on another machine, and knowing it does not
//! grant anything to anyone. No private key passes through here.
//!
//! **What locks the installation.** Three refusals, all on the server:
//!
//! 1. only through the loopback, measured on the socket's real address
//!    (`serve::PeerAddr`). It is not `admin_exposure`, which may be open to the
//!    network, and it is not a proxy header: `X-Forwarded-For` proves nothing and
//!    does not enter this decision;
//! 2. only while the node does not yet have an anchor - replacing the root is manual
//!    re-anchoring on the machine (`pom admin-anchor --replace`), never an
//!    interface gesture;
//! 3. only with the fingerprint typed by the operator matching the anchored key
//!    of the pasted chain. The file does not validate itself.
//!
//! The chain validation and the fingerprint check are those of
//! `admin_trust`, the same ones `pom admin-anchor` uses. If the command's path
//! and the interface's diverge, one of them is wrong.
//!
//! **Why the connection key is not required here.** The neighbouring authority
//! routes require it; these do not, on purpose. The boundary remains the
//! same as for the whole administrative surface (`require_admin_for_surface` ->
//! `AdminAuth`: loopback in local mode, human password in network mode). Adding
//! that credential does not protect against the threat the loopback lock
//! exists to contain - whoever reaches the browser on the loopback reaches the
//! `localStorage` of the same origin, where it lives - and it costs exactly what
//! this screen cannot lose: a browser that has not yet received it would see
//! "missing Authorization header" in place of the fingerprint, and the operator
//! would be dependent on the terminal again.

use super::*;
use axum::Extension;

/// The anchoring state of this machine, as the interface draws it.
#[derive(Debug, Serialize)]
pub struct TrustAnchorView {
    /// Whether this node already has a valid trust root.
    pub anchored: bool,
    /// SHA-256 fingerprint of the anchored key. `None` when not yet anchored.
    pub fingerprint: Option<String>,
    pub cluster_id: Option<String>,
    /// The machine whose key is the root, so the operator can recognize it.
    pub anchor_node_id: Option<String>,
    /// Whether the root is **this** node's key, and not one it received.
    pub is_anchor: bool,
    /// Whether this request arrived through the loopback. Real socket address.
    pub loopback: bool,
    /// Whether installing the anchor from here is possible right now.
    pub can_install: bool,
    /// File the operator copies from the admin to here.
    pub trust_filename: String,
}

#[derive(Debug, Deserialize)]
pub struct InstallAnchorRequest {
    /// Pasted content of the trust file coming from the admin.
    pub document: String,
    /// Fingerprint the operator brought through their own channel and typed.
    /// It is never pre-filled from `document`: it is what makes the
    /// check worth anything.
    pub fingerprint: String,
}

/// Materials an administrator can hand to the operator of another node.
/// The trust document is public chain data; the connection key is returned
/// only for a network that still uses the private shared secret.
#[derive(Debug, Serialize)]
pub struct TrustShareView {
    pub filename: String,
    pub document: String,
    pub connection_key: Option<String>,
    pub visibility: crate::config::NetworkVisibility,
}

#[derive(Debug)]
pub(crate) enum AnchorValidationError {
    InvalidDocument(anyhow::Error),
    FingerprintMismatch,
}

/// Shared validation path for the configured gateway and the loopback-only
/// setup router. The HTTP handlers own their transport policy; this function
/// owns the document and fingerprint rules.
pub(crate) fn validate_transported_anchor(
    body: &InstallAnchorRequest,
) -> Result<(crate::admin_trust::AdminTrustDocument, String), AnchorValidationError> {
    let document = crate::admin_trust::parse_transported(&body.document)
        .map_err(AnchorValidationError::InvalidDocument)?;
    let expected = crate::admin_trust::anchor_fingerprint(&document);
    if !crate::admin_trust::fingerprint_matches(&body.fingerprint, &expected) {
        return Err(AnchorValidationError::FingerprintMismatch);
    }
    Ok((document, expected))
}

/// Shared durable adoption path for the configured gateway and setup router.
pub(crate) fn adopt_transported_anchor(
    trust: &crate::admin_trust::AdminTrust,
    body: &InstallAnchorRequest,
) -> anyhow::Result<String> {
    let (document, expected) = match validate_transported_anchor(body) {
        Ok(value) => value,
        Err(AnchorValidationError::InvalidDocument(error)) => return Err(error),
        Err(AnchorValidationError::FingerprintMismatch) => {
            anyhow::bail!("the checked fingerprint does not match the anchor of this chain")
        }
    };
    trust.adopt_anchor(document, &body.fingerprint)?;
    Ok(expected)
}

/// Refuses with a stable code next to the sentence, so the interface translates the
/// message instead of showing the raw server text.
fn refusal(status: StatusCode, reason: &'static str, message: &str) -> Response {
    (status, Json(json!({ "error": message, "reason": reason }))).into_response()
}

fn is_loopback(peer: Option<&crate::serve::PeerAddr>) -> bool {
    // Without a socket address there is no proof of origin: fail closed.
    peer.is_some_and(|peer| crate::auth::is_loopback_addr(peer.0))
}

/// Only relay URLs with a DNS name go into the document: `relay_peers` also
/// accepts a relay on a bare IP, and a document never carries an IP.
pub(crate) fn relay_hint_keys(relay_peers: &[String]) -> Vec<String> {
    let mut keys = Vec::new();
    for entry in relay_peers {
        if let Some(hint) = crate::p2p::relay_hint(entry) {
            if !keys.contains(&hint) {
                keys.push(hint);
            }
        }
    }
    keys
}

pub(crate) async fn setup_metadata_from_local_configuration(
    state: &GatewayState,
) -> anyhow::Result<Option<crate::admin_trust::TrustNetworkMetadata>> {
    let (network, relay_hints) = {
        let config = state.auth.read().await;
        (config.network.clone(), relay_hint_keys(&config.relay_peers))
    };
    if network.public_name.trim().is_empty() {
        return Ok(None);
    }
    let directory_replicas = state
        .database
        .public_directory_settings()?
        .map(|settings| settings.replicas)
        .unwrap_or_default();
    Ok(Some(crate::admin_trust::TrustNetworkMetadata {
        public_name: network.public_name.trim().to_string(),
        organization: network.organization.trim().to_string(),
        visibility: network.visibility,
        access_policy: network.access_policy,
        relay_hints,
        directory_replicas,
    }))
}

pub(super) async fn trust_anchor(
    peer: Option<Extension<crate::serve::PeerAddr>>,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    let loopback = is_loopback(peer.as_ref().map(|Extension(peer)| peer));
    let anchored = state.admin_trust.is_anchored();
    let document = state.admin_trust.document();
    let (fingerprint, cluster_id, anchor_node_id, is_anchor) = if anchored {
        (
            Some(crate::admin_trust::anchor_fingerprint(&document)),
            Some(document.anchor.cluster_id.clone()),
            Some(document.anchor.principal.node_id.clone()),
            document.anchor.principal == state.authority_principal,
        )
    } else {
        (None, None, None, false)
    };
    Json(TrustAnchorView {
        anchored,
        fingerprint,
        cluster_id,
        anchor_node_id,
        is_anchor,
        loopback,
        can_install: !anchored && !state.admin_trust.anchor_file_exists() && loopback,
        trust_filename: crate::config::admin_trust_filename(),
    })
}

pub(super) async fn install_trust_anchor(
    peer: Option<Extension<crate::serve::PeerAddr>>,
    State(state): State<GatewayState>,
    Json(body): Json<InstallAnchorRequest>,
) -> Response {
    if !is_loopback(peer.as_ref().map(|Extension(peer)| peer)) {
        // Whoever reaches the browser would choose who administers the network. The
        // configured administrative exposure does not serve as a lock here: it
        // may be open to the network on purpose.
        warn!("refused the installation of an administrative anchor coming from outside loopback");
        return refusal(
            StatusCode::FORBIDDEN,
            "not_loopback",
            "the administrative anchor is only installed from this machine",
        );
    }
    if state.admin_trust.is_anchored() || state.admin_trust.anchor_file_exists() {
        return refusal(
            StatusCode::CONFLICT,
            "already_anchored",
            "this node already has a trust root; changing the root is a manual re-anchoring on the machine",
        );
    }
    let (document, expected) = match validate_transported_anchor(&body) {
        Ok(value) => value,
        Err(AnchorValidationError::InvalidDocument(error)) => {
            return refusal(
                StatusCode::BAD_REQUEST,
                "invalid_document",
                &format!("{error:#}"),
            );
        }
        Err(AnchorValidationError::FingerprintMismatch) => {
            return refusal(
                StatusCode::BAD_REQUEST,
                "fingerprint_mismatch",
                "the checked fingerprint does not match the anchor of this chain",
            );
        }
    };
    // The refusals above exist to give the operator the right sentence; the one that cannot
    // be bypassed is this one, which redoes the three under the same lock before
    // writing. Getting here with an error is a race or the disk, not a browser.
    match state.admin_trust.adopt_anchor(document, &body.fingerprint) {
        Ok(()) => {
            state
                .network
                .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
            info!(
                "administrative anchor installed by the operator; checked fingerprint {expected}"
            );
            Json(json!({ "anchored": true, "fingerprint": expected })).into_response()
        }
        Err(error) => refusal(
            StatusCode::CONFLICT,
            "install_failed",
            &format!("{error:#}"),
        ),
    }
}

pub(crate) async fn trust_share(State(state): State<GatewayState>) -> Response {
    if !super::local_is_primary(&state).await {
        return refusal(
            StatusCode::FORBIDDEN,
            "admin_authority_required",
            "only an authorized administrator can share the trust",
        );
    }

    let (visibility, connection_key) = {
        let config = state.auth.read().await;
        (
            config.network.visibility,
            (config.network.visibility != crate::config::NetworkVisibility::Public)
                .then(|| config.connection_key.clone())
                .filter(|key| !key.is_empty()),
        )
    };
    let document = if state.admin_trust.document().setup.is_none() {
        let setup = match setup_metadata_from_local_configuration(&state).await {
            Ok(Some(setup)) => setup,
            Ok(None) => {
                return refusal(
                    StatusCode::CONFLICT,
                    "trust_setup_metadata_unavailable",
                    "could not recover the initial network data for the trust root",
                )
            }
            Err(error) => {
                warn!("could not read the network configuration for the trust root: {error:#}");
                return refusal(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "trust_setup_metadata_unavailable",
                    "could not read the initial network data for the trust root",
                );
            }
        };
        match state.admin_trust.ensure_setup_metadata(
            setup,
            &state.authority_signing_key,
            &state.authority_principal,
        ) {
            Ok(document) => document,
            Err(error) => {
                warn!("could not complete the trust root metadata: {error:#}");
                return refusal(
                    StatusCode::CONFLICT,
                    "trust_setup_metadata_unavailable",
                    "could not complete the initial network data in the trust root",
                );
            }
        }
    } else {
        state.admin_trust.document()
    };
    let document = match serde_json::to_string_pretty(&document) {
        Ok(document) => document,
        Err(error) => {
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "trust_document_unavailable",
                &format!("could not serialize the trust root: {error}"),
            )
        }
    };
    Json(TrustShareView {
        filename: crate::config::admin_trust_filename(),
        document,
        connection_key,
        visibility,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    /// The real router, with the profile middleware the gateway applies. The
    /// tests go through it on purpose: the lock has to hold on the whole
    /// HTTP path, not only inside the handler.
    fn app(state: GatewayState) -> Router {
        Router::new()
            .merge(crate::webui::routes())
            .with_state(state.clone())
            .layer(axum::middleware::from_fn_with_state(
                state,
                crate::gateway::require_admin_for_surface,
            ))
    }

    /// A signed chain like the one the operator copies from the admin, with the
    /// fingerprint they carry with them.
    fn admin_chain() -> (String, String) {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            "ksia",
            "peer-ksia",
            &signing_key,
        );
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "cluster-do-operador",
            principal,
            &signing_key,
        )
        .expect("test root chain");
        let document = trust.document();
        let fingerprint = crate::admin_trust::anchor_fingerprint(&document);
        (
            serde_json::to_string(&document).expect("chain serializes"),
            fingerprint,
        )
    }

    async fn to_anchor(node_id: &str) -> GatewayState {
        let mut state = state_with(node_id, vec![]).await;
        state.admin_trust = Arc::new(crate::admin_trust::AdminTrust::empty());
        state
    }

    #[tokio::test]
    async fn admin_can_download_trust_and_private_connection_secret() {
        let state = state_with("ksia", vec![]).await;
        {
            let mut config = state.auth.write().await;
            config.profile = crate::profile::NodeProfile::Admin;
            config.network.public_name = "Rede local".into();
            config.advertise_addr = Some("[2001:db8::10]:42777".into());
        }
        let mut request = HttpRequest::get("/api/ui/network/trust-share")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let response = app(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert!(body["filename"].as_str().unwrap().ends_with("trust.json"));
        assert!(body["document"].as_str().unwrap().contains("anchor"));
        assert_eq!(body["connection_key"], "chave");
    }

    #[tokio::test]
    async fn admin_download_hydrates_missing_setup_metadata_from_local_configuration() {
        let state = state_with("ksia", vec![]).await;
        {
            let mut config = state.auth.write().await;
            config.profile = crate::profile::NodeProfile::Admin;
            config.network = crate::config::NetworkSettings {
                public_name: "Rede do wizard".into(),
                organization: "POM".into(),
                country_code: "BR".into(),
                country_name: "Brasil".into(),
                city: "Curitiba".into(),
                geo_label: "BR-Curitiba".into(),
                visibility: crate::config::NetworkVisibility::Private,
                access_policy: crate::config::NetworkAccessPolicy::AnyoneWithAnchor,
                ..Default::default()
            };
            // A local pin and bootstrap do not go into the document: it only
            // carries keys and host names. A relay on a bare IP in
            // `relay_peers` stays out too.
            config.advertise_addr = Some("[2001:db8::10]:43123".into());
            config.bootstrap_nodes = vec!["[2001:db8::10]:43123".into()];
            config.relay_peers = vec![
                "https://relay.example.org".into(),
                "http://[2001:db8::20]:52777".into(),
            ];
        }
        state
            .database
            .save_public_directory_settings(&crate::public_directory::PublicDirectorySettings {
                replicas: vec!["https://directory.example".into()],
            })
            .unwrap();

        let mut request = HttpRequest::get("/api/ui/network/trust-share")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let response = app(state.clone()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        let document: crate::admin_trust::AdminTrustDocument =
            serde_json::from_str(body["document"].as_str().unwrap()).unwrap();
        let setup = document
            .setup
            .expect("download must carry the network setup");
        assert_eq!(setup.public_name, "Rede do wizard");
        assert_eq!(setup.organization, "POM");
        assert_eq!(
            setup.relay_hints,
            vec!["https://relay.example.org/".to_string()]
        );
        assert!(
            !body["document"].as_str().unwrap().contains("2001:db8"),
            "the trust document must not carry an IP"
        );
        assert_eq!(setup.directory_replicas, vec!["https://directory.example"]);
        assert_eq!(state.admin_trust.document().setup, Some(setup));
    }

    fn install(peer: &str, document: &str, fingerprint: &str) -> HttpRequest<Body> {
        let mut request = HttpRequest::post("/api/ui/network/trust-anchor")
            .header("authorization", "Bearer chave")
            .header("content-type", "application/json")
            // A proxy header is written by the client. It travels in all
            // these tests precisely to prove that it weighs nothing.
            .header("x-forwarded-for", "127.0.0.1")
            .header("x-real-ip", "127.0.0.1")
            .body(Body::from(
                serde_json::json!({ "document": document, "fingerprint": fingerprint }).to_string(),
            ))
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr(peer.parse().unwrap()));
        request
    }

    /// The lock is the socket's real address, not `admin_exposure`: here the
    /// administrative surface is deliberately open to the network, the
    /// requester brings the correct human password - and the installation is still
    /// refused. Using the configured exposure as the lock would be a hole.
    #[tokio::test]
    async fn the_anchor_is_not_installed_from_outside_loopback() {
        let state = to_anchor("salem").await;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token =
                Some(crate::auth::hash_admin_password("token-do-operador").unwrap());
        }
        let (document, fingerprint) = admin_chain();

        let mut request = install("192.0.2.7:4321", &document, &fingerprint);
        request.headers_mut().insert(
            crate::auth::ADMIN_ACCESS_TOKEN_HEADER,
            "token-do-operador".parse().unwrap(),
        );
        let response = app(state.clone()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_of(response).await["reason"], "not_loopback");
        assert!(
            !state.admin_trust.is_anchored(),
            "a refusal cannot leave authority installed"
        );
    }

    /// Replacing the root is not an interface gesture. An already anchored node refuses and says
    /// that re-anchoring is manual, on the machine.
    #[tokio::test]
    async fn an_already_anchored_node_refuses_the_installation() {
        // `state_with` is born already anchored on its own key.
        let state = state_with("ksia", vec![]).await;
        let original_anchor = state.admin_trust.document().anchor;
        let (document, fingerprint) = admin_chain();

        let response = app(state.clone())
            .oneshot(install("127.0.0.1:4321", &document, &fingerprint))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(response).await["reason"], "already_anchored");
        assert_eq!(
            state.admin_trust.document().anchor,
            original_anchor,
            "the current anchor cannot be replaced from the interface"
        );
    }

    /// The file does not validate itself: what validates it is the value the operator
    /// brought through their own channel. Getting that value wrong is a refusal.
    #[tokio::test]
    async fn the_wrong_fingerprint_is_refused() {
        let state = to_anchor("salem").await;
        let (document, fingerprint) = admin_chain();
        let wrong = format!("{}0", &fingerprint[1..]);
        assert_ne!(wrong, fingerprint);

        let response = app(state.clone())
            .oneshot(install("127.0.0.1:4321", &document, &wrong))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_of(response).await["reason"], "fingerprint_mismatch");
        assert!(
            !state.admin_trust.is_anchored(),
            "a wrong fingerprint cannot fabricate authority"
        );
    }

    /// The happy path on the receiving side: pasting the file and typing the right
    /// fingerprint anchors the node.
    #[tokio::test]
    async fn the_right_fingerprint_installs_the_anchor() {
        let state = to_anchor("salem").await;
        let (document, fingerprint) = admin_chain();

        // Spaces and case are human transcription, not another value.
        let typed = format!("  {}  ", fingerprint.to_uppercase());
        let response = app(state.clone())
            .oneshot(install("127.0.0.1:4321", &document, &typed))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await["fingerprint"], fingerprint);
        assert!(state.admin_trust.is_anchored());
        assert_eq!(
            state.admin_trust.document().anchor.cluster_id,
            "cluster-do-operador"
        );
    }

    /// This task's fold: an unanchored `profile: admin` has no
    /// effective admin capability. The fingerprint screen has to stay
    /// outside the `AdminOnly` scope, otherwise the operator never reaches it - and it is
    /// exactly there that they need it.
    #[tokio::test]
    async fn an_admin_yet_to_be_anchored_reaches_the_fingerprint_screen() {
        let state = to_anchor("ksia").await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;

        let mut refused = HttpRequest::get("/api/ui/requests")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        refused
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        assert_eq!(
            app(state.clone()).oneshot(refused).await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "without an anchor, the admin profile does not open the administrative surface"
        );

        let mut reached = HttpRequest::get("/api/ui/network/trust-anchor")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        reached
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let response = app(state).oneshot(reached).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let view = body_of(response).await;
        assert_eq!(view["anchored"], false);
        assert_eq!(view["can_install"], true);
        assert!(view["fingerprint"].is_null());
    }

    /// The other half of the fold: the screen also cannot depend on the cluster's
    /// connection key. A browser that has not yet received it has to see the
    /// fingerprint, otherwise the operator goes back to the terminal - which is
    /// exactly what this task exists to avoid.
    #[tokio::test]
    async fn the_screen_does_not_depend_on_the_cluster_connection_key() {
        let state = state_with("ksia", vec![]).await;

        let mut without_credential = HttpRequest::get("/api/ui/network/trust-anchor")
            .body(Body::empty())
            .unwrap();
        without_credential
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let response = app(state).oneshot(without_credential).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await["anchored"], true);
    }

    /// The ksia side: an anchored node shows the fingerprint the
    /// operator carries with them, and says the root is its own.
    #[tokio::test]
    async fn an_anchored_node_shows_the_anchor_fingerprint() {
        let state = state_with("ksia", vec![]).await;
        let expected = crate::admin_trust::anchor_fingerprint(&state.admin_trust.document());

        let mut request = HttpRequest::get("/api/ui/network/trust-anchor")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let view = body_of(app(state).oneshot(request).await.unwrap()).await;

        assert_eq!(view["anchored"], true);
        assert_eq!(view["fingerprint"], expected);
        assert_eq!(view["cluster_id"], "test-cluster");
        assert_eq!(view["is_anchor"], true);
        assert_eq!(view["can_install"], false);
    }
}
