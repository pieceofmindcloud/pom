/// Taking the whole pipeline down.
#[cfg(test)]
mod undeploy_tests {
    use super::*;
    use crate::deployment::{Deployment, DeploymentKind, DeploymentModel, DeploymentNode};
    use crate::gateway::testkit::{body_of, state_with};
    use crate::gateway::GatewayState;
    use crate::profile::NodeProfile;
    use axum::routing::post as axum_post;
    use axum::{body::Body, http::Request};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn admin_state(peers: Vec<PeerInfo>) -> GatewayState {
        let state = state_with("eu", peers).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        state
    }

    /// A peer serving `qwen35` on the given range.
    fn peer(node_id: &str, host: &str, gateway_port: u16, range: (u32, u32)) -> PeerInfo {
        let mut p = crate::gateway::testkit::peer(node_id, host, gateway_port);
        p.model = "qwen35".into();
        p.start_layer = range.0;
        p.end_layer = range.1;
        p.total_layers = 32;
        p
    }

    fn deployment_node(
        node_id: &str,
        start_layer: u32,
        end_layer: u32,
        peer_index: u32,
        peer_count: u32,
    ) -> DeploymentNode {
        DeploymentNode {
            node_id: node_id.into(),
            hostname: node_id.into(),
            start_layer,
            end_layer,
            peer_index,
            peer_count,
            primary: start_layer == 0,
            part_sha256: None,
            source_model_path: None,
            tp_rank: None,
            config: RuntimeParams::default(),
        }
    }

    fn deployment(nodes: Vec<DeploymentNode>) -> Deployment {
        Deployment::new(
            "d-aaaaaaaa".into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: "qwen35".into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            nodes,
        )
    }

    /// A machine that answers `/api/ui/unload` with the requested status,
    /// counting how many times it was called.
    async fn fake_machine(status: axum::http::StatusCode) -> (u16, Arc<AtomicU32>) {
        let hits = Arc::new(AtomicU32::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/api/ui/unload",
            axum_post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (status, Json(json!({ "unloaded": true })))
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

    /// Two deployments of the same model: taking one down has to leave the
    /// other serving.
    ///
    /// Before `deploy_id` this was inexpressible both ways - it was not possible
    /// to have both up, and `resolve_group` could only talk about labels, so an
    /// "undeploy qwen35" would have taken both along.
    #[tokio::test]
    async fn undeploying_one_deployment_leaves_the_other_serving() {
        let (port_a, calls_a) = fake_machine(axum::http::StatusCode::OK).await;
        let (port_b, calls_b) = fake_machine(axum::http::StatusCode::OK).await;

        let mut a = peer("no-a", "maquina-a", port_a, (0, 31));
        let mut b = peer("no-b", "maquina-b", port_b, (0, 31));
        a.deploy_id = "d-aaaaaaaa".into();
        b.deploy_id = "d-bbbbbbbb".into();

        let state = admin_state(vec![a, b]).await;

        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: "d-aaaaaaaa".into(),
                group: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_of(resp).await;

        assert_eq!(body["released"], 1, "only the chosen deployment goes down");
        assert_eq!(
            body["results"].as_array().unwrap().len(),
            1,
            "the other deployment does not even appear in the report"
        );
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
        assert_eq!(
            calls_b.load(Ordering::SeqCst),
            0,
            "the machine of the other deployment cannot be touched"
        );
    }

    /// An id nobody serves must not become "take the whole label down".
    #[tokio::test]
    async fn an_unknown_deployment_id_releases_nothing() {
        let (port, calls) = fake_machine(axum::http::StatusCode::OK).await;
        let mut a = peer("no-a", "maquina-a", port, (0, 31));
        a.deploy_id = "d-aaaaaaaa".into();

        let state = admin_state(vec![a]).await;

        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: "d-cccccccc".into(),
                group: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// The report has one line per machine, and a machine that did not confirm
    /// does **not** count as released. Faking success here would leave memory
    /// stuck in a box the operator thinks he emptied.
    #[tokio::test]
    async fn every_machine_is_reported_and_a_silent_one_is_not_a_success() {
        let (ok_port, ok_hits) = fake_machine(axum::http::StatusCode::OK).await;
        let (bad_port, bad_hits) =
            fake_machine(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let state = admin_state(vec![
            peer("no-a", "ksia", ok_port, (0, 15)),
            peer("no-b", "macbook", bad_port, (16, 31)),
        ])
        .await;

        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: String::new(),
                group: "qwen35".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_of(resp).await;

        assert_eq!(body["group"], "qwen35");
        assert_eq!(body["released"], 1);
        assert_eq!(body["failed"], 1);

        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "one row per machine, always");
        // Sorted by start layer: the report reads in pipeline order.
        assert_eq!(results[0]["hostname"], "ksia");
        assert_eq!(results[0]["released"], true);
        assert!(results[0].get("error").is_none());

        assert_eq!(results[1]["hostname"], "macbook");
        assert_eq!(results[1]["released"], false);
        assert!(
            results[1]["error"].as_str().unwrap().contains("500"),
            "the reason must be what the machine said: {}",
            results[1]["error"]
        );

        // Both were really called: neither was skipped because of the other.
        assert_eq!(ok_hits.load(Ordering::SeqCst), 1);
        assert_eq!(bad_hits.load(Ordering::SeqCst), 1);
    }

    /// A machine that is down is a failure with a reason, not a line that vanishes.
    #[tokio::test]
    async fn an_unreachable_machine_still_gets_a_line() {
        let (ok_port, _) = fake_machine(axum::http::StatusCode::OK).await;
        // A port nobody answers: reserved and immediately given back.
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let state = admin_state(vec![
            peer("no-a", "ksia", ok_port, (0, 15)),
            peer("no-b", "sumida", dead, (16, 31)),
        ])
        .await;

        let body = body_of(
            undeploy_endpoint(
                ConnectionAuth,
                State(state),
                Json(UndeployRequest {
                    deploy_id: String::new(),
                    group: "qwen35".into(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(body["failed"], 1);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results[1]["hostname"], "sumida");
        assert_eq!(results[1]["released"], false);
        assert!(results[1]["error"].is_string());
    }

    /// With no pipeline up there is nothing to undeploy, and saying so is better
    /// than returning an empty report that reads as success.
    #[tokio::test]
    async fn undeploying_nothing_is_a_refusal_not_an_empty_report() {
        let state = admin_state(vec![]).await;
        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: String::new(),
                group: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    /// Which pipeline to take down: obvious with one, a refusal with two. Taking
    /// the wrong one down costs a whole redeployment, so the guess is not ours.
    #[test]
    fn the_group_is_only_guessed_when_there_is_no_doubt() {
        let one = vec![peer("a", "h", 1, (0, 15)), peer("b", "h", 2, (16, 31))];
        assert_eq!(resolve_group("", &one).unwrap(), "qwen35");
        assert_eq!(resolve_group("outro", &one).unwrap(), "outro");

        let mut two = one.clone();
        two[1].model = "llama3".into();
        let error = resolve_group("", &two).unwrap_err();
        assert!(error.contains("llama3") && error.contains("qwen35"), "{error}");

        assert!(resolve_group("", &[]).is_err());
    }

    #[tokio::test]
    async fn non_admin_with_valid_connection_key_cannot_manage_deployment() {
        let app = routes().with_state(state_with("eu", vec![]).await);
        let requests = [
            (
                "/api/ui/deploy/node",
                r#"{"group":"qwen35","target_node_id":"no-2","start_layer":0,"end_layer":15}"#,
                "only the administrator can add a machine to the deployment",
            ),
            (
                "/api/ui/undeploy",
                r#"{"group":"qwen35"}"#,
                "only the administrator can undeploy the deployment",
            ),
        ];

        for (path, payload, expected_error) in requests {
            let request = Request::post(path)
                .header("authorization", "Bearer chave")
                .header("content-type", "application/json")
                .body(Body::from(payload))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::FORBIDDEN,
                "{path}"
            );
            assert_eq!(body_of(response).await["error"], expected_error);
        }
    }

    #[tokio::test]
    async fn admin_with_valid_connection_key_passes_both_authority_gates() {
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        let app = routes().with_state(state);

        let add_request = Request::post("/api/ui/deploy/node")
            .header("authorization", "Bearer chave")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"group":"qwen35","target_node_id":"no-2","start_layer":15,"end_layer":0}"#,
            ))
            .unwrap();
        let add_response = app.clone().oneshot(add_request).await.unwrap();
        assert_eq!(add_response.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            body_of(add_response).await["error"],
            "inverted layer range"
        );

        let undeploy_request = Request::post("/api/ui/undeploy")
            .header("authorization", "Bearer chave")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"group":"qwen35"}"#))
            .unwrap();
        let undeploy_response = app.oneshot(undeploy_request).await.unwrap();
        assert_eq!(
            undeploy_response.status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        assert!(body_of(undeploy_response).await["error"]
            .as_str()
            .unwrap()
            .contains("no machine"));
    }

    #[tokio::test]
    async fn a_node_profile_can_still_remove_its_own_machine() {
        let state = state_with("eu", vec![peer("eu", "eu", 0, (0, 31))]).await;
        let response = remove_node_endpoint(
            ConnectionAuth,
            State(state),
            Json(RemoveNodeRequest {
                group: "qwen35".into(),
                target_node_id: "eu".into(),
                deploy_id: String::new(),
            }),
        )
        .await;

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(body_of(response).await["removed"], true);
    }

    #[tokio::test]
    async fn removing_one_machine_appends_a_version_and_keeps_the_deployment_serving() {
        let (port, _) = fake_machine(axum::http::StatusCode::OK).await;
        let mut target = peer("no-a", "ksia", port, (0, 15));
        target.deploy_id = "d-aaaaaaaa".into();
        let mut remaining = peer("eu", "eu", 0, (16, 31));
        remaining.deploy_id = "d-aaaaaaaa".into();
        let state = admin_state(vec![target, remaining]).await;
        state
            .database
            .save_deployment(&deployment(vec![
                deployment_node("no-a", 0, 15, 0, 2),
                deployment_node("eu", 16, 31, 1, 2),
            ]))
            .unwrap();

        let response = remove_node_endpoint(
            ConnectionAuth,
            State(state.clone()),
            Json(RemoveNodeRequest {
                group: "qwen35".into(),
                target_node_id: "no-a".into(),
                deploy_id: "d-aaaaaaaa".into(),
            }),
        )
        .await;
        let body = body_of(response).await;

        assert_eq!(body["removed"], true, "{body}");
        assert_eq!(body["serving"], true);
        assert_eq!(body["version"], 2);
        let saved = state.database.deployment("d-aaaaaaaa").unwrap().unwrap();
        assert_eq!(saved.active_version, 2);
        assert_eq!(saved.nodes.len(), 1);
        assert_eq!(saved.nodes[0].node_id, "eu");
        assert_eq!(saved.versions.len(), 2);
    }

    #[tokio::test]
    async fn removing_the_last_machine_appends_an_empty_inactive_version() {
        let mut local = peer("eu", "eu", 0, (0, 31));
        local.deploy_id = "d-aaaaaaaa".into();
        let state = admin_state(vec![local]).await;
        state
            .database
            .save_deployment(&deployment(vec![deployment_node("eu", 0, 31, 0, 1)]))
            .unwrap();

        let response = remove_node_endpoint(
            ConnectionAuth,
            State(state.clone()),
            Json(RemoveNodeRequest {
                group: "qwen35".into(),
                target_node_id: "eu".into(),
                deploy_id: "d-aaaaaaaa".into(),
            }),
        )
        .await;
        let body = body_of(response).await;

        assert_eq!(body["removed"], true, "{body}");
        assert_eq!(body["serving"], false);
        assert_eq!(body["version"], 2);
        let saved = state.database.deployment("d-aaaaaaaa").unwrap().unwrap();
        assert!(saved.nodes.is_empty());
        assert_eq!(saved.versions.len(), 2);
        assert!(crate::deployment::active(&[saved], "qwen35").is_none());
    }
}
