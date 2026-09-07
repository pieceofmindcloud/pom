/// Tirar a pipeline inteira do ar.
#[cfg(test)]
mod undeploy_tests {
    use super::*;
    use crate::config::Role;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::post as axum_post;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// Um peer servindo `qwen35` na faixa dada.
    fn peer(node_id: &str, host: &str, gateway_port: u16, range: (u32, u32)) -> PeerInfo {
        let mut p = crate::gateway::testkit::peer(node_id, host, gateway_port);
        p.model = "qwen35".into();
        p.start_layer = range.0;
        p.end_layer = range.1;
        p.total_layers = 32;
        p
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
        let (porta_a, chamadas_a) = fake_machine(axum::http::StatusCode::OK).await;
        let (porta_b, chamadas_b) = fake_machine(axum::http::StatusCode::OK).await;

        let mut a = peer("no-a", "maquina-a", porta_a, (0, 31));
        let mut b = peer("no-b", "maquina-b", porta_b, (0, 31));
        a.deploy_id = "d-aaaaaaaa".into();
        b.deploy_id = "d-bbbbbbbb".into();

        let state = state_with("eu", vec![a, b]).await;
        state.auth.write().await.role = Role::Principal;

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
        assert_eq!(chamadas_a.load(Ordering::SeqCst), 1);
        assert_eq!(
            chamadas_b.load(Ordering::SeqCst),
            0,
            "a máquina da outra implantação não pode ser tocada"
        );
    }

    /// Um id que ninguém serve não pode virar "derruba o rótulo inteiro".
    #[tokio::test]
    async fn an_unknown_deployment_id_releases_nothing() {
        let (porta, chamadas) = fake_machine(axum::http::StatusCode::OK).await;
        let mut a = peer("no-a", "maquina-a", porta, (0, 31));
        a.deploy_id = "d-aaaaaaaa".into();

        let state = state_with("eu", vec![a]).await;
        state.auth.write().await.role = Role::Principal;

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
        assert_eq!(chamadas.load(Ordering::SeqCst), 0);
    }

    /// O relatório tem uma linha por máquina, e uma máquina que não confirmou
    /// **não** conta como liberada. Fingir sucesso aqui deixaria memória presa
    /// numa caixa que o operador acha que esvaziou.
    #[tokio::test]
    async fn every_machine_is_reported_and_a_silent_one_is_not_a_success() {
        let (ok_port, ok_hits) = fake_machine(axum::http::StatusCode::OK).await;
        let (bad_port, bad_hits) =
            fake_machine(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let state = state_with(
            "eu",
            vec![
                peer("no-a", "ksia", ok_port, (0, 15)),
                peer("no-b", "macbook", bad_port, (16, 31)),
            ],
        )
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
        let state = state_with(
            "eu",
            vec![
                peer("no-a", "ksia", ok_port, (0, 15)),
                peer("no-b", "sumida", dead, (16, 31)),
            ],
        )
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
        let state = state_with("eu", vec![]).await;
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
        let uma = vec![peer("a", "h", 1, (0, 15)), peer("b", "h", 2, (16, 31))];
        assert_eq!(resolve_group("", &uma).unwrap(), "qwen35");
        assert_eq!(resolve_group("outro", &uma).unwrap(), "outro");

        let mut duas = uma.clone();
        duas[1].model = "llama3".into();
        let erro = resolve_group("", &duas).unwrap_err();
        assert!(erro.contains("llama3") && erro.contains("qwen35"), "{erro}");

        assert!(resolve_group("", &[]).is_err());
    }

    /// **Documenta a porta aberta de propósito.** Nesta primeira versão qualquer
    /// nó desimplanta; quando a restrição "somente o nó principal" entrar em
    /// `undeploy_authority`, é este teste que tem que mudar junto - e é isso que
    /// torna o fechamento uma decisão, e não um efeito colateral.
    #[test]
    fn any_node_may_undeploy_for_now() {
        assert!(undeploy_authority(&Role::Node).is_ok());
        assert!(undeploy_authority(&Role::Principal).is_ok());
    }
}

