/// A visão do cluster e o que a tela de implantações precisa saber.
#[cfg(test)]
mod deployments_tests {
    use super::*;
    use crate::deployment::{Deployment, DeploymentKind, DeploymentModel, DeploymentNode};
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::get as axum_get;

    fn no(id: &str, inicio: u32, fim: u32) -> DeploymentNode {
        DeploymentNode {
            node_id: id.into(),
            hostname: id.into(),
            start_layer: inicio,
            end_layer: fim,
            peer_index: 0,
            peer_count: 1,
            primary: true,
            part_sha256: None,
            source_model_path: None,
            tp_rank: None,
            config: RuntimeParams::default(),
        }
    }

    fn registro(id: &str, rotulo: &str, ativado: &str) -> Deployment {
        let mut d = Deployment::new(
            id.into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: rotulo.into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            vec![no("n1", 0, 31)],
        );
        d.activated_at = ativado.into();
        d
    }

    /// Um peer que responde `/api/ui/deployments?local=true` com os registros dados.
    async fn peer_with_records(registros: Vec<Deployment>) -> u16 {
        let corpo = json!({ "node_id": "outro", "deployments": registros });
        let app = Router::new().route(
            "/api/ui/deployments",
            axum_get(move || {
                let corpo = corpo.clone();
                async move { Json(corpo) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// O registro tem uma cópia por nó participante, então a listagem junta e
    /// **deduplica** - senão uma implantação de três nós apareceria três vezes.
    /// Empate desfeito pela ativação mais recente: um nó que estava fora do ar
    /// durante um `activate` tem a cópia velha, e ela não pode ganhar.
    #[tokio::test]
    async fn copies_of_one_deployment_collapse_to_the_newest() {
        let velha = registro("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        let nova = registro("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");

        let porta = peer_with_records(vec![nova.clone()]).await;
        let mut peer = crate::gateway::testkit::peer("outro", "outro", porta);
        peer.model = "qwen3:8b".into();
        peer.deploy_id = "d-aaaaaaaa".into();
        peer.total_layers = 32;

        let state = state_with("eu", vec![peer]).await;
        let _ = velha; // a cópia velha vive no disco de quem não respondeu

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: false }),
        )
        .await;
        let body = body_of(resp).await;

        let linhas = body["deployments"].as_array().unwrap();
        assert_eq!(linhas.len(), 1, "uma implantação, uma linha");
        assert_eq!(linhas[0]["deployment"]["deploy_id"], "d-aaaaaaaa");
        assert_eq!(
            linhas[0]["deployment"]["activated_at"], "2026-08-19T00:00:00Z",
            "a cópia mais recentemente ativada é a que vale"
        );
        assert_eq!(linhas[0]["serving"], true, "há peer vivo anunciando o id");
        assert_eq!(linhas[0]["is_default"], true, "é a única do seu modelo");
    }

    /// O coordenador pode ter uma cópia velha no próprio banco enquanto um
    /// peer participante já recebeu a versão corrente. Operações dirigidas a
    /// um `deploy_id` precisam consultar a história mais nova, não parar na
    /// primeira cópia encontrada.
    #[tokio::test]
    async fn find_record_chooses_the_current_copy_over_local_history() {
        let mut velha = registro("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        velha.versions[0].params = params_com_ctx(4096);

        let mut corrente = registro("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");
        corrente.versions[0].params = params_com_ctx(8192);
        corrente.upsert_version(params_com_ctx(16384), corrente.nodes.clone(), "outro");
        corrente.activated_at = "2026-08-19T00:00:00Z".into();

        let porta = peer_with_records(vec![corrente.clone()]).await;
        let peer = crate::gateway::testkit::peer("outro", "outro", porta);
        let state = state_with("eu", vec![peer]).await;
        state.database.save_deployment(&velha).unwrap();

        let encontrada = find_record(&state, "d-aaaaaaaa")
            .await
            .expect("a cópia do peer deve ser encontrada");

        assert_eq!(encontrada.activated_at, corrente.activated_at);
        assert_eq!(encontrada.active_version, 2);
        assert_eq!(encontrada.versions.len(), 2);
    }

    /// "No ar" e "padrão" são coisas diferentes, e a tela não pode confundir:
    /// duas implantações do mesmo modelo podem estar **as duas** servindo, mas
    /// só uma atende quem pede o nome pelado.
    #[tokio::test]
    async fn serving_and_default_are_not_the_same_thing() {
        let antiga = registro("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        let recente = registro("d-bbbbbbbb", "qwen3:8b", "2026-08-19T00:00:00Z");

        let porta = peer_with_records(vec![antiga, recente]).await;
        let mut p1 = crate::gateway::testkit::peer("outro", "outro", porta);
        p1.model = "qwen3:8b".into();
        p1.deploy_id = "d-aaaaaaaa".into();
        p1.total_layers = 32;
        // Só a `d-aaaaaaaa` tem peer vivo; a `d-bbbbbbbb` está registrada mas
        // ninguém a está servindo.
        let state = state_with("eu", vec![p1]).await;

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: false }),
        )
        .await;
        let body = body_of(resp).await;
        let linhas = body["deployments"].as_array().unwrap();
        assert_eq!(linhas.len(), 2);

        let por_id = |id: &str| -> serde_json::Value {
            linhas
                .iter()
                .find(|l| l["deployment"]["deploy_id"] == id)
                .cloned()
                .unwrap()
        };

        let a = por_id("d-aaaaaaaa");
        assert_eq!(a["serving"], true, "tem peer anunciando");
        assert_eq!(a["is_default"], false, "mas não é a mais recente");

        let b = por_id("d-bbbbbbbb");
        assert_eq!(b["serving"], false, "registrada, mas ninguém no ar");
        assert_eq!(b["is_default"], true, "é a de ativação mais recente");
    }

    /// `?local=true` responde do disco e **não** varre o cluster - é o que
    /// impede a varredura de se chamar em círculo, um nó perguntando ao outro
    /// que pergunta de volta.
    #[tokio::test]
    async fn the_local_answer_does_not_fan_out() {
        let porta = peer_with_records(vec![registro(
            "d-cccccccc",
            "qwen3:8b",
            "2026-08-19T00:00:00Z",
        )])
        .await;
        let peer = crate::gateway::testkit::peer("outro", "outro", porta);
        let state = state_with("eu", vec![peer]).await;

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: true }),
        )
        .await;
        let body = body_of(resp).await;

        // O registro do peer não aparece: esta resposta é só do disco local.
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

    fn params_com_ctx(n_ctx: u32) -> RuntimeParams {
        RuntimeParams {
            n_ctx,
            ..Default::default()
        }
    }

    /// `resolve` com `version` reativa uma versão registrada sem anexar outra;
    /// com `params` novos, anexa. Sem nenhum dos dois, recusa.
    #[test]
    fn redeploy_request_resolve_escolhe_ou_cria_versao() {
        let mut d = registro("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");
        d.versions[0].params = params_com_ctx(8192);
        d.upsert_version(params_com_ctx(16384), d.nodes.clone(), "n1");
        assert_eq!(d.active_version, 2);

        // version -> volta para a 1, sem anexar.
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

        // params novos -> v3.
        let base = RedeployRequest {
            params: Some(params_com_ctx(4096)),
            version: None,
            per_node: HashMap::new(),
        }
        .resolve(&mut d, "eu")
        .unwrap();
        assert_eq!(base.n_ctx, 4096);
        assert_eq!(d.versions.len(), 3);
        assert_eq!(d.active_version, 3);

        // nem version nem params -> erro.
        assert!(RedeployRequest {
            params: None,
            version: None,
            per_node: HashMap::new(),
        }
        .resolve(&mut d, "eu")
        .is_err());
    }

    /// Pedir o redeploy de uma versão que não existe é recusado antes de
    /// derrubar máquina nenhuma.
    #[tokio::test]
    async fn redeploy_de_versao_inexistente_e_recusado() {
        let state = state_with("eu", vec![]).await;
        let d = registro("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");
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
