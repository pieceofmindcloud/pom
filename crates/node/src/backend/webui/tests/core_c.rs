/// An announced address that stopped answering.
///
/// The defect these tests pin down: while `gateway_url()` was just
/// `peer.addr`, the control plane had **one** address and no memory of
/// failure. A rotating temporary IPv6 (macOS generates a new one and retires the old
/// one by itself) left the Dashboard dialing an address that exists on no machine
/// at all, forever, with the message "nothing below can be read" - while
/// the same peer was alive and announced on another address, already inside
/// `addr_candidates`.
#[cfg(test)]
mod dead_address_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};

    /// A real `/api/ui/node` on the loopback, which counts how many times it was
    /// read. Returns `(port, counter)`.
    async fn live_node(hostname: &str) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let name = hostname.to_string();
        let app = Router::new().route(
            "/api/ui/node",
            get(move || {
                let name = name.clone();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Json(json!({ "hostname": name, "node_id": "outro" })) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        (port, hits)
    }

    /// `127.0.0.2` on the same port is the dead address: on macOS the SYN vanishes
    /// (no alias on lo0), on Linux it is refused. Both arrive at the same
    /// place - a transport error - which is what a retired address produces.
    fn dead_host() -> &'static str {
        "127.0.0.2"
    }

    fn peer_with_dead_address(port: u16) -> PeerInfo {
        let mut p = peer("outro", "macbook", port);
        // What the cluster stored: the dead address in front, because it is what
        // the peer announced when it was still its own.
        p.addr = format!("{}:{port}", dead_host());
        p.addr_candidates = vec![
            format!("{}:{port}", dead_host()),
            format!("127.0.0.1:{port}"),
        ];
        p.capabilities = vec![routing_contract::ModelCapability {
            model_id: "qwen".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: Some(16),
            supports_inference: true,
        }];
        p.model = "qwen".into();
        p.start_layer = 0;
        p.end_layer = 15;
        p.total_layers = 16;
        p
    }

    /// The test of the defect: a single Dashboard read already recovers the peer through
    /// the live address, **without manual intervention**, and the dead address is
    /// recorded as such so it is not dialed again.
    #[tokio::test]
    async fn a_dead_announced_address_is_forgotten_and_the_peer_is_read_at_the_new_one() {
        let _scheme_lock = crate::peer_client::GLOBAL_POLICY.lock();
        crate::peer_client::set_tls_enabled(false);

        let (port, hits) = live_node("macbook").await;
        let p = peer_with_dead_address(port);
        let dead = format!("http://{}:{port}", dead_host());
        let alive = format!("http://127.0.0.1:{port}");
        let state = state_with("eu", vec![p]).await;

        let body = body_of(cluster(HeaderMap::new(), State(state.clone())).await).await;
        let other = body["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["node_id"] == "outro")
            .expect("the peer must appear in the cluster");

        assert_eq!(
            other["node"]["hostname"], "macbook",
            "the read had to fall on the live address instead of dying on the first"
        );
        assert!(
            other["unreachable"].is_null(),
            "the permanent Dashboard warning cannot survive a readable peer: {}",
            other["unreachable"]
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        let routes = &state.network.routes;
        assert!(
            routes.is_failed("outro", &dead),
            "the address that did not answer must stay in the failure memory"
        );
        assert!(
            !routes.is_failed("outro", &alive),
            "the address that answered cannot stay discredited"
        );
        let candidates = state
            .all_peers()
            .await
            .into_iter()
            .find(|p| p.node_id == "outro")
            .unwrap()
            .gateway_candidates();
        assert_eq!(
            routes.usable("outro", &candidates),
            vec![alive],
            "the next read cannot pay the timeout of the dead address again"
        );
    }

    /// When **every** known address dies, the node stops insisting and moves on
    /// to rediscovering the peer. The screen needs this: "rediscovering" is work in
    /// progress, "cannot be read" is a dead end.
    #[tokio::test]
    async fn when_every_address_is_dead_the_node_asks_for_the_current_one() {
        let (port, _) = live_node("macbook").await;
        let mut p = peer_with_dead_address(port);
        p.addr_candidates = vec![format!("{}:{port}", dead_host())];
        let state = state_with("eu", vec![p]).await;

        // First read: discovers that the only address does not answer.
        let body = body_of(cluster(HeaderMap::new(), State(state.clone())).await).await;
        let other = body["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["node_id"] == "outro")
            .unwrap()
            .clone();
        assert_eq!(
            other["rediscovering"], true,
            "without a usable address, the Dashboard shows the node being rediscovered"
        );
        assert!(
            other["unreachable"].is_null(),
            "rediscovering is not unreachable: the user cannot read a dead end"
        );
        assert!(
            state.network.is_rediscovering("outro"),
            "the node must have asked for the current address of the peer"
        );

        // And the corpse left the local entry, so that the peer's next `Announce`
        // repopulates it instead of adding to it.
        let stored = state.network.peers.read().await["outro"]
            .addr_candidates
            .clone();
        assert!(
            stored.is_empty(),
            "the discredited candidate had to leave the entry: {stored:?}"
        );
    }

    /// "Rediscovering" has a deadline. Three unanswered questions to the network are no
    /// longer work in progress: they are a peer that announces over UDP and does not answer
    /// on HTTP. Insisting on the word would restore the dishonest warning this
    /// work came to remove, only optimistic instead of pessimistic.
    #[tokio::test]
    async fn rediscovering_gives_up_and_says_so_instead_of_promising_forever() {
        let (port, _) = live_node("macbook").await;
        let mut p = peer_with_dead_address(port);
        p.addr_candidates = vec![format!("{}:{port}", dead_host())];
        let state = state_with("eu", vec![p]).await;

        let read_file = |state: GatewayState| async move {
            body_of(cluster(HeaderMap::new(), State(state)).await)
                .await["peers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["node_id"] == "outro")
                .unwrap()
                .clone()
        };

        assert_eq!(read_file(state.clone()).await["rediscovering"], true);

        // Age the search past the deadline, without waiting in real time.
        state
            .network
            .age_rediscovery_for_test("outro", crate::network::REDISCOVERY_GIVE_UP);

        let gave_up = read_file(state.clone()).await;
        assert_eq!(
            gave_up["rediscovering"], false,
            "past the deadline, the screen cannot keep promising it rediscovers"
        );
        assert!(
            gave_up["unreachable"]
                .as_str()
                .is_some_and(|error| error.contains("known addresses")),
            "and it must say why: {}",
            gave_up["unreachable"]
        );
    }

    /// Coverage comes from gossip, not from the read. An unreadable peer keeps
    /// contributing layers - and it is honest that it does, because its
    /// inference server may be perfectly fine and only the control address
    /// died - but this cannot stay invisible.
    #[tokio::test]
    async fn an_unreadable_peer_cannot_pass_for_healthy_coverage() {
        let (port, _) = live_node("macbook").await;
        let mut p = peer_with_dead_address(port);
        p.addr_candidates = vec![format!("{}:{port}", dead_host())];
        let state = state_with("eu", vec![p]).await;

        let body = body_of(cluster(HeaderMap::new(), State(state)).await).await;
        let qwen = body["model_inventory"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["model_id"] == "qwen")
            .expect("the announced model enters the inventory");
        assert_eq!(
            qwen["covered_layers"], 16,
            "the layers stay announced; removing them would be lying in the other direction"
        );
        assert_eq!(
            qwen["unreadable_nodes"],
            json!(["outro"]),
            "the coverage must say whom it depends on without being able to read"
        );
    }

    #[tokio::test]
    async fn an_announced_model_is_not_complete_before_runtime_health() {
        let (port, _) = live_node("macbook").await;
        let mut p = peer("outro", "macbook", port);
        p.model = "qwen".into();
        p.start_layer = 0;
        p.end_layer = 15;
        p.total_layers = 16;
        p.inference_port = Some(8181);
        p.capabilities = vec![routing_contract::ModelCapability {
            model_id: "qwen".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: Some(16),
            supports_inference: true,
        }];
        p.runtime_capacity = None;
        let state = state_with("eu", vec![p]).await;

        let body = body_of(cluster(HeaderMap::new(), State(state)).await).await;
        let qwen = body["model_inventory"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["model_id"] == "qwen")
            .expect("the announced model enters the inventory");
        assert_eq!(qwen["covered_layers"], 16);
        assert_eq!(qwen["complete"], false);
    }
}

#[cfg(test)]
mod profile_network_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};

    #[tokio::test]
    async fn infer_network_response_contains_only_the_local_node() {
        let state = state_with("eu", vec![peer("outro", "salem", 1)]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Infer;

        let body = body_of(cluster(HeaderMap::new(), State(state)).await).await;
        let peers = body["peers"].as_array().expect("peers must be a list");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["node_id"], "eu");
        assert!(
            !peers.iter().any(|peer| peer["node_id"] == "outro"),
            "infer cannot receive data from another node"
        );
    }

    #[tokio::test]
    async fn chat_capabilities_keep_full_coverage_without_exposing_infer_topology() {
        let mut tail = peer("outro", "salem", 1);
        tail.model = "qwen".into();
        tail.start_layer = 16;
        tail.end_layer = 31;
        tail.total_layers = 32;
        let state = state_with("eu", vec![tail]).await;
        let mut config = crate::gateway::testkit::spec_config();
        config.model_label = "qwen".into();
        config.end_layer = 15;
        config.total_layers = 32;
        config.mmproj_path = Some("/models/mmproj-qwen.gguf".into());
        config.no_thinking = false;
        state.runtime.seed_config_for_test(config.clone()).await;
        *state.local_assignment.write().await = Some(crate::model::LayerAssignment {
            deploy_id: String::new(),
            model: "qwen".into(),
            total_layers: 32,
            start_layer: 0,
            end_layer: 15,
            layers_count: 16,
            memory_used_mb: 0.0,
            memory_allocated_mb: 0.0,
        });
        state
            .network
            .set_runtime_capacity(Some(crate::network::RuntimeCapacity {
                n_ctx: 8_192,
                max_new_tokens: 256,
            }));
        state.auth.write().await.profile = crate::profile::NodeProfile::Infer;

        let body = body_of(chat_capabilities(State(state.clone()), Query(Default::default())).await.into_response()).await;
        assert_eq!(body["vision"], "supported");
        assert_eq!(body["coverage"], "complete");
        assert_eq!(body["thinking"], "supported");

        config.no_thinking = true;
        state.runtime.seed_config_for_test(config).await;
        let body = body_of(chat_capabilities(State(state.clone()), Query(Default::default())).await.into_response()).await;
        assert_eq!(body["thinking"], "unsupported");

        let cluster_body = body_of(cluster(HeaderMap::new(), State(state)).await).await;
        assert_eq!(cluster_body["peers"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn chat_capabilities_answer_for_a_model_served_elsewhere_on_a_node_that_serves_nothing() {
        // The Ksia case: the browser's node only coordinates and loads nothing,
        // while another machine serves the whole model.
        let mut remote = peer("outro", "salem", 1);
        remote.model = "qwen".into();
        remote.start_layer = 0;
        remote.end_layer = 15;
        remote.total_layers = 16;
        remote.runtime_capacity = Some(crate::network::RuntimeCapacity {
            n_ctx: 8_192,
            max_new_tokens: 256,
        });
        let state = state_with("eu", vec![remote]).await;
        let ask = |model: &str| ChatCapabilitiesQuery { model: model.into() };

        let body = body_of(chat_capabilities(State(state.clone()), Query(ask(""))).await.into_response()).await;
        assert_eq!(body["active_model"], "qwen");
        assert_eq!(body["coverage"], "complete");

        let body = body_of(chat_capabilities(State(state.clone()), Query(ask("qwen@d-4b521593"))).await.into_response()).await;
        assert_eq!(body["active_model"], "qwen");
        assert_eq!(body["coverage"], "complete");

        let body = body_of(chat_capabilities(State(state), Query(ask("other"))).await.into_response()).await;
        assert_eq!(body["coverage"], "unavailable");
    }

    #[tokio::test]
    async fn chat_does_not_advertise_an_unready_remote_model() {
        let mut remote = peer("outro", "salem", 1);
        remote.model = "qwen".into();
        remote.start_layer = 0;
        remote.end_layer = 15;
        remote.total_layers = 16;
        remote.runtime_capacity = None;
        let mut state = state_with("eu", vec![remote]).await;
        state.default_model = "qwen".into();

        let body = body_of(chat_capabilities(State(state), Query(Default::default())).await.into_response()).await;
        assert_eq!(body["coverage"], "unavailable");
    }

    #[tokio::test]
    async fn chat_capabilities_reads_mmproj_from_a_remote_sliced_head_config() {
        let app = Router::new().route(
            "/api/ui/node",
            get(|| async {
                Json(json!({
                    "config": {
                        "start_layer": 0,
                        "model_label": "qwen",
                        "mmproj_path": "/models/mmproj-qwen.gguf",
                        "no_thinking": false
                    },
                    "active_model": "qwen"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut head = peer("head", "salem", port);
        head.model = "qwen".into();
        head.start_layer = 0;
        head.end_layer = 15;
        head.total_layers = 32;
        head.inference_port = Some(8181);
        let mut tail = peer("tail", "ksia", 1);
        tail.model = "qwen".into();
        tail.start_layer = 16;
        tail.end_layer = 31;
        tail.total_layers = 32;
        tail.inference_port = Some(8181);

        let mut state = state_with("eu", vec![head, tail]).await;
        state.default_model = "qwen".into();

        let body = body_of(chat_capabilities(State(state), Query(Default::default())).await.into_response()).await;
        assert_eq!(body["vision"], "supported");
        assert_eq!(body["coverage"], "complete");
        assert_eq!(body["thinking"], "supported");
    }

    #[tokio::test]
    async fn infer_head_response_selects_the_fastest_fresh_candidate() {
        let mut slow = peer("slow", "salem", 8091);
        slow.model = "qwen35".into();
        slow.start_layer = 0;
        slow.end_layer = 31;
        slow.total_layers = 32;
        slow.inference_port = Some(8181);
        let mut fast = peer("fast", "ksia", 8092);
        fast.model = "qwen35".into();
        fast.start_layer = 0;
        fast.end_layer = 31;
        fast.total_layers = 32;
        fast.inference_port = Some(8181);
        let mut middle = peer("middle", "atena", 8093);
        middle.model = "qwen35".into();
        middle.start_layer = 16;
        middle.end_layer = 31;
        middle.total_layers = 32;
        middle.inference_port = Some(8181);

        // a head still loading its model (no capacity published yet): never offered
        let mut loading = peer("loading", "ksia2", 8094);
        loading.model = "qwen35".into();
        loading.start_layer = 0;
        loading.end_layer = 31;
        loading.total_layers = 32;
        loading.inference_port = Some(8181);
        loading.runtime_capacity = None;

        let state = state_with("eu", vec![slow, fast, middle, loading]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Infer;
        state.network.rtts.remember("loading", 1.0, "direct-health");
        state.network.rtts.remember("slow", 80.0, "direct-health");
        state.network.rtts.remember("fast", 12.0, "p2p");

        let body = body_of(
            inference_heads(
                ConnectionAuth,
                State(state),
                Query(InferenceHeadsQuery {
                    model: "qwen35".into(),
                }),
            )
            .await
            .into_response(),
        )
        .await;

        assert_eq!(body["status"], "selected");
        assert_eq!(body["selected_node_id"], "fast");
        assert_eq!(body["selection_policy"], "coverage_then_weighted_rtt_load");
        assert_eq!(body["candidates"].as_array().unwrap().len(), 2);
        assert!(body["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| candidate["node_id"] != "middle" && candidate["node_id"] != "loading"));
        assert_eq!(
            body["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["node_id"] == "fast")
                .unwrap()["rtt_state"],
            "fresh"
        );
    }

    #[tokio::test]
    async fn infer_head_response_explains_when_no_candidate_exists() {
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Infer;

        let body = body_of(
            inference_heads(
                ConnectionAuth,
                State(state),
                Query(InferenceHeadsQuery {
                    model: "qwen35".into(),
                }),
            )
            .await
            .into_response(),
        )
        .await;

        assert_eq!(body["status"], "no_candidate");
        assert!(body["selected_node_id"].is_null());
        assert!(body["candidates"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn infer_head_response_does_not_choose_a_stale_low_rtt() {
        let mut fresh = peer("fresh", "salem", 8091);
        fresh.model = "qwen35".into();
        fresh.start_layer = 0;
        fresh.end_layer = 31;
        fresh.total_layers = 32;
        fresh.inference_port = Some(8181);
        let mut stale = peer("stale", "ksia", 8092);
        stale.model = "qwen35".into();
        stale.start_layer = 0;
        stale.end_layer = 31;
        stale.total_layers = 32;
        stale.inference_port = Some(8181);

        let state = state_with("eu", vec![fresh, stale]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Infer;
        state.network.rtts.remember("fresh", 80.0, "direct-health");
        state.network.rtts.remember_for_test_with_age(
            "stale",
            1.0,
            "p2p",
            crate::route::RTT_MEASURE_INTERVAL + std::time::Duration::from_secs(1),
        );

        let body = body_of(
            inference_heads(
                ConnectionAuth,
                State(state),
                Query(InferenceHeadsQuery {
                    model: "qwen35".into(),
                }),
            )
            .await
            .into_response(),
        )
        .await;

        assert_eq!(body["selected_node_id"], "fresh");
        assert_eq!(
            body["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["node_id"] == "stale")
                .unwrap()["rtt_state"],
            "stale"
        );
    }
}
