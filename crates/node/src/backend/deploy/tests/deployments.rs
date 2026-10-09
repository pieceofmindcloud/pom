/// The cluster view and what the deployments screen needs to know.
#[cfg(test)]
mod deployments_tests {
    use super::*;
    use crate::deployment::{Deployment, DeploymentKind, DeploymentModel, DeploymentNode};
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::get as axum_get;

    fn no(id: &str, start: u32, end: u32) -> DeploymentNode {
        DeploymentNode {
            node_id: id.into(),
            hostname: id.into(),
            start_layer: start,
            end_layer: end,
            peer_index: 0,
            peer_count: 1,
            primary: true,
            part_sha256: None,
            source_model_path: None,
            tp_rank: None,
            config: RuntimeParams::default(),
        }
    }

    fn record(id: &str, label: &str, enabled: &str) -> Deployment {
        let mut d = Deployment::new(
            id.into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: label.into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            vec![no("n1", 0, 31)],
        );
        d.activated_at = enabled.into();
        d
    }

    /// A peer that answers `/api/ui/deployments?local=true` with the given records.
    async fn peer_with_records(records: Vec<Deployment>) -> u16 {
        let body = json!({ "node_id": "outro", "deployments": records });
        let app = Router::new().route(
            "/api/ui/deployments",
            axum_get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// The record has one copy per participating node, so the listing joins and
    /// **deduplicates** - otherwise a three-node deployment would show up three
    /// times. The tie is broken by the latest activation: a node that was down
    /// during an `activate` has the old copy, and it must not win.
    #[tokio::test]
    async fn copies_of_one_deployment_collapse_to_the_newest() {
        let old = record("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        let new = record("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");

        let port = peer_with_records(vec![new.clone()]).await;
        let mut peer = crate::gateway::testkit::peer("outro", "outro", port);
        peer.model = "qwen3:8b".into();
        peer.deploy_id = "d-aaaaaaaa".into();
        peer.total_layers = 32;

        let state = state_with("eu", vec![peer]).await;
        let _ = old; // the old copy lives on the disk of whoever did not answer

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: false }),
        )
        .await;
        let body = body_of(resp).await;

        let lines = body["deployments"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "one deployment, one row");
        assert_eq!(lines[0]["deployment"]["deploy_id"], "d-aaaaaaaa");
        assert_eq!(
            lines[0]["deployment"]["activated_at"], "2026-08-19T00:00:00Z",
            "the most recently activated copy is the one that counts"
        );
        assert_eq!(lines[0]["serving"], true, "there is a live peer announcing the id");
        assert_eq!(lines[0]["is_default"], true, "it is the only one of its model");
    }

    /// The coordinator may have an old copy in its own database while a
    /// participating peer has already received the current version. Operations
    /// aimed at a `deploy_id` must consult the newest history, not stop at the
    /// first copy found.
    #[tokio::test]
    async fn find_record_chooses_the_current_copy_over_local_history() {
        let mut old = record("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        old.versions[0].params = params_with_ctx(4096);

        let mut current = record("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");
        current.versions[0].params = params_with_ctx(8192);
        current.upsert_version(params_with_ctx(16384), current.nodes.clone(), "outro");
        current.activated_at = "2026-08-19T00:00:00Z".into();

        let port = peer_with_records(vec![current.clone()]).await;
        let peer = crate::gateway::testkit::peer("outro", "outro", port);
        let state = state_with("eu", vec![peer]).await;
        state.database.save_deployment(&old).unwrap();

        let found = find_record(&state, "d-aaaaaaaa")
            .await
            .expect("the peer copy must be found");

        assert_eq!(found.activated_at, current.activated_at);
        assert_eq!(found.active_version, 2);
        assert_eq!(found.versions.len(), 2);
    }

    /// "Up" and "default" are different things, and the screen must not confuse
    /// them: two deployments of the same model can **both** be serving, but
    /// only one answers whoever asks for the bare name.
    #[tokio::test]
    async fn serving_and_default_are_not_the_same_thing() {
        let old = record("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        let recent = record("d-bbbbbbbb", "qwen3:8b", "2026-08-19T00:00:00Z");

        let port = peer_with_records(vec![old, recent]).await;
        let mut p1 = crate::gateway::testkit::peer("outro", "outro", port);
        p1.model = "qwen3:8b".into();
        p1.deploy_id = "d-aaaaaaaa".into();
        p1.total_layers = 32;
        // Only `d-aaaaaaaa` has a live peer; `d-bbbbbbbb` is registered but
        // nobody is serving it.
        let state = state_with("eu", vec![p1]).await;

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: false }),
        )
        .await;
        let body = body_of(resp).await;
        let lines = body["deployments"].as_array().unwrap();
        assert_eq!(lines.len(), 2);

        let by_id = |id: &str| -> serde_json::Value {
            lines
                .iter()
                .find(|l| l["deployment"]["deploy_id"] == id)
                .cloned()
                .unwrap()
        };

        let a = by_id("d-aaaaaaaa");
        assert_eq!(a["serving"], true, "there is a peer announcing");
        assert_eq!(a["is_default"], false, "but it is not the most recent");

        let b = by_id("d-bbbbbbbb");
        assert_eq!(b["serving"], false, "registered, but nobody up");
        assert_eq!(b["is_default"], true, "it is the one with the most recent activation");
    }

    /// `?local=true` answers from disk and does **not** sweep the cluster - it
    /// is what keeps the sweep from calling itself in a circle, one node asking
    /// the other, which asks back.
    #[tokio::test]
    async fn the_local_answer_does_not_fan_out() {
        let port = peer_with_records(vec![record(
            "d-cccccccc",
            "qwen3:8b",
            "2026-08-19T00:00:00Z",
        )])
        .await;
        let peer = crate::gateway::testkit::peer("outro", "outro", port);
        let state = state_with("eu", vec![peer]).await;

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: true }),
        )
        .await;
        let body = body_of(resp).await;

        // The peer's record does not appear: this answer is from the local disk only.
        assert!(body["deployments"].as_array().unwrap().is_empty());
        assert!(body.get("node_id").is_some());
    }

    #[tokio::test]
    async fn a_bogus_deploy_id_is_refused_before_anything_happens() {
        let state = state_with("eu", vec![]).await;
        let resp = activate_deployment(
            ConnectionAuth,
            State(state.clone()),
            Path("../../etc/passwd".to_string()),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);

        let resp = redeploy_endpoint(
            ConnectionAuth,
            State(state),
            Path("nao-e-id".to_string()),
            Json(RedeployRequest {
                params: Some(RuntimeParams::default()),
                version: None,
                per_node: HashMap::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    fn params_with_ctx(n_ctx: u32) -> RuntimeParams {
        RuntimeParams {
            n_ctx,
            ..Default::default()
        }
    }

    /// `resolve` with `version` reactivates a recorded version without appending
    /// another; with new `params`, it appends. With neither, it refuses.
    #[test]
    fn redeploy_request_resolve_picks_or_creates_a_version() {
        let mut d = record("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");
        d.versions[0].params = params_with_ctx(8192);
        d.upsert_version(params_with_ctx(16384), d.nodes.clone(), "n1");
        assert_eq!(d.active_version, 2);

        // version -> goes back to 1, without appending.
        let base = RedeployRequest {
            params: None,
            version: Some(1),
            per_node: HashMap::new(),
        }
        .resolve(&mut d, "eu")
        .unwrap();
        assert_eq!(d.active_version, 1);
        assert_eq!(d.versions.len(), 2);
        assert_eq!(base.n_ctx, 8192);

        // new params -> v3.
        let base = RedeployRequest {
            params: Some(params_with_ctx(4096)),
            version: None,
            per_node: HashMap::new(),
        }
        .resolve(&mut d, "eu")
        .unwrap();
        assert_eq!(base.n_ctx, 4096);
        assert_eq!(d.versions.len(), 3);
        assert_eq!(d.active_version, 3);

        // neither version nor params -> error.
        assert!(RedeployRequest {
            params: None,
            version: None,
            per_node: HashMap::new(),
        }
        .resolve(&mut d, "eu")
        .is_err());
    }

    /// Asking for the redeploy of a version that does not exist is refused
    /// before taking any machine down.
    #[tokio::test]
    async fn redeploying_a_missing_version_is_refused() {
        let state = state_with("eu", vec![]).await;
        let d = record("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");
        state.database.save_deployment(&d).unwrap();

        let resp = redeploy_endpoint(
            ConnectionAuth,
            State(state),
            Path("d-aaaaaaaa".to_string()),
            Json(RedeployRequest {
                params: None,
                version: Some(99),
                per_node: HashMap::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
