/// Tirar a pipeline inteira do ar.
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

    /// Um peer servindo `qwen35` na faixa dada.
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

    /// Uma máquina que responde `/api/ui/unload` com o status pedido, contando
    /// quantas vezes foi chamada.
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

    /// Duas implantações do mesmo modelo: derrubar uma tem que deixar a outra
    /// servindo.
    ///
    /// Antes do `deploy_id` isto era inexprimível nos dois sentidos - não dava
    /// para ter as duas no ar, e `resolve_group` só sabia falar de rótulos, de
    /// modo que um "desimplantar qwen35" teria levado as duas junto.
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

        assert_eq!(body["released"], 1, "só a implantação escolhida sai do ar");
        assert_eq!(
            body["results"].as_array().unwrap().len(),
            1,
            "a outra implantação nem aparece no relatório"
        );
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
        assert_eq!(
            calls_b.load(Ordering::SeqCst),
            0,
            "a máquina da outra implantação não pode ser tocada"
        );
    }

    /// Um id que ninguém serve não pode virar "derruba o rótulo inteiro".
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

    /// O relatório tem uma linha por máquina, e uma máquina que não confirmou
    /// **não** conta como liberada. Fingir sucesso aqui deixaria memória presa
    /// numa caixa que o operador acha que esvaziou.
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
        assert_eq!(results.len(), 2, "uma linha por máquina, sempre");
        // Ordenado pela camada inicial: o relatório se lê na ordem do pipeline.
        assert_eq!(results[0]["hostname"], "ksia");
        assert_eq!(results[0]["released"], true);
        assert!(results[0].get("error").is_none());

        assert_eq!(results[1]["hostname"], "macbook");
        assert_eq!(results[1]["released"], false);
        assert!(
            results[1]["error"].as_str().unwrap().contains("500"),
            "o motivo tem que ser o que a máquina disse: {}",
            results[1]["error"]
        );

        // As duas foram mesmo chamadas: nenhuma foi pulada por causa da outra.
        assert_eq!(ok_hits.load(Ordering::SeqCst), 1);
        assert_eq!(bad_hits.load(Ordering::SeqCst), 1);
    }

    /// Uma máquina fora do ar é falha com motivo, não uma linha que some.
    #[tokio::test]
    async fn an_unreachable_machine_still_gets_a_line() {
        let (ok_port, _) = fake_machine(axum::http::StatusCode::OK).await;
        // Porta que ninguém atende: reservada e imediatamente devolvida.
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

    /// Sem pipeline no ar não há o que desimplantar, e dizer isso é melhor que
    /// devolver um relatório vazio que se lê como sucesso.
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

    /// Qual pipeline derrubar: óbvio com uma, recusa com duas. Derrubar a
    /// errada custa uma reimplantação inteira, então o palpite não é nosso.
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
                "somente o administrador pode adicionar uma máquina à implantação",
            ),
            (
                "/api/ui/undeploy",
                r#"{"group":"qwen35"}"#,
                "somente o administrador pode desimplantar a implantação",
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
            "faixa de camadas invertida"
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
            .contains("nenhuma"));
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
