/// Um endereço anunciado que deixou de responder.
///
/// O defeito que estes testes travam: enquanto `gateway_url()` era só
/// `peer.addr`, o plano de controle tinha **um** endereço e nenhuma memória de
/// falha. Um IPv6 temporário que roda (macOS gera um novo e aposenta o antigo
/// sozinho) deixava o Painel a discar um endereço que não existe em máquina
/// nenhuma, para sempre, com a mensagem "nada abaixo pode ser lido" - enquanto
/// o mesmo peer estava vivo e anunciado noutro endereço, já dentro de
/// `addr_candidates`.
#[cfg(test)]
mod dead_address_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};

    /// Um `/api/ui/node` de verdade na loopback, que conta quantas vezes foi
    /// lido. Devolve `(porta, contador)`.
    async fn live_node(hostname: &str) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let contador = hits.clone();
        let nome = hostname.to_string();
        let app = Router::new().route(
            "/api/ui/node",
            get(move || {
                let nome = nome.clone();
                contador.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Json(json!({ "hostname": nome, "node_id": "outro" })) }
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

    /// `127.0.0.2` na mesma porta é o endereço morto: no macOS o SYN some
    /// (nenhum alias na lo0), no Linux ele é recusado. Os dois chegam ao mesmo
    /// lugar - erro de transporte - que é o que um endereço aposentado produz.
    fn dead_host() -> &'static str {
        "127.0.0.2"
    }

    fn peer_com_endereco_morto(port: u16) -> PeerInfo {
        let mut p = peer("outro", "macbook", port);
        // O que o cluster guardou: o endereço morto na frente, porque é o que
        // o peer anunciou quando ainda era o dele.
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

    /// O teste do defeito: uma única leitura do Painel já recupera o peer pelo
    /// endereço vivo, **sem intervenção manual**, e o endereço morto fica
    /// registado como tal para não voltar a ser discado.
    #[tokio::test]
    async fn a_dead_announced_address_is_forgotten_and_the_peer_is_read_at_the_new_one() {
        let (port, hits) = live_node("macbook").await;
        let p = peer_com_endereco_morto(port);
        let morto = format!("http://{}:{port}", dead_host());
        let vivo = format!("http://127.0.0.1:{port}");
        let state = state_with("eu", vec![p]).await;

        let body = body_of(cluster(HeaderMap::new(), State(state.clone())).await).await;
        let outro = body["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["node_id"] == "outro")
            .expect("o peer tem de aparecer no cluster");

        assert_eq!(
            outro["node"]["hostname"], "macbook",
            "a leitura tinha de cair no endereço vivo em vez de morrer no primeiro"
        );
        assert!(
            outro["unreachable"].is_null(),
            "o aviso permanente do Painel não pode sobreviver a um peer legível: {}",
            outro["unreachable"]
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        let routes = &state.network.routes;
        assert!(
            routes.is_failed("outro", &morto),
            "o endereço que não respondeu tem de ficar na memória de falha"
        );
        assert!(
            !routes.is_failed("outro", &vivo),
            "o endereço que respondeu não pode ficar desacreditado"
        );
        let candidatos = state
            .all_peers()
            .await
            .into_iter()
            .find(|p| p.node_id == "outro")
            .unwrap()
            .gateway_candidates();
        assert_eq!(
            routes.usable("outro", &candidatos),
            vec![vivo],
            "a leitura seguinte não pode voltar a pagar o timeout do endereço morto"
        );
    }

    /// Quando **todo** endereço conhecido morre, o nó para de insistir e passa
    /// a reencontrar o par. A tela precisa disso: "reencontrando" é trabalho em
    /// curso, "não dá para ler" é beco sem saída.
    #[tokio::test]
    async fn when_every_address_is_dead_the_node_asks_for_the_current_one() {
        let (port, _) = live_node("macbook").await;
        let mut p = peer_com_endereco_morto(port);
        p.addr_candidates = vec![format!("{}:{port}", dead_host())];
        let state = state_with("eu", vec![p]).await;

        // Primeira leitura: descobre que o único endereço não responde.
        let body = body_of(cluster(HeaderMap::new(), State(state.clone())).await).await;
        let outro = body["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["node_id"] == "outro")
            .unwrap()
            .clone();
        assert_eq!(
            outro["rediscovering"], true,
            "sem endereço utilizável, o Painel mostra o nó a ser reencontrado"
        );
        assert!(
            outro["unreachable"].is_null(),
            "reencontrando não é inalcançável: o utilizador não pode ler um beco sem saída"
        );
        assert!(
            state.network.is_rediscovering("outro"),
            "o nó tem de ter pedido o endereço atual do par"
        );

        // E o cadáver saiu da entrada local, para que o próximo `Announce` do
        // peer a repovoe em vez de somar-se a ele.
        let guardados = state.network.peers.read().await["outro"]
            .addr_candidates
            .clone();
        assert!(
            guardados.is_empty(),
            "o candidato desacreditado tinha de sair da entrada: {guardados:?}"
        );
    }

    /// "Reencontrando" tem prazo. Três perguntas à rede sem resposta não são
    /// mais trabalho em curso: são um par que anuncia por UDP e não atende no
    /// HTTP. Insistir na palavra seria repor o aviso desonesto que este
    /// trabalho veio remover, só que otimista em vez de pessimista.
    #[tokio::test]
    async fn rediscovering_gives_up_and_says_so_instead_of_promising_forever() {
        let (port, _) = live_node("macbook").await;
        let mut p = peer_com_endereco_morto(port);
        p.addr_candidates = vec![format!("{}:{port}", dead_host())];
        let state = state_with("eu", vec![p]).await;

        let ler = |state: GatewayState| async move {
            body_of(cluster(HeaderMap::new(), State(state)).await)
                .await["peers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["node_id"] == "outro")
                .unwrap()
                .clone()
        };

        assert_eq!(ler(state.clone()).await["rediscovering"], true);

        // Envelhecer a busca para além do prazo, sem esperar em tempo real.
        state
            .network
            .age_rediscovery_for_test("outro", crate::network::REDISCOVERY_GIVE_UP);

        let desistiu = ler(state.clone()).await;
        assert_eq!(
            desistiu["rediscovering"], false,
            "passado o prazo, a tela não pode continuar prometendo que reencontra"
        );
        assert!(
            desistiu["unreachable"]
                .as_str()
                .is_some_and(|erro| erro.contains("endereços conhecidos")),
            "e tem de dizer por quê: {}",
            desistiu["unreachable"]
        );
    }

    /// A cobertura sai do gossip, não da leitura. Um par ilegível continua a
    /// contribuir camadas - e é honesto que continue, porque o servidor de
    /// inferência dele pode estar perfeito e só o endereço de controle é que
    /// morreu - mas isso não pode ficar invisível.
    #[tokio::test]
    async fn an_unreadable_peer_cannot_pass_for_healthy_coverage() {
        let (port, _) = live_node("macbook").await;
        let mut p = peer_com_endereco_morto(port);
        p.addr_candidates = vec![format!("{}:{port}", dead_host())];
        let state = state_with("eu", vec![p]).await;

        let body = body_of(cluster(HeaderMap::new(), State(state)).await).await;
        let qwen = body["model_inventory"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["model_id"] == "qwen")
            .expect("o modelo anunciado entra no inventário");
        assert_eq!(
            qwen["covered_layers"], 16,
            "as camadas continuam anunciadas; retirá-las seria mentir na outra direção"
        );
        assert_eq!(
            qwen["unreadable_nodes"],
            json!(["outro"]),
            "a cobertura tem de dizer de quem ela depende sem conseguir ler"
        );
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
        let peers = body["peers"].as_array().expect("peers precisa ser uma lista");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["node_id"], "eu");
        assert!(
            !peers.iter().any(|peer| peer["node_id"] == "outro"),
            "infer não pode receber dados de outro nó"
        );
    }
}
