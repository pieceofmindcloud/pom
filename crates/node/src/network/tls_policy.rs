//! Who decides whether the cluster speaks TLS, and which path that decision travels.
//!
//! The decision belongs to the principal node. The path is **not** the gossip: the
//! discovery UDP is cleartext and unauthenticated, so an announce forged by
//! anyone on the network would downgrade the whole cluster to cleartext - the exact
//! attack TLS exists to prevent.
//!
//! So the `tls` field of the Announce is **a trigger, never a value**: disagreeing
//! with it only fires an authenticated fetch on the principal, and only the answer to
//! that fetch is written to `pom.json`. An intruder can fire the fetch at will; it
//! cannot answer for it, because it does not hold the connection key.
//!
//! And a fetch that fails **preserves the local value**: a principal that is down or
//! a partitioned network never downgrades the cluster. Fail-closed.

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::peer_client::PeerClients;

/// Path the principal serves and the ratchet queries.
pub const TLS_POLICY_PATH: &str = "/api/cluster/tls";

#[derive(Debug, PartialEq)]
pub enum PolicyOutcome {
    /// Nothing to do.
    Unchanged,
    /// Fetch the truth from the principal.
    Fetch { principal: String },
    /// Value confirmed by an authenticated fetch, ready to be written.
    Adopt(bool),
}

/// What to do on seeing an Announce.
///
/// `local` is what this node has stored; `announced`, what the announce says;
/// `from_principal`, whether the announcer presents itself as the principal.
pub fn on_announce(
    local: bool,
    announced: bool,
    from_principal: bool,
    principal_id: &str,
) -> PolicyOutcome {
    if !from_principal || announced == local {
        return PolicyOutcome::Unchanged;
    }
    PolicyOutcome::Fetch {
        principal: principal_id.to_string(),
    }
}

#[derive(Deserialize)]
struct TlsReply {
    tls_required: bool,
}

/// Asks the principal, over an authenticated channel, whether the cluster speaks TLS.
///
/// An error here is `Err`, and the caller **keeps the local value** - see the paragraph
/// on fail-closed at the top of the module.
pub(crate) async fn fetch_from_principal_candidates(
    clients: &PeerClients,
    principal_id: &str,
    base_urls: &[String],
    connection_key: &str,
    signing_key: &ed25519_dalek::SigningKey,
    authority_principal: &crate::admin_trust::AuthorityPrincipal,
    trust: &crate::admin_trust::AdminTrust,
    tunnel: Option<&crate::peer_client::TunnelPeer>,
) -> Result<bool> {
    let headers = crate::gateway::GatewayState::signed_control_headers_for(
        signing_key,
        authority_principal.clone(),
        trust,
        "GET",
        TLS_POLICY_PATH,
        &[],
    )?;
    let resp = clients
        .get_internal_from_candidates(
            principal_id,
            base_urls,
            TLS_POLICY_PATH,
            connection_key,
            &headers,
            tunnel,
        )
        .await
        .context("could not reach the principal")?;
    let body: TlsReply = resp.json().context("principal response did not parse")?;
    info!(
        "TLS policy confirmed by the principal: {}",
        body.tls_required
    );
    Ok(body.tls_required)
}

/// Applies the result of the fetch, preserving the local value when it fails.
pub fn adopt_or_keep(local: bool, fetched: Result<bool>) -> PolicyOutcome {
    match fetched {
        Ok(v) if v != local => PolicyOutcome::Adopt(v),
        Ok(_) => PolicyOutcome::Unchanged,
        Err(e) => {
            warn!("TLS policy fetch failed, keeping the local value ({local}): {e}");
            PolicyOutcome::Unchanged
        }
    }
}

/// Spawns the task that serves the ratchet.
///
/// Receives `(principal's node_id, the tls it announced)`, fetches the policy from it
/// over an authenticated channel, and only then writes it. The announced boolean is
/// used **only** to know which scheme to dial the probe with - see the note in
/// `handle_message` of `network.rs`.
pub(crate) fn spawn_ratchet(
    net: std::sync::Arc<crate::network::Network>,
    clients: std::sync::Arc<PeerClients>,
    auth: std::sync::Arc<tokio::sync::RwLock<crate::config::Config>>,
    tls_server: Option<crate::serve::TlsServer>,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    authority_principal: crate::admin_trust::AuthorityPrincipal,
    trust: std::sync::Arc<crate::admin_trust::AdminTrust>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    net.set_tls_probe(tx);
    tokio::spawn(async move {
        while let Some((principal, announced)) = rx.recv().await {
            let Some(peer) = net
                .alive_peers()
                .await
                .into_iter()
                .find(|p| p.node_id == principal)
            else {
                continue;
            };
            let candidates =
                net.order_gateway_bases(&peer.node_id, peer.gateway_candidates_with_tls(announced));
            if candidates.is_empty() {
                warn!("principal {principal} does not announce a gateway port; TLS policy could not be confirmed");
                continue;
            }

            let (local, key) = {
                let cfg = auth.read().await;
                (
                    cfg.tls_required,
                    crate::auth::internal_node_token(&cfg).unwrap_or_default(),
                )
            };
            // The probe has to speak the scheme the URL says, and the local cache
            // is still on the old policy. A client of its own avoids pushing
            // the new policy into the cache before it is confirmed.
            let probe = PeerClients::new(std::time::Duration::from_secs(10));
            probe.set_tls(announced);
            let searched = fetch_from_principal_candidates(
                &probe,
                &principal,
                &candidates,
                &key,
                &signing_key,
                &authority_principal,
                &trust,
                crate::peer_client::TunnelPeer::from_peer(&peer).as_ref(),
            )
            .await;

            if let PolicyOutcome::Adopt(value) = adopt_or_keep(local, searched) {
                {
                    let mut cfg = auth.write().await;
                    cfg.tls_required = value;
                    if let Err(e) = cfg.save() {
                        warn!("TLS policy confirmed but not written: {e}");
                    }
                }
                clients.apply_cluster_policy(value);
                net.set_tls(value);
                if let Some(t) = &tls_server {
                    t.set_strict(value);
                }
                info!(
                    "cluster TLS policy adopted from the principal: {}",
                    if value { "on" } else { "off" }
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = "aaaa0000";

    /// An announce that agrees with what we already know does nothing. It is the common
    /// case: one Announce every 5 seconds, per peer.
    #[test]
    fn an_agreeing_announce_does_nothing() {
        assert_eq!(
            on_announce(true, true, true, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            on_announce(false, false, true, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
    }

    /// Disagreeing does **not** change anything by itself: it only fires the fetch. The
    /// gossip is a trigger, never a value.
    #[test]
    fn a_principal_disagreement_triggers_a_fetch() {
        assert_eq!(
            on_announce(false, true, true, PRINCIPAL),
            PolicyOutcome::Fetch {
                principal: PRINCIPAL.to_string()
            }
        );
    }

    /// This is the test that holds up the whole feature: any node on the network
    /// announcing "no TLS" is ignored. The UDP is cleartext and unauthenticated, and
    /// obeying it would be offering the downgrade that TLS exists to
    /// prevent.
    #[test]
    fn a_non_principal_announce_is_ignored_even_when_disagreeing() {
        assert_eq!(
            on_announce(true, false, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            on_announce(false, true, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
    }

    /// A principal that is down downgrades nobody.
    #[test]
    fn a_failing_fetch_preserves_the_local_value() {
        assert_eq!(
            adopt_or_keep(true, Err(anyhow::anyhow!("connection refused"))),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            adopt_or_keep(false, Err(anyhow::anyhow!("connection refused"))),
            PolicyOutcome::Unchanged
        );
    }

    #[test]
    fn a_successful_fetch_adopts_the_principal_value() {
        assert_eq!(adopt_or_keep(false, Ok(true)), PolicyOutcome::Adopt(true));
        assert_eq!(adopt_or_keep(true, Ok(false)), PolicyOutcome::Adopt(false));
        assert_eq!(adopt_or_keep(true, Ok(true)), PolicyOutcome::Unchanged);
    }
}
