#[cfg(test)]
mod delete_tests {
    use super::*;

    /// The Ollama store warning exists because deleting a blob from there leaves the
    /// manifest pointing at nothing - the store ends up broken, and it is not
    /// ours to fix.
    #[test]
    fn an_ollama_blob_is_recognised_by_its_layout() {
        let root = std::env::temp_dir().join(format!("pom-del-{}", std::process::id()));
        let blobs = root.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        let blob = blobs.join("sha256-abc");
        std::fs::write(&blob, b"GGUF").unwrap();

        assert!(is_ollama_blob(&blob));

        // A `blobs/` without a neighbouring `manifests/` is not the Ollama store -
        // LM Studio also has a folder with that name, and empty.
        let other = std::env::temp_dir().join(format!("pom-del2-{}", std::process::id()));
        std::fs::create_dir_all(other.join("blobs")).unwrap();
        let loose = other.join("blobs").join("sha256-xyz");
        std::fs::write(&loose, b"GGUF").unwrap();
        assert!(!is_ollama_blob(&loose));

        let slice = root.join("solto.gguf");
        std::fs::write(&slice, b"GGUF").unwrap();
        assert!(!is_ollama_blob(&slice));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }
}

/// "Leave the network" on a machine's card.
#[cfg(test)]
mod leave_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};
    use axum::routing::post as axum_post;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A machine that serves `/api/ui/leave` and counts the requests.
    async fn fake_machine(status: StatusCode) -> (u16, Arc<AtomicU32>) {
        let hits = Arc::new(AtomicU32::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/api/ui/leave",
            axum_post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (status, Json(json!({ "detached": true })))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, hits)
    }

    /// A test state that **has** administrative authority. Relaying
    /// "leave the network" to another machine is an administrator's act; leaving alone is
    /// not, and that is why the abdication tests use the normal state.
    async fn admin_state_with(node_id: &str, peers: Vec<PeerInfo>) -> GatewayState {
        let state = state_with(node_id, peers).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        state
    }

    /// A peer's card makes the local node relay: the panel only talks to its
    /// own node, and the one that shuts down is always the owner of that process.
    #[tokio::test]
    async fn leaving_for_a_peer_is_forwarded_to_that_peer() {
        let (port, hits) = fake_machine(StatusCode::OK).await;
        let state = admin_state_with("eu", vec![peer("outro", "ksia", port)]).await;

        let resp = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("outro".into()),
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_of(resp).await["node_id"], "outro");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // And this node stays in the network: the request was about the other machine.
        assert!(
            !state.network.is_detached(),
            "forwarding cannot turn off whoever forwarded"
        );
    }

    /// An absent body and the node's own `node_id` are the same thing: this node leaves. It is the
    /// path the "This node" menu and the Network screen always used.
    #[tokio::test]
    async fn leaving_without_a_target_still_detaches_this_node() {
        let state = state_with("eu", vec![]).await;
        let resp = leave_network(ConnectionAuth, State(state.clone()), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.network.is_detached());

        let state = state_with("eu", vec![]).await;
        let _ = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("eu".into()),
            })),
        )
        .await;
        assert!(state.network.is_detached());
    }

    /// A machine that does not answer is a refusal with a reason, not a false "left".
    #[tokio::test]
    async fn a_peer_that_refuses_is_not_reported_as_gone() {
        let (port, _) = fake_machine(StatusCode::INTERNAL_SERVER_ERROR).await;
        let state = admin_state_with("eu", vec![peer("outro", "ksia", port)]).await;

        let resp = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("outro".into()),
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.network.is_detached());

        // A peer that does not even exist in the table does not become a success either.
        let resp = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("fantasma".into()),
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Leaving is abdicating one's own post: `infer` and `node` shut down their
    /// own machine without anyone's permission. Only a target other than itself
    /// asks for authority.
    #[tokio::test]
    async fn any_profile_detaches_its_own_machine() {
        for profile in [
            crate::profile::NodeProfile::Visitor,
            crate::profile::NodeProfile::Worker,
        ] {
            let state = state_with("eu", vec![]).await;
            state.auth.write().await.profile = profile;
            assert!(
                state.runtime.config().await.is_none(),
                "the test starts without a model"
            );
            let resp = leave_network(ConnectionAuth, State(state.clone()), None).await;
            assert_eq!(resp.status(), StatusCode::OK, "perfil={profile}");
            assert!(state.network.is_detached(), "perfil={profile}");
            assert_eq!(
                state.runtime.status().await.state,
                crate::runtime::ServerState::Disabled,
                "leaving cannot depend on a Ready server, profile={profile}"
            );
        }
    }
}

#[cfg(test)]
mod catalog_under_tls_tests {
    use super::*;
    use crate::gateway::testkit::state_with;
    use crate::network::PeerInfo;

    /// Port 1, closed on purpose: what is asserted here is not that the call
    /// succeeded, but **which client** it went out through.
    fn peer_with_gateway(node_id: &str) -> PeerInfo {
        crate::gateway::testkit::peer(node_id, node_id, 1)
    }

    /// The deploy screen lists the slices by asking each peer, and those
    /// calls have to go out through the pinning client.
    ///
    /// This test exists because of a real defect: the control plane kept
    /// using the shared client without pinning after TLS came in, and with
    /// the policy on **every** peer query failed silently - the screen said
    /// "no machine in the cluster has slices of <model>" and there was no way to
    /// deploy anything. The per-peer cache only gains an entry when someone asked for a
    /// pinned client, so it is the proof that the right path was used.
    #[tokio::test]
    async fn cluster_models_talks_to_the_peers_through_the_pinned_client() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let state = state_with("eu", vec![peer_with_gateway("outro")]).await;
        state.peer_http.apply_cluster_policy(true);
        assert!(state.peer_http.cached_peers().is_empty());

        let _ = cluster_models(State(state.clone())).await;

        assert_eq!(
            state.peer_http.cached_peers(),
            vec!["outro".to_string()],
            "the catalog query went out through a client without pinning"
        );
        state.peer_http.apply_cluster_policy(false);
    }

    /// The same proof for the deploy screen's survey (`deploy::survey`),
    /// which is where the defect showed up for the operator.
    #[tokio::test]
    async fn the_machine_list_for_deploying_is_pinned_too() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let state = state_with("eu", vec![peer_with_gateway("outro")]).await;
        state.peer_http.apply_cluster_policy(true);

        let _ = crate::deploy::survey_for_tests(&state, "qwen35").await;

        // The node itself also enters: the survey asks its catalog
        // through the same HTTP route, and under TLS that also needs a verified
        // certificate. What matters is that the peer is there.
        assert!(
            state
                .peer_http
                .cached_peers()
                .contains(&"outro".to_string()),
            "the survey went out through a client without pinning: {:?}",
            state.peer_http.cached_peers()
        );
        state.peer_http.apply_cluster_policy(false);
    }
}

#[cfg(test)]
mod cluster_logs_settings_tests {
    use super::*;
    use crate::gateway::testkit::state_with;

    #[tokio::test]
    async fn a_regular_node_cannot_turn_logs_on() {
        let state = state_with("eu", vec![]).await;

        let response = save_cluster_logs(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveClusterLogsRequest { logs_enabled: true }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!state.cluster_logs.enabled());
        assert!(!state.network.cluster_logs_enabled());
        assert!(!state.auth.read().await.cluster_logs_enabled);
    }

    #[tokio::test]
    async fn the_principal_turns_collection_on_and_off_without_the_logs_plugin() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;

        // The cluster console lives in the core: collection needs no plugin.
        let response = save_cluster_logs(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveClusterLogsRequest { logs_enabled: true }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.cluster_logs.enabled());
        assert!(state.auth.read().await.cluster_logs_enabled);

        let response = save_cluster_logs(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveClusterLogsRequest {
                logs_enabled: false,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!state.cluster_logs.enabled());
        assert!(!state.network.cluster_logs_enabled());
        assert!(!state.auth.read().await.cluster_logs_enabled);
    }
}

#[cfg(test)]
mod network_chat_settings_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with, state_with_secondary_admin};
    use axum::routing::post as axum_post;

    #[tokio::test]
    async fn principal_toggle_survives_the_next_authenticated_policy_cycle() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;

        let response = save_network_chat(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveNetworkChatRequest {
                network_chat_enabled: true,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await["network_chat_enabled"], true);
        assert!(state.auth.read().await.network_chat_enabled);
        assert!(state.network.network_chat_enabled());
        assert!(matches!(
            crate::network_chat_policy::adopt_or_keep(true, Ok(true)),
            crate::network_chat_policy::PolicyOutcome::Unchanged
        ));
    }

    #[tokio::test]
    async fn secondary_admin_forwards_the_toggle_to_the_principal() {
        let app = Router::new().route(
            "/api/ui/network/chat",
            axum_post(|| async { Json(json!({ "network_chat_enabled": true })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let primary_key = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let mut primary = peer("primary", "primary", port);
        primary.endpoint_id = Some("peer-primary".into());
        primary.pubkey = hex::encode(primary_key.verifying_key().as_bytes());
        let state = state_with_secondary_admin("secondary", vec![primary]).await;

        let response = save_network_chat(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveNetworkChatRequest {
                network_chat_enabled: true,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await["network_chat_enabled"], true);
        assert!(
            !state.auth.read().await.network_chat_enabled,
            "the secondary does not write the policy locally"
        );
    }
}

#[cfg(test)]
mod advertise_addr_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

    /// The response already carries the effective value, so the screen confirms the
    /// effect without a second round trip - it is the very reason this field exists
    /// in the response.
    #[tokio::test]
    async fn pinning_an_address_takes_effect_immediately_and_live() {
        let state = state_with("eu", vec![]).await;

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "203.0.113.9:42777".into(),
                advertise_only: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = body_of(resp).await;
        assert_eq!(body["advertise_addr"], "203.0.113.9:42777");
        assert_eq!(body["effective_address"], "203.0.113.9:42777");

        // Live: the same `Network` already announces the new value, without a restart.
        assert_eq!(state.network.announce_addr(), "203.0.113.9:42777");
        // And persisted: re-reading the config on disk sees the same value.
        assert_eq!(
            state.auth.read().await.advertise_addr.as_deref(),
            Some("203.0.113.9:42777")
        );
    }

    /// An empty string clears the pin, same convention as `SavePathsRequest.shard_dir`.
    #[tokio::test]
    async fn an_empty_value_clears_the_pin() {
        let state = state_with("eu", vec![]).await;
        state.network.set_advertise_addr(Some("203.0.113.9:42777"));

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "".into(),
                advertise_only: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(state.auth.read().await.advertise_addr, None);
    }

    /// A value with no recognizable form is refused, not stored broken -
    /// same spirit as `check_dir` for model folders.
    #[tokio::test]
    async fn a_malformed_value_is_rejected_not_saved() {
        let state = state_with("eu", vec![]).await;

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "[not-a-valid-ipv6]".into(),
                advertise_only: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(state.auth.read().await.advertise_addr, None);
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

    /// Turning iroh off is the direction every consumer of the mode follows
    /// live: the gate holds, the announce round takes the direct path, the
    /// record stops being republished. The switch saves and applies it in one
    /// call, and the answer says so (spec 2026-11-15).
    #[tokio::test]
    async fn turning_iroh_off_saves_the_mode_and_holds_the_relays_at_once() {
        let state = state_with("eu", vec![]).await;

        let resp = save_transport(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTransportRequest { no_iroh: true }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_of(resp).await;
        assert_eq!(body["no_iroh"], true);
        assert_eq!(body["live"], true);
        assert_eq!(body["restart_needed"], false);

        // Live: the network and the gate hold the mode now, without a restart.
        assert!(state.network.is_direct_only());
        assert!(state.network.relay_gate().status().direct_only);
        // Persisted: the next start reads the same mode off the config file.
        assert!(state.auth.read().await.no_iroh);
    }

    /// Turning iroh back on cannot be applied to a run whose endpoint was built
    /// without the gossip subscription: the switch saves the intent and the
    /// view says it waits for the restart, instead of leaving the node
    /// announcing into silence.
    #[tokio::test]
    async fn saving_a_reenable_waits_for_the_restart_it_needs() {
        let state = state_with("eu", vec![]).await;
        state.network.set_direct_only(true);
        state.network.relay_gate().set_direct_only(true);

        let resp = save_transport(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTransportRequest { no_iroh: false }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_of(resp).await;
        assert_eq!(body["no_iroh"], false);
        assert_eq!(body["live"], true);
        assert_eq!(body["restart_needed"], true);

        // The run keeps the mode it started with; only the file changed.
        assert!(state.network.is_direct_only());
        assert!(!state.auth.read().await.no_iroh);
    }

    /// The startup refuses the pair iroh-off + relaying for any network, so the
    /// routes refuse saving it too: otherwise the screen would write a file the
    /// next start cannot boot.
    #[tokio::test]
    async fn the_transport_and_relaying_for_any_network_refuse_each_other() {
        let state = state_with("eu", vec![]).await;
        {
            let mut cfg = state.auth.write().await;
            cfg.relay.everyone = true;
            cfg.save().expect("save relay option");
        }

        let resp = save_transport(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTransportRequest { no_iroh: true }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.network.is_direct_only());
        assert!(!state.auth.read().await.no_iroh);

        // The other direction: with the mode live, the relay route refuses the
        // option that contradicts it.
        state.auth.write().await.relay.everyone = false;
        let resp = save_transport(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTransportRequest { no_iroh: true }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.network.is_direct_only());

        let resp = save_relay(
            ConnectionAuth,
            State(state.clone()),
            Json(crate::config::RelayConfig {
                everyone: true,
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.auth.read().await.relay.everyone);
    }

    /// The read side answers what the switch shows: the saved value, what this
    /// run is doing, and the divergence the command line leaves behind.
    #[tokio::test]
    async fn the_transport_view_separates_the_saved_mode_from_the_live_one() {
        let mut state = state_with("eu", vec![]).await;
        state.restart_needed = true;

        let body = body_of(get_transport(ConnectionAuth, State(state.clone())).await).await;
        assert_eq!(body["no_iroh"], false);
        assert_eq!(body["live"], false);
        // The flag holds iroh off while the file says on: the screen must say
        // the saved value only takes effect once the flag is gone.
        assert_eq!(body["restart_needed"], true);
    }
}
