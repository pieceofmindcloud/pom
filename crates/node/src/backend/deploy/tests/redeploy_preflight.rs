#[cfg(test)]
mod redeploy_preflight_tests {
    use super::*;
    use crate::deployment::{Deployment, DeploymentIdentity, DeploymentKind, DeploymentNode};
    use crate::gateway::testkit::state_with;
    use axum::routing::{get as axum_get, post as axum_post};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    async fn peer_with_deployment(
        identity: Option<DeploymentIdentity>,
        unload_status: axum::http::StatusCode,
        identity_path: &'static str,
    ) -> (u16, Arc<AtomicU32>) {
        let released = Arc::new(AtomicBool::new(false));
        let unload_hits = Arc::new(AtomicU32::new(0));
        let record = Deployment::new(
            "d-aaaaaaaa".into(),
            DeploymentKind::Whole,
            crate::deployment::DeploymentModel {
                label: "qwen".into(),
                total_layers: 32,
                pom_id: None,
                identity,
            },
            "coordinator",
            vec![DeploymentNode {
                node_id: "peer-a".into(),
                hostname: "ksia".into(),
                start_layer: 0,
                end_layer: 31,
                peer_index: 0,
                peer_count: 1,
                primary: true,
                part_sha256: None,
                source_model_path: None,
                tp_rank: None,
                config: RuntimeParams::default(),
            }],
        );
        let deployments = json!({ "deployments": [record] });
        let released_for_unload = released.clone();
        let unload_hits_for_route = unload_hits.clone();
        let released_for_node = released.clone();
        let app = Router::new()
            .route(
                "/api/ui/deployments",
                axum_get(move || {
                    let deployments = deployments.clone();
                    async move { Json(deployments) }
                }),
            )
            .route(
                "/api/ui/unload",
                axum_post(move || {
                    let released = released_for_unload.clone();
                    let hits = unload_hits_for_route.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if unload_status.is_success() {
                            released.store(true, Ordering::SeqCst);
                        }
                        (
                            unload_status,
                            Json(json!({ "unloaded": unload_status.is_success() })),
                        )
                    }
                }),
            )
            .route(
                "/api/ui/node",
                axum_get(move || {
                    let released = released_for_node.clone();
                    async move {
                        if released.load(Ordering::SeqCst) {
                            Json(json!({
                                "config": null,
                                "status": {"state": "disabled"},
                                "active_model": "",
                                "active_deployment": ""
                            }))
                        } else {
                            Json(json!({
                                "config": {"model_path": "/models/active.gguf"},
                                "status": {"state": "ready"},
                                "active_model": "qwen",
                                "active_deployment": "d-aaaaaaaa"
                            }))
                        }
                    }
                }),
            )
            .route(
                "/api/ui/model-identity",
                axum_post(move || async move {
                    Json(json!({
                        "canonical_path": identity_path,
                        "sha256": serde_json::Value::Null
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, unload_hits)
    }

    fn serving_peer(port: u16) -> PeerInfo {
        let mut peer = crate::gateway::testkit::peer("peer-a", "ksia", port);
        peer.model = "qwen".into();
        peer.deploy_id = "d-aaaaaaaa".into();
        peer.start_layer = 0;
        peer.end_layer = 31;
        peer.total_layers = 32;
        peer
    }

    fn desired(hash: char) -> RequestedModelIdentity {
        RequestedModelIdentity {
            proof: Some(DeploymentIdentity::Sha256 {
                sha256: hash.to_string().repeat(64),
            }),
            paths_by_peer: HashMap::new(),
        }
    }

    async fn install_job(state: &GatewayState, id: &str) {
        jobs()
            .write()
            .await
            .insert(id.into(), new_job(id, "deploy", "teste", "preparando"));
        let _ = state;
    }

    #[tokio::test]
    async fn same_proven_model_is_released_and_waited_before_continue() {
        let (port, hits) = peer_with_deployment(
            Some(DeploymentIdentity::Sha256 {
                sha256: "a".repeat(64),
            }),
            axum::http::StatusCode::OK,
            "/models/active.gguf",
        )
        .await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "preflight-same").await;

        prepare_existing_model(&state, "qwen", &desired('a'), None, "preflight-same")
            .await
            .expect("the equal deployment must be released");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_machine_outside_the_plan_is_kept_when_asked() {
        let (port, hits) = peer_with_deployment(
            Some(DeploymentIdentity::Sha256 {
                sha256: "a".repeat(64),
            }),
            axum::http::StatusCode::OK,
            "/models/active.gguf",
        )
        .await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "preflight-keep").await;

        let plan_nodes = vec!["peer-b".to_string()];
        prepare_existing_model(
            &state,
            "qwen",
            &desired('a'),
            Some(&plan_nodes),
            "preflight-keep",
        )
        .await
        .expect("a kept deployment does not block the new one");
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        // The same machine inside the plan is still released.
        let plan_nodes = vec!["peer-a".to_string()];
        prepare_existing_model(
            &state,
            "qwen",
            &desired('a'),
            Some(&plan_nodes),
            "preflight-keep",
        )
        .await
        .expect("a machine of the plan is released");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn identity_failure_is_visible_and_does_not_unload() {
        let (port, hits) =
            peer_with_deployment(None, axum::http::StatusCode::OK, "/models/different.gguf").await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "preflight-unknown").await;
        let error = prepare_existing_model(
            &state,
            "qwen",
            &RequestedModelIdentity {
                proof: Some(DeploymentIdentity::Sha256 {
                    sha256: "a".repeat(64),
                }),
                paths_by_peer: [("peer-a".into(), "/models/qwen.gguf".into())]
                    .into_iter()
                    .collect(),
            },
            None,
            "preflight-unknown",
        )
        .await
        .expect_err("without proof, it cannot undeploy");
        assert!(error.contains("could not prove"), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_unload_is_visible_and_blocks_the_new_deploy() {
        let (port, hits) = peer_with_deployment(
            Some(DeploymentIdentity::Sha256 {
                sha256: "a".repeat(64),
            }),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "/models/active.gguf",
        )
        .await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "preflight-failed").await;
        let error = prepare_existing_model(&state, "qwen", &desired('a'), None, "preflight-failed")
            .await
            .expect_err("an unload failure blocks the deploy");
        assert!(error.contains("undeploy failed"), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn idle_timeout_names_the_peer_and_the_occupied_state() {
        let app = Router::new().route(
            "/api/ui/node",
            axum_get(|| async {
                Json(json!({
                    "config": {"model_path": "/models/active.gguf"},
                    "status": {"state": "ready"},
                    "active_model": "qwen",
                    "active_deployment": "d-aaaaaaaa"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        let error =
            wait_for_peer_idle_with_timeout(&state, &serving_peer(port), Duration::from_millis(10))
                .await
                .expect_err("a busy peer must exceed the limit");
        assert!(error.contains("ksia") && error.contains("busy"), "{error}");
    }

    /// A node (`peer-a`) that is serving nothing: `/api/ui/node` says
    /// `config: null` until `/api/ui/reconfigure` is called, and from then on
    /// says `ready`. It is the case that broke the old `redeploy`.
    async fn peer_not_serving() -> (u16, Arc<AtomicU32>, Arc<AtomicU32>, Arc<AtomicU32>) {
        let seq = Arc::new(AtomicU32::new(0));
        let reconfigured_at = Arc::new(AtomicU32::new(0));
        let unloaded_at = Arc::new(AtomicU32::new(0));
        let serving = Arc::new(AtomicBool::new(false));

        let seq_u = seq.clone();
        let unloaded_at_r = unloaded_at.clone();
        let seq_r = seq.clone();
        let reconfigured_at_r = reconfigured_at.clone();
        let serving_r = serving.clone();
        let serving_n = serving.clone();

        let app = Router::new()
            .route(
                "/api/ui/unload",
                axum_post(move || {
                    let seq = seq_u.clone();
                    let at = unloaded_at_r.clone();
                    async move {
                        at.store(seq.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        Json(json!({ "unloaded": true }))
                    }
                }),
            )
            .route(
                "/api/ui/reconfigure",
                axum_post(move || {
                    let seq = seq_r.clone();
                    let at = reconfigured_at_r.clone();
                    let serving = serving_r.clone();
                    async move {
                        at.store(seq.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        serving.store(true, Ordering::SeqCst);
                        Json(json!({ "reconfigured": true }))
                    }
                }),
            )
            .route(
                "/api/ui/node",
                axum_get(move || {
                    let serving = serving_n.clone();
                    async move {
                        if serving.load(Ordering::SeqCst) {
                            Json(json!({
                                "config": {"model_path": "/models/active.gguf", "model_label": "qwen"},
                                "status": {"state": "ready"},
                                "active_model": "qwen",
                                "active_deployment": "d-aaaaaaaa"
                            }))
                        } else {
                            Json(json!({
                                "config": null,
                                "status": {"state": "disabled"},
                                "active_model": "",
                                "active_deployment": ""
                            }))
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, seq, reconfigured_at, unloaded_at)
    }

    fn whole_record() -> Deployment {
        Deployment::new(
            "d-aaaaaaaa".into(),
            DeploymentKind::Whole,
            crate::deployment::DeploymentModel {
                label: "qwen".into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordinator",
            vec![DeploymentNode {
                node_id: "peer-a".into(),
                hostname: "ksia".into(),
                start_layer: 0,
                end_layer: 31,
                peer_index: 0,
                peer_count: 1,
                primary: true,
                part_sha256: None,
                source_model_path: None,
                tp_rank: None,
                config: RuntimeParams::default(),
            }],
        )
    }

    #[tokio::test]
    async fn a_node_that_is_not_serving_is_relaunched_not_failed() {
        let (port, _seq, reconfigured_at, unloaded_at) = peer_not_serving().await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "redeploy-down").await;

        let failures = run_redeploy(
            state,
            whole_record(),
            RedeployRequest {
                params: Some(RuntimeParams::default()),
                version: None,
                per_node: HashMap::new(),
            },
            "redeploy-down",
        )
        .await
        .expect("the redeploy must not fail outright");

        assert_eq!(
            failures, 0,
            "the machine that was not serving must be started, not counted as a failure"
        );
        assert!(
            reconfigured_at.load(Ordering::SeqCst) > 0,
            "/api/ui/reconfigure must be called"
        );
        assert!(
            unloaded_at.load(Ordering::SeqCst) < reconfigured_at.load(Ordering::SeqCst),
            "the unload comes before the start (unload={}, reconfigure={})",
            unloaded_at.load(Ordering::SeqCst),
            reconfigured_at.load(Ordering::SeqCst),
        );
    }
}
