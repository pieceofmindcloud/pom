#[cfg(test)]
mod delete_tests {
    use super::*;

    /// O aviso do store do Ollama existe porque apagar um blob de lá deixa o
    /// manifesto apontando para nada — o store fica quebrado, e ele não é
    /// nosso para consertar.
    #[test]
    fn an_ollama_blob_is_recognised_by_its_layout() {
        let root = std::env::temp_dir().join(format!("pom-del-{}", std::process::id()));
        let blobs = root.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        let blob = blobs.join("sha256-abc");
        std::fs::write(&blob, b"GGUF").unwrap();

        assert!(is_ollama_blob(&blob));

        // Um `blobs/` sem `manifests/` ao lado não é o store do Ollama — o
        // LM Studio também tem uma pasta com esse nome, e vazia.
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

/// "Sair da rede" no card de uma máquina.
#[cfg(test)]
mod leave_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};
    use axum::routing::post as axum_post;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Uma máquina que atende `/api/ui/leave` e conta os pedidos.
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

    /// Um estado de teste que **tem** autoridade administrativa. Repassar o
    /// "sair da rede" a outra máquina é ato de administrador; sair sozinho não
    /// é, e por isso os testes de abdicação usam o estado normal.
    async fn admin_state_with(node_id: &str, peers: Vec<PeerInfo>) -> GatewayState {
        let state = state_with(node_id, peers).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        state
    }

    /// O card de um peer manda o nó local repassar: o painel só fala com o
    /// próprio nó, e quem se desliga é sempre o dono daquele processo.
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

        // E este nó continua na rede: o pedido era sobre a outra máquina.
        assert!(
            !state.network.is_detached(),
            "repassar não pode desligar quem repassou"
        );
    }

    /// Corpo ausente e o próprio `node_id` são a mesma coisa: este nó sai. É o
    /// caminho que o menu "Este nó" e a tela de Rede sempre usaram.
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

    /// Uma máquina que não responde é recusa com motivo, não um "saiu" falso.
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

        // Peer que nem existe na tabela também não vira sucesso.
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

    /// Sair é abdicar do próprio posto: o `infer` e o `node` desligam a
    /// própria máquina sem licença de ninguém. Só o alvo diferente de si é que
    /// pede autoridade.
    #[tokio::test]
    async fn any_profile_detaches_its_own_machine() {
        for profile in [
            crate::profile::NodeProfile::Infer,
            crate::profile::NodeProfile::Node,
        ] {
            let state = state_with("eu", vec![]).await;
            state.auth.write().await.profile = profile;
            assert!(
                state.runtime.config().await.is_none(),
                "o teste começa sem modelo"
            );
            let resp = leave_network(ConnectionAuth, State(state.clone()), None).await;
            assert_eq!(resp.status(), StatusCode::OK, "perfil={profile}");
            assert!(state.network.is_detached(), "perfil={profile}");
            assert_eq!(
                state.runtime.status().await.state,
                crate::runtime::ServerState::Disabled,
                "sair não pode depender de um servidor Ready, perfil={profile}"
            );
        }
    }
}

#[cfg(test)]
mod catalog_under_tls_tests {
    use super::*;
    use crate::gateway::testkit::state_with;
    use crate::network::PeerInfo;

    /// Porta 1, fechada de proposito: o que se afirma aqui nao e que a chamada
    /// deu certo, e sim **por qual cliente** ela saiu.
    fn peer_with_gateway(node_id: &str) -> PeerInfo {
        crate::gateway::testkit::peer(node_id, node_id, 1)
    }

    /// A tela de implantar lista as fatias perguntando a cada peer, e essas
    /// chamadas tem que sair pelo cliente com pinning.
    ///
    /// Este teste existe por um defeito real: o plano de controle continuou
    /// usando o cliente compartilhado e sem pinning depois do TLS entrar, e com
    /// a politica ligada **toda** consulta a peer falhava calada - a tela dizia
    /// "nenhuma maquina do cluster tem fatias de <modelo>" e nao havia como
    /// implantar nada. O cache por peer so ganha entrada quando alguem pediu um
    /// cliente pinado, entao ele e a prova de que o caminho certo foi usado.
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
            "a consulta de catalogo saiu por um cliente sem pinning"
        );
        state.peer_http.apply_cluster_policy(false);
    }

    /// Mesma prova para o levantamento da tela de implantar (`deploy::survey`),
    /// que e por onde o defeito apareceu para o operador.
    #[tokio::test]
    async fn the_machine_list_for_deploying_is_pinned_too() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let state = state_with("eu", vec![peer_with_gateway("outro")]).await;
        state.peer_http.apply_cluster_policy(true);

        let _ = crate::deploy::survey_for_tests(&state, "qwen35").await;

        // O proprio no tambem entra: o levantamento pergunta o catalogo dele
        // pela mesma rota HTTP, e sob TLS isso tambem precisa de certificado
        // verificado. O que importa e o peer estar la.
        assert!(
            state
                .peer_http
                .cached_peers()
                .contains(&"outro".to_string()),
            "o levantamento saiu por um cliente sem pinning: {:?}",
            state.peer_http.cached_peers()
        );
        state.peer_http.apply_cluster_policy(false);
    }
}

#[cfg(test)]
mod tls_settings_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

    /// Só o principal decide. A checagem é **no servidor**: esconder o botão na
    /// tela é conveniência, não controle.
    #[tokio::test]
    async fn a_regular_node_cannot_turn_tls_on() {
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Node;

        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest { tls_required: true }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            !state.auth.read().await.tls_required,
            "403 não pode ter gravado nada"
        );
        assert!(!state.network.tls_enabled());
    }

    /// No principal, virar a flag vale na hora: config gravada, anúncio novo, e
    /// os clientes trocados. Sem reinício, e nos dois sentidos.
    #[tokio::test]
    async fn the_principal_turns_it_on_and_off_without_a_restart() {
        // Este teste mexe no esquema global das URLs; ver a nota em
        // `peer_client::GLOBAL_POLICY`.
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;

        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest { tls_required: true }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_of(resp).await["tls_required"], true);
        assert!(state.auth.read().await.tls_required);
        assert!(
            state.network.tls_enabled(),
            "o anúncio precisa dizer a verdade"
        );
        assert!(state.peer_http.tls_enabled());

        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest {
                tls_required: false,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!state.auth.read().await.tls_required);
        assert!(!state.network.tls_enabled());
        assert!(!state.peer_http.tls_enabled());
    }

    /// A tela precisa de duas coisas diferentes: se está ligado, e se **este**
    /// nó pode mexer.
    #[tokio::test]
    async fn node_info_reports_state_and_authority() {
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Node;
        let info = node_info(State(state.clone())).await;
        assert!(!info.0.tls_required);
        assert!(!info.0.tls_authority, "nó comum não decide");

        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        state.auth.write().await.tls_required = true;
        let info = node_info(State(state)).await;
        assert!(info.0.tls_required);
        assert!(info.0.tls_authority);
    }
}

#[cfg(test)]
mod cluster_logs_settings_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

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
    async fn the_principal_without_the_logs_plugin_cannot_turn_collection_on_but_can_turn_it_off() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        state.network.set_cluster_logs_enabled(true);
        state.cluster_logs.set_enabled(true);
        state.auth.write().await.cluster_logs_enabled = true;

        let response = save_cluster_logs(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveClusterLogsRequest { logs_enabled: true }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_of(response).await["code"], "logs_plugin_required");

        let response = save_cluster_logs(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveClusterLogsRequest { logs_enabled: false }),
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
            axum_post(|| async {
                Json(json!({ "network_chat_enabled": true }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let primary_key = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let mut primary = peer("primary", "primary", port);
        primary.libp2p_peer_id = Some("peer-primary".into());
        primary.pubkey =
            hex::encode(primary_key.verifying_key().as_bytes());
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
            "o secundário não grava a política localmente"
        );
    }
}

#[cfg(test)]
mod advertise_addr_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

    /// A resposta já traz o efetivo, para a tela confirmar o efeito sem uma
    /// segunda volta — é o próprio motivo de existir deste campo na resposta.
    #[tokio::test]
    async fn pinning_an_address_takes_effect_immediately_and_live() {
        let state = state_with("eu", vec![]).await;

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "203.0.113.9:42777".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = body_of(resp).await;
        assert_eq!(body["advertise_addr"], "203.0.113.9:42777");
        assert_eq!(body["effective_address"], "203.0.113.9:42777");

        // Live: o mesmo `Network` já anuncia o valor novo, sem reinício.
        assert_eq!(state.network.announce_addr(), "203.0.113.9:42777");
        // E persistido: uma releitura do config em disco vê o mesmo valor.
        assert_eq!(
            state.auth.read().await.advertise_addr.as_deref(),
            Some("203.0.113.9:42777")
        );
    }

    /// String vazia limpa o pino, mesma convenção de `SavePathsRequest.shard_dir`.
    #[tokio::test]
    async fn an_empty_value_clears_the_pin() {
        let state = state_with("eu", vec![]).await;
        state.network.set_advertise_addr(Some("203.0.113.9:42777"));

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(state.auth.read().await.advertise_addr, None);
    }

    /// Um valor sem forma reconhecível é recusado, não gravado quebrado —
    /// mesmo espírito de `check_dir` para pastas de modelo.
    #[tokio::test]
    async fn a_malformed_value_is_rejected_not_saved() {
        let state = state_with("eu", vec![]).await;

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "[not-a-valid-ipv6]".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(state.auth.read().await.advertise_addr, None);
    }
}
