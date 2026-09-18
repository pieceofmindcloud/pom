#[cfg(test)]
use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn inference_auth_probe(_: crate::auth::ApiKeyAuth) -> StatusCode {
        StatusCode::OK
    }

    #[tokio::test]
    async fn inference_routes_are_api_key_only_for_generate_and_v1() {
        let state = testkit::state_with("connection-secret", vec![]).await;
        state
            .database
            .insert_api_key(&crate::config::ApiKey {
                id: "ui-key".into(),
                key: "sk-active".into(),
                label: "pom-eu".into(),
                node_id: "eu".into(),
                origin_node_id: "eu".into(),
                active: true,
                created_at: "2026-09-07T00:00:00Z".into(),
            })
            .unwrap();
        let app = Router::new()
            .route("/api/generate", post(inference_auth_probe))
            .route("/v1/models", get(inference_auth_probe))
            .route("/v1/chat/completions", post(inference_auth_probe))
            .route("/v1/completions", post(inference_auth_probe))
            .route("/v1/messages", post(inference_auth_probe))
            .with_state(state);

        for (path, method) in [
            ("/api/generate", "POST"),
            ("/v1/models", "GET"),
            ("/v1/chat/completions", "POST"),
            ("/v1/completions", "POST"),
            ("/v1/messages", "POST"),
        ] {
            let absent = Request::builder()
                .method(method)
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(absent).await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );

            let connection = Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", "Bearer connection-secret")
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(connection).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );

            let active = Request::builder()
                .method(method)
                .uri(path)
                .header("x-api-key", "sk-active")
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(active).await.unwrap().status(),
                StatusCode::OK
            );

            let independent = Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", "Bearer stale-api-key")
                .header("x-api-key", "sk-active")
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(independent).await.unwrap().status(),
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn relay_result_ticket_is_sufficient_without_a_client_api_key() {
        let state = testkit::state_with("gateway", vec![]).await;
        let ticket = state.reply_inbox.register();
        let body = crate::wire::encode(
            &json!({"ticket": ticket.id(), "token_id": 1234, "done": true}),
            None,
            crate::wire::WireDtype::F16,
        )
        .unwrap();
        let app = Router::new()
            .route(crate::direct::RELAY_RESULT_PATH, post(handle_relay_result))
            .with_state(state);

        let response = app
            .oneshot(
                Request::post(crate::direct::RELAY_RESULT_PATH)
                    .header("content-type", "application/octet-stream")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn remote_stream_result_ticket_is_sufficient_without_a_client_api_key() {
        let state = testkit::state_with("gateway", vec![]).await;
        let mut ticket = state.reply_inbox.register_stream();
        let body = crate::wire::encode(
            &json!({
                "ticket": ticket.id(),
                "accepted": [{"token_id": 1234}],
                "done": true,
            }),
            None,
            crate::wire::WireDtype::F16,
        )
        .unwrap();
        let app = Router::new()
            .route(
                crate::direct::REMOTE_STREAM_RESULT_PATH,
                post(handle_remote_stream_result),
            )
            .with_state(state);

        let response = app
            .oneshot(
                Request::post(crate::direct::REMOTE_STREAM_RESULT_PATH)
                    .header("content-type", "application/octet-stream")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            ticket.recv().await.unwrap()["accepted"][0]["token_id"],
            1234
        );
    }

    #[tokio::test]
    async fn local_admin_boundary_does_not_restrict_pipeline_routes() {
        let state = testkit::state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        let app: Router = Router::new()
            .route("/api/ui/new-admin-endpoint", get(|| async { "admin" }))
            .route("/assets/app.js", get(|| async { "javascript" }))
            .route("/v1/pipeline-fallback", get(|| async { "pipeline" }))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        let mut admin_request = Request::builder()
            .uri("/api/ui/new-admin-endpoint")
            .body(axum::body::Body::empty())
            .unwrap();
        admin_request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        let admin_response = app.clone().oneshot(admin_request).await.unwrap();
        assert_eq!(admin_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            admin_response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        let admin_body = axum::body::to_bytes(admin_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let admin_body = String::from_utf8(admin_body.to_vec()).unwrap();
        assert!(admin_body.contains("127.0.0.1"));
        assert!(admin_body.contains("This administrative interface is available only"));
        assert!(admin_body.contains("Esta interface administrativa está disponível apenas"));
        assert!(!admin_body.contains("node_id"));
        assert!(!admin_body.contains("version"));

        let mut asset_request = Request::builder()
            .uri("/assets/app.js")
            .body(axum::body::Body::empty())
            .unwrap();
        asset_request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        let asset_response = app.clone().oneshot(asset_request).await.unwrap();
        assert_eq!(asset_response.status(), StatusCode::FORBIDDEN);
        let asset_body = axum::body::to_bytes(asset_response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!asset_body.starts_with(b"javascript"));

        let pipeline_request = Request::builder()
            .uri("/v1/pipeline-fallback")
            .header("authorization", "Bearer eu")
            .body(axum::body::Body::empty())
            .unwrap();
        let pipeline_response = app.oneshot(pipeline_request).await.unwrap();
        assert_eq!(pipeline_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn exposed_admin_boundary_requires_and_accepts_the_human_token() {
        let state = testkit::state_with("cluster-secret", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token =
                Some(crate::auth::hash_admin_password("person-secret").unwrap());
        }
        let app: Router = Router::new()
            .route("/api/ui/new-admin-endpoint", get(|| async { "admin" }))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        let mut without_token = Request::builder()
            .uri("/api/ui/new-admin-endpoint")
            .body(axum::body::Body::empty())
            .unwrap();
        without_token
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        let rejected = app.clone().oneshot(without_token).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

        let mut with_token = Request::builder()
            .uri("/api/ui/new-admin-endpoint")
            .header(crate::auth::ADMIN_ACCESS_TOKEN_HEADER, "person-secret")
            .body(axum::body::Body::empty())
            .unwrap();
        with_token
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        let accepted = app.oneshot(with_token).await.unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn non_admin_profiles_cannot_reach_admin_routes_without_internal_authority() {
        for profile in [
            crate::profile::NodeProfile::Infer,
            crate::profile::NodeProfile::Node,
        ] {
            let state = testkit::state_with("connection-secret", vec![]).await;
            {
                let mut auth = state.auth.write().await;
                auth.connection_key = "connection-secret".into();
                auth.profile = profile;
            }
            let app = Router::new()
                .merge(crate::admin::admin_routes())
                .merge(crate::webui::routes())
                .with_state(state.clone())
                .layer(middleware::from_fn_with_state(
                    state,
                    require_admin_for_surface,
                ));
            // Bloquear ou promover uma máquina continua administrativo mesmo
            // depois de a leitura do mesmo prefixo (`GET /admin/nodes`) abrir
            // para `node`.
            for path in [
                "/admin/nodes/outra/block",
                "/admin/nodes/outra/promote-admin",
            ] {
                let mut request = Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("authorization", "Bearer connection-secret")
                    .header("x-pom-profile", "admin")
                    .body(axum::body::Body::empty())
                    .unwrap();
                request
                    .extensions_mut()
                    .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));

                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "perfil={profile}, path={path}"
                );
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"], "perfil sem autorização para esta rota");
            }
        }
    }

    #[tokio::test]
    async fn authenticated_node_to_node_admin_routes_ignore_receiver_profile() {
        let state = testkit::state_with("connection-secret", vec![]).await;
        {
            let mut auth = state.auth.write().await;
            auth.connection_key = "connection-secret".into();
            auth.profile = crate::profile::NodeProfile::Node;
            auth.admin_exposure = crate::config::AdminExposure::Network;
        }
        let app = Router::new()
            .route("/api/ui/deployments", get(|| async { StatusCode::OK }))
            .route("/api/ui/stop-inference", get(|| async { StatusCode::OK }))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        // A leitura de deployments é NodeAdmin; a ação abaixo continua
        // AdminOnly e é o caso que verifica a separação de autoridade.
        for path in ["/api/ui/stop-inference"] {
            let mut browser_request = Request::get(path)
                .header("authorization", "Bearer connection-secret")
                .body(axum::body::Body::empty())
                .unwrap();
            browser_request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
            assert_eq!(
                app.clone().oneshot(browser_request).await.unwrap().status(),
                StatusCode::FORBIDDEN,
                "browser request must not inherit node authority for {path}"
            );

            let mut internal_request = Request::get(path)
                .header("authorization", "Bearer connection-secret")
                .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
                .body(axum::body::Body::empty())
                .unwrap();
            internal_request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
            assert_eq!(
                app.clone()
                    .oneshot(internal_request)
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK,
                "authenticated node request must reach {path}"
            );
        }
    }

    #[tokio::test]
    async fn policy_gets_accept_peer_auth_and_reject_bare_connection_key() {
        let state = testkit::state_with("principal", vec![]).await;
        state.network.set_cluster_logs_enabled(true);
        state.network.set_network_chat_enabled(true);
        let app = Router::new()
            .route(crate::tls_policy::TLS_POLICY_PATH, get(cluster_tls))
            .route(
                crate::cluster_logs_policy::CLUSTER_LOGS_POLICY_PATH,
                get(cluster_logs_policy),
            )
            .route(
                crate::network_chat_policy::NETWORK_CHAT_POLICY_PATH,
                get(network_chat_policy),
            )
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_admin_for_surface,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(2));
        let base = format!("http://{address}");
        assert!(!crate::tls_policy::fetch_from_principal_candidates(
            &clients,
            "principal",
            std::slice::from_ref(&base),
            "chave",
            state.authority_signing_key.as_ref(),
            &state.authority_principal,
            state.admin_trust.as_ref(),
        )
        .await
        .unwrap());
        assert!(crate::cluster_logs_policy::fetch_from_principal_candidates(
            &clients,
            "principal",
            std::slice::from_ref(&base),
            "chave",
            state.authority_signing_key.as_ref(),
            &state.authority_principal,
            state.admin_trust.as_ref(),
        )
        .await
        .unwrap());
        assert!(crate::network_chat_policy::fetch_from_principal_candidates(
            &clients,
            "principal",
            std::slice::from_ref(&base),
            "chave",
            state.authority_signing_key.as_ref(),
            &state.authority_principal,
            state.admin_trust.as_ref(),
        )
        .await
        .unwrap());

        for path in [
            crate::cluster_logs_policy::CLUSTER_LOGS_POLICY_PATH,
            crate::network_chat_policy::NETWORK_CHAT_POLICY_PATH,
        ] {
            let bare = clients
                .plain()
                .get(format!("{base}{path}"))
                .bearer_auth("chave")
                .send()
                .await
                .unwrap();
            assert_eq!(bare.status(), StatusCode::FORBIDDEN, "path={path}");
        }
        server.abort();
    }

    #[tokio::test]
    async fn an_admin_profile_without_a_trust_grant_has_no_admin_capability() {
        let mut state = testkit::state_with("connection-secret", vec![]).await;
        state.admin_trust = Arc::new(crate::admin_trust::AdminTrust::empty());
        {
            let mut auth = state.auth.write().await;
            auth.profile = crate::profile::NodeProfile::Admin;
            auth.connection_key = "connection-secret".into();
        }
        let app = Router::new()
            .route("/admin/authority", get(|| async { StatusCode::OK }))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        let mut request = Request::get("/admin/authority")
            .header("authorization", "Bearer connection-secret")
            .body(axum::body::Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Uma requisição pelo middleware, no perfil pedido, sem credencial
    /// interna: é assim que o navegador do operador chega ao seu próprio nó.
    #[cfg(test)]
    async fn browser_request(
        profile: crate::profile::NodeProfile,
        method: &str,
        path: &str,
    ) -> StatusCode {
        let state = testkit::state_with("connection-secret", vec![]).await;
        {
            let mut auth = state.auth.write().await;
            auth.connection_key = "connection-secret".into();
            auth.profile = profile;
        }
        let app = Router::new()
            .merge(crate::admin::admin_routes())
            .merge(crate::webui::routes())
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer connection-secret")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));
        app.oneshot(request).await.unwrap().status()
    }

    /// A recusa que interessa é a do perfil. Qualquer outro estado - a rota
    /// existir, o corpo agradar, o banco responder - é assunto do handler.
    fn refused_by_profile(status: StatusCode) -> bool {
        status == StatusCode::FORBIDDEN
    }

    /// Cada rota que este trabalho reclassificou: quem deve alcançar alcança,
    /// e quem não deve continua recusado. Uma tabela só, para a próxima
    /// mudança de escopo ter onde encostar.
    #[tokio::test]
    async fn the_reclassified_routes_open_for_the_right_profiles_only() {
        use crate::profile::NodeProfile::{Admin, Infer, Node};

        // (método, caminho, perfis que **entram**)
        let cases: &[(&str, &str, &[crate::profile::NodeProfile])] = &[
            // A tela Rede do `node` lê o retrato da rede.
            ("GET", "/admin/nodes", &[Node, Admin]),
            // Administrar aquelas máquinas continua sendo do administrador.
            ("POST", "/admin/nodes/outra/block", &[Admin]),
            ("POST", "/admin/nodes/outra/unblock", &[Admin]),
            ("DELETE", "/admin/nodes/outra", &[Admin]),
            ("POST", "/admin/nodes/outra/promote-admin", &[Admin]),
            // Girar a chave do cluster corta todos os outros nós.
            ("POST", "/admin/regenerate-key", &[Admin]),
            // A API key é a credencial de quem atende `/v1`, e todo perfil
            // atende - inclusive o `infer`, cujo rodapé abre esta tela.
            ("GET", "/admin/api-keys", &[Infer, Node, Admin]),
            ("POST", "/admin/api-keys", &[Infer, Node, Admin]),
            ("DELETE", "/admin/api-keys/abc", &[Infer, Node, Admin]),
            // Sair da rede é abdicar do próprio posto.
            ("POST", "/api/ui/leave", &[Infer, Node, Admin]),
            ("POST", "/api/ui/rejoin", &[Infer, Node, Admin]),
            // Corrigir o endereço anunciado descreve esta própria máquina.
            (
                "POST",
                "/api/ui/network/advertise-addr",
                &[Infer, Node, Admin],
            ),
            // O que não foi tocado continua onde estava.
            ("GET", "/api/ui/deployments", &[Node, Admin]),
            ("GET", "/api/ui/models", &[Node, Admin]),
        ];

        for (method, path, allowed) in cases {
            for profile in [Infer, Node, Admin] {
                let status = browser_request(profile, method, path).await;
                if allowed.contains(&profile) {
                    assert!(
                        !refused_by_profile(status),
                        "{profile} devia alcançar {method} {path}, veio {status}"
                    );
                } else {
                    assert!(
                        refused_by_profile(status),
                        "{profile} não devia alcançar {method} {path}, veio {status}"
                    );
                }
            }
        }
    }

    /// Reproduz a chamada do navegador no caso que falhava ao vivo: o `infer`
    /// atravessa o middleware, grava apenas a própria configuração e o próximo
    /// anúncio já carrega o endereço novo.
    #[tokio::test]
    async fn an_infer_node_can_save_its_own_address_and_announce_it_next() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let state = testkit::state_with("infer-node", vec![]).await;
        {
            let mut auth = state.auth.write().await;
            auth.connection_key = "connection-secret".into();
            auth.profile = crate::profile::NodeProfile::Infer;
        }
        struct ConfigPathGuard {
            previous: Option<std::ffi::OsString>,
            path: std::path::PathBuf,
        }
        impl Drop for ConfigPathGuard {
            fn drop(&mut self) {
                match &self.previous {
                    Some(previous) => std::env::set_var("POM_CONFIG", previous),
                    None => std::env::remove_var("POM_CONFIG"),
                }
                std::fs::remove_file(&self.path).ok();
            }
        }
        let config_path = std::env::current_dir()
            .unwrap()
            .join(format!(".pom-test-advertise-{}.json", std::process::id()));
        std::fs::remove_file(&config_path).ok();
        let _config_path_guard = ConfigPathGuard {
            previous: std::env::var_os("POM_CONFIG"),
            path: config_path.clone(),
        };
        std::env::set_var("POM_CONFIG", &config_path);
        let expected_address = format!("203.0.113.9:{}", state.network.bind_addr.port());
        let app = Router::new()
            .merge(crate::webui::routes())
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_admin_for_surface,
            ));

        let mut request = Request::post("/api/ui/network/advertise-addr")
            .header("authorization", "Bearer connection-secret")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"advertise_addr":"203.0.113.9"}"#,
            ))
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));

        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "body={}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            state.auth.read().await.advertise_addr.as_deref(),
            Some(expected_address.as_str())
        );

        let wire = state.network.gossip_announce_bytes(None).unwrap();
        let announce: crate::network::P2PMessage = serde_json::from_slice(&wire).unwrap();
        match announce {
            crate::network::P2PMessage::Announce {
                addr,
                addr_candidates,
                ..
            } => {
                assert_eq!(addr, expected_address);
                assert!(
                    addr_candidates.contains(&expected_address),
                    "candidatos={addr_candidates:?}"
                );
            }
            other => panic!("esperava Announce, veio {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_authenticated_unknown_admin_mutation_reaches_the_404_fallback() {
        let state = testkit::state_with("admin-node", vec![]).await;
        {
            let mut auth = state.auth.write().await;
            auth.connection_key = "connection-secret".into();
            auth.profile = crate::profile::NodeProfile::Admin;
        }
        let app = Router::new()
            .merge(crate::admin::admin_routes())
            .merge(crate::webui::routes())
            .merge(crate::webapp::routes())
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        let mut request = Request::post("/admin/not-a-real-route")
            .header("authorization", "Bearer connection-secret")
            .body(axum::body::Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));

        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    /// Sair é abdicar; mandar **outra** máquina sair é ato de administrador.
    /// Sem esta trava, o escopo mais largo da rota daria a qualquer nó o poder
    /// de expulsar um peer - o repasse viaja com a credencial interna, que o
    /// alvo não julga pelo perfil de quem pediu.
    #[tokio::test]
    async fn only_an_administrator_takes_another_machine_off_the_network() {
        use crate::profile::NodeProfile::{Infer, Node};

        for profile in [Infer, Node] {
            let state = testkit::state_with("connection-secret", vec![]).await;
            {
                let mut auth = state.auth.write().await;
                auth.connection_key = "connection-secret".into();
                auth.profile = profile;
            }
            let app = Router::new()
                .merge(crate::webui::routes())
                .with_state(state.clone())
                .layer(middleware::from_fn_with_state(
                    state,
                    require_admin_for_surface,
                ));
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/ui/leave")
                .header("authorization", "Bearer connection-secret")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"node_id":"outra-maquina"}"#))
                .unwrap();
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "perfil={profile}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body["error"],
                "só um administrador tira outra máquina da rede"
            );
        }
    }

    /// A frase da recusa é para a pessoa e muda com o idioma; o navegador
    /// precisa de um valor estável para saber que a sessão administrativa
    /// acabou e voltar ao portão, em vez de imprimir o 401 cru dentro da tela.
    #[tokio::test]
    async fn a_dead_administrative_session_names_itself_to_the_browser() {
        let state = testkit::state_with("connection-secret", vec![]).await;
        {
            let mut auth = state.auth.write().await;
            auth.profile = crate::profile::NodeProfile::Node;
            auth.admin_exposure = crate::config::AdminExposure::Network;
            auth.admin_access_token =
                Some(crate::auth::hash_admin_password("person-secret").unwrap());
        }
        let app = Router::new()
            .merge(crate::webui::routes())
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        let mut without = Request::get("/api/ui/paths")
            .body(axum::body::Body::empty())
            .unwrap();
        without
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        let response = app.clone().oneshot(without).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(crate::auth::ADMIN_ERROR_HEADER)
                .unwrap(),
            crate::auth::ADMIN_ERROR_MISSING_TOKEN
        );

        let mut wrong = Request::get("/api/ui/paths")
            .header(crate::auth::ADMIN_ACCESS_TOKEN_HEADER, "outra-coisa")
            .body(axum::body::Body::empty())
            .unwrap();
        wrong
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        let response = app.oneshot(wrong).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get(crate::auth::ADMIN_ERROR_HEADER)
                .unwrap(),
            crate::auth::ADMIN_ERROR_INVALID_TOKEN
        );
    }

    /// A chave de conexão volta ao `/api/ui/node`: a tela "Chave de Conexão" a
    /// mostra e o navegador a guarda para autenticar o que exige
    /// `ConnectionAuth`. Sem ela, aquela tela lia `undefined.length`.
    #[tokio::test]
    async fn the_node_view_carries_the_connection_key_for_the_operator_browser() {
        let state = testkit::state_with("connection-secret", vec![]).await;
        state.auth.write().await.connection_key = "connection-secret".into();
        let app = Router::new()
            .merge(crate::webui::routes())
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        let mut request = Request::get("/api/ui/node")
            .body(axum::body::Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["connection_key"], "connection-secret");
    }

    #[tokio::test]
    async fn each_profile_reaches_its_scope_and_gets_an_explicit_refusal() {
        let cases = [
            (
                crate::profile::NodeProfile::Infer,
                "/api/ui/network/request-node",
                "/api/ui/models",
            ),
            (
                crate::profile::NodeProfile::Node,
                "/api/ui/models",
                "/api/ui/access",
            ),
            (
                crate::profile::NodeProfile::Admin,
                "/api/ui/access",
                "/api/setup/save",
            ),
        ];

        for (profile, allowed_path, refused_path) in cases {
            let state = testkit::state_with("profile-test", vec![]).await;
            state.auth.write().await.profile = profile;
            let app = Router::new()
                .route(allowed_path, get(|| async { StatusCode::OK }))
                .route(refused_path, get(|| async { StatusCode::OK }))
                .with_state(state.clone())
                .layer(middleware::from_fn_with_state(
                    state,
                    require_admin_for_surface,
                ));

            let mut allowed_request = Request::get(allowed_path)
                .body(axum::body::Body::empty())
                .unwrap();
            allowed_request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));
            let allowed = app.clone().oneshot(allowed_request).await.unwrap();
            assert_eq!(allowed.status(), StatusCode::OK, "perfil={profile}");

            let mut refused_request = Request::get(refused_path)
                .body(axum::body::Body::empty())
                .unwrap();
            refused_request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));
            let refused = app.oneshot(refused_request).await.unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "perfil={profile}");
            let body = axum::body::to_bytes(refused.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], "perfil sem autorização para esta rota");
            assert_eq!(body["path"], refused_path);
            assert_eq!(body["profile"], profile.to_string());
        }
    }

    #[tokio::test]
    async fn a_valid_admin_token_mints_a_server_session_used_on_the_next_request() {
        let state = testkit::state_with("cluster-secret", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token =
                Some(crate::auth::hash_admin_password("person-secret").unwrap());
        }
        let app: Router = Router::new()
            .route("/api/ui/new-admin-endpoint", get(|| async { "admin" }))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));

        let mut login = Request::builder()
            .uri("/api/ui/new-admin-endpoint")
            .header(crate::auth::ADMIN_ACCESS_TOKEN_HEADER, "person-secret")
            .body(axum::body::Body::empty())
            .unwrap();
        login
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        login
            .extensions_mut()
            .insert(crate::serve::TransportSecurity(true));
        let login_response = app.clone().oneshot(login).await.unwrap();
        assert_eq!(login_response.status(), StatusCode::OK);
        let cookies: Vec<String> = login_response
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect();
        assert_eq!(cookies.len(), 3);
        assert!(cookies.iter().any(|cookie| cookie.contains("Path=/api/ui")));
        assert!(cookies.iter().any(|cookie| cookie.contains("Path=/admin")));
        assert!(cookies
            .iter()
            .any(|cookie| cookie.contains("Path=/admin-ui")));
        assert!(cookies.iter().all(|cookie| cookie.contains("; Secure")));
        let cookie = cookies[0].split(';').next().unwrap().to_string();

        let mut next_request = Request::builder()
            .uri("/api/ui/new-admin-endpoint")
            .header(axum::http::header::COOKIE, cookie)
            .body(axum::body::Body::empty())
            .unwrap();
        next_request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
        let next_response = app.oneshot(next_request).await.unwrap();
        assert_eq!(next_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn regenerating_the_token_or_returning_to_local_mode_rejects_the_old_session() {
        let state = testkit::state_with("cluster-secret", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token =
                Some(crate::auth::hash_admin_password("person-secret").unwrap());
        }
        let app: Router = Router::new()
            .route("/api/ui/new-admin-endpoint", get(|| async { "admin" }))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_admin_for_surface,
            ));

        async fn token_login(app: &Router) -> String {
            let mut request = Request::builder()
                .uri("/api/ui/new-admin-endpoint")
                .header(crate::auth::ADMIN_ACCESS_TOKEN_HEADER, "person-secret")
                .body(axum::body::Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
            let response = app.clone().oneshot(request).await.unwrap();
            response
                .headers()
                .get(axum::http::header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_string()
        }

        async fn with_cookie(app: &Router, cookie: &str) -> StatusCode {
            let mut request = Request::builder()
                .uri("/api/ui/new-admin-endpoint")
                .header(axum::http::header::COOKIE, cookie)
                .body(axum::body::Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("192.0.2.7:1234".parse().unwrap()));
            app.clone().oneshot(request).await.unwrap().status()
        }

        let cookie = token_login(&app).await;
        assert_eq!(with_cookie(&app, &cookie).await, StatusCode::OK);
        state.auth.write().await.admin_access_token =
            Some(crate::auth::hash_admin_password("new-secret").unwrap());
        assert_eq!(with_cookie(&app, &cookie).await, StatusCode::UNAUTHORIZED);

        state.auth.write().await.admin_access_token =
            Some(crate::auth::hash_admin_password("person-secret").unwrap());
        let cookie = token_login(&app).await;
        state.auth.write().await.admin_exposure = crate::config::AdminExposure::Localhost;
        assert_eq!(with_cookie(&app, &cookie).await, StatusCode::FORBIDDEN);
    }

    #[test]
    fn administrative_namespace_is_default_restricted() {
        assert!(is_admin_surface_path("/api/ui/new-route"));
        assert!(is_admin_surface_path("/admin/new-route"));
        assert!(is_admin_surface_path("/"));
        assert!(is_admin_surface_path("/assets/app.js"));
        assert!(!is_admin_surface_path("/api/access"));
        assert!(!is_admin_surface_path("/v1/prefill"));
    }

    #[tokio::test]
    async fn occupied_gateway_port_is_reported_before_starting_the_router() {
        let listener = tokio::net::TcpListener::bind("[::]:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let error = ensure_gateway_port_available(port, None)
            .await
            .expect_err("a porta ocupada precisa falhar");
        assert!(error.to_string().contains(&port.to_string()));
    }

    #[test]
    fn stop_matcher_holds_a_suffix_that_is_a_stop_prefix() {
        let mut matcher = StopMatcher::new(vec!["<END>".to_string()]);

        assert_eq!(
            matcher.feed("answer<").text,
            "answer",
            "the possible prefix must stay buffered"
        );
        assert_eq!(matcher.feed("EN").text, "");
        let matched = matcher.feed("D>ignored");
        assert_eq!(matched.text, "");
        assert_eq!(matched.matched.as_deref(), Some("<END>"));
        assert_eq!(matcher.feed("after").text, "");
    }

    #[test]
    fn stop_matcher_emits_text_before_a_stop_and_flushes_unmatched_suffix() {
        let mut matcher = StopMatcher::new(vec!["STOP".to_string()]);

        assert_eq!(matcher.feed("abcST").text, "abc");
        assert_eq!(matcher.finish(), "ST");

        let mut matcher = StopMatcher::new(vec!["STOP".to_string()]);
        assert_eq!(matcher.feed("abcST").text, "abc");
        assert_eq!(matcher.feed("ill").text, "STill");
        assert_eq!(matcher.finish(), "");
    }

    #[test]
    fn stop_matcher_preserves_utf8_boundaries_and_flushes_at_end() {
        let mut matcher = StopMatcher::new(vec!["fim".to_string()]);

        assert_eq!(matcher.feed("olá f").text, "olá ");
        assert_eq!(matcher.feed("oi").text, "foi");
        assert_eq!(matcher.finish(), "");

        let mut unicode = StopMatcher::new(vec!["終".to_string()]);
        assert_eq!(unicode.feed("pré").text, "pré");
        let emission = unicode.feed("終depois");
        assert_eq!(emission.text, "");
        assert_eq!(emission.matched.as_deref(), Some("終"));
    }

    #[test]
    fn stop_matcher_uses_the_earliest_match_and_normalizes_requests() {
        let mut matcher = StopMatcher::new(vec![
            "DONE".to_string(),
            "DONE".to_string(),
            "END".to_string(),
            String::new(),
        ]);

        let emission = matcher.feed("prefixENDsuffix");
        assert_eq!(emission.text, "prefix");
        assert_eq!(emission.matched.as_deref(), Some("END"));
    }

    #[test]
    fn openai_stop_input_accepts_one_string_or_a_list() {
        let one: StopInput = serde_json::from_str(r#""END""#).unwrap();
        let many: StopInput = serde_json::from_str(r#"["END","DONE"]"#).unwrap();

        assert_eq!(one.into_vec(), vec!["END"]);
        assert_eq!(many.into_vec(), vec!["END", "DONE"]);
    }

    /// O orçamento pedido pelo cliente é validado aqui; capacidade e contexto
    /// efetivos são resolvidos a partir do runtime ativo.
    #[test]
    fn api_generate_max_tokens_defaults_accepts_and_rejects_zero_only() {
        let absent: GenerateRequest = serde_json::from_str(r#"{"prompt":"oi"}"#).unwrap();
        let requested: GenerateRequest =
            serde_json::from_str(r#"{"prompt":"oi","max_tokens":64}"#).unwrap();
        let excessive: GenerateRequest =
            serde_json::from_str(r#"{"prompt":"oi","max_tokens":513}"#).unwrap();

        assert_eq!(resolve_request_max_tokens(absent.max_tokens), Ok(512));
        assert_eq!(resolve_request_max_tokens(requested.max_tokens), Ok(64));
        assert_eq!(resolve_request_max_tokens(excessive.max_tokens), Ok(513));
    }

    #[test]
    fn api_generate_max_tokens_rejects_zero_instead_of_silently_clamping() {
        assert_eq!(
            resolve_request_max_tokens(Some(0)),
            Err("max_tokens deve ser maior que zero".into())
        );
    }

    #[test]
    fn output_budget_counts_the_first_token_returned_by_prefill() {
        assert_eq!(remaining_generation_steps(64, 1), 63);
        assert_eq!(remaining_generation_steps(1, 1), 0);
        assert_eq!(remaining_generation_steps(64, 64), 0);
    }

    #[test]
    fn generation_finish_reason_prioritizes_the_effective_limit() {
        assert_eq!(generation_finish_reason(4000, 4000, true), Some("length"));
        assert_eq!(generation_finish_reason(4000, 3640, true), Some("stop"));
        assert_eq!(generation_finish_reason(4000, 3640, false), None);
    }

    /// Exercicio de ponta a ponta do caminho que um consumidor de
    /// `/api/generate` usa: o gateway transmite as fichas repetidas, para
    /// antes de pedir a próxima, libera a sessão e publica `cycle` em vez de
    /// amputar silenciosamente o orçamento.
    #[tokio::test]
    async fn api_generate_reports_and_stops_a_detected_cycle() {
        let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let script: std::sync::Arc<Vec<&'static str>> = std::sync::Arc::new(vec![
            "alpha ", "beta ", "alpha ", "beta ", "alpha ", "beta ",
        ]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(
                "/health",
                axum::routing::get(|| async {
                    axum::Json(json!({"status": "ok", "model_loaded": true}))
                }),
            )
            .route(
                "/v1/prefill",
                axum::routing::post(|| async {
                    let response = json!({
                        "done": false,
                        "more_chunks": false,
                        "n_prompt_tokens": 1,
                        "token_id": 1,
                        "text": "seed "
                    });
                    let bytes = crate::wire::encode_batch_single(
                        &response,
                        None,
                        crate::wire::WireDtype::F16,
                    )
                    .unwrap();
                    (
                        StatusCode::OK,
                        [("content-type", "application/octet-stream")],
                        bytes,
                    )
                        .into_response()
                }),
            )
            .route(
                "/v1/step",
                axum::routing::post({
                    let steps = steps.clone();
                    let script = script.clone();
                    move || {
                        let steps = steps.clone();
                        let script = script.clone();
                        async move {
                            let index = steps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let text = script.get(index).copied().unwrap_or("unexpected ");
                            let response = json!({
                                "done": false,
                                "token_id": index as u32 + 2,
                                "text": text
                            });
                            let bytes = crate::wire::encode_batch_single(
                                &response,
                                None,
                                crate::wire::WireDtype::F16,
                            )
                            .unwrap();
                            (
                                StatusCode::OK,
                                [("content-type", "application/octet-stream")],
                                bytes,
                            )
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/session/release",
                axum::routing::post(|| async { StatusCode::OK }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut peer = crate::gateway::testkit::peer("head", "head", 8080);
        peer.model = "lab".into();
        peer.start_layer = 0;
        peer.end_layer = 0;
        peer.total_layers = 1;
        peer.inference_port = Some(port);
        let state = crate::gateway::testkit::state_with("eu", vec![peer]).await;
        let mut config = crate::gateway::testkit::spec_config();
        config.total_layers = 1;
        config.end_layer = 0;
        config.peer_count = 1;
        config.speculative_decoding_enabled = false;
        config.cycle_detection_min_window_tokens = 2;
        config.cycle_detection_max_window_tokens = 2;
        config.cycle_detection_repetitions = 3;
        state.runtime.seed_config_for_test(config).await;

        let request = serde_json::to_vec(&GenerateRequest {
            model: Some("lab".into()),
            prompt: "Oi".into(),
            max_tokens: Some(32),
            stop: None,
        })
        .unwrap();
        let response = handle_generate(
            crate::auth::ApiKeyAuth::for_test(crate::reqlog::Caller::operator()),
            State(state),
            axum::body::Bytes::from(request),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["response"], "seed alpha beta alpha beta alpha beta ");
        assert_eq!(body["tokens_generated"], 7);
        assert_eq!(body["finish_reason"], "cycle");
        assert_eq!(body["partial"], true);
        assert_eq!(body["stop_sequence"], Value::Null);
        assert_eq!(steps.load(std::sync::atomic::Ordering::SeqCst), 6);
    }

    #[test]
    fn normal_api_generate_responses_keep_the_existing_shape() {
        let response = GenerateResponse {
            response: "ok".into(),
            source: "pipeline:head".into(),
            tokens_generated: 1,
            tokens_per_sec: 1.0,
            duration_ms: 1,
            timings: None,
            stop_sequence: None,
            finish_reason: None,
            partial: false,
            trace: None,
        };
        let value = serde_json::to_value(response).unwrap();
        assert!(!value.as_object().unwrap().contains_key("finish_reason"));
        assert!(!value.as_object().unwrap().contains_key("partial"));
    }

    #[test]
    fn audit_status_distinguishes_length_from_transport_partial() {
        let summary = |finish_reason: Option<&str>, partial: bool| GenSummary {
            source: "pipeline:node-a".into(),
            tokens_generated: 4000,
            tokens_per_sec: 1.0,
            duration_ms: 1,
            timings: timing_metrics(1, 1, 1, 1, 4000),
            partial,
            finish_reason: finish_reason.map(str::to_owned),
            stop_sequence: None,
            legs: 1,
            trace: None,
        };

        assert_eq!(
            audit_status_for_summary(&summary(Some("length"), false)),
            "incomplete"
        );
        assert_eq!(audit_status_for_summary(&summary(None, true)), "partial");
        assert_eq!(
            audit_status_for_summary(&summary(Some("stop"), false)),
            "ok"
        );
    }

    /// O endereco anunciado tem que ganhar do IP da LAN. Esta entrada viaja
    /// dentro de `next_hops`, e um hop noutro site que receba `192.168.x.y`
    /// disca um endereco que nao existe para ele - a requisicao morre sem
    /// chegar a lugar nenhum, e so quando este no NAO e a cabeca do pipeline.
    #[test]
    fn an_advertised_address_beats_the_lan_ip_for_the_self_entry() {
        assert_eq!(
            self_dial_host(
                Some("[2804:d55:5579:1000:89f3:3f94:7c07:e183]".into()),
                Some("192.168.100.138".into()),
            ),
            "[2804:d55:5579:1000:89f3:3f94:7c07:e183]",
        );
    }

    /// Sem endereco anunciado o IP da LAN continua valendo: num cluster de uma
    /// rede so ele e alcancavel, e o loopback nao seria.
    #[test]
    fn without_an_advertised_address_the_lan_ip_is_kept() {
        assert_eq!(
            self_dial_host(None, Some("192.168.100.138".into())),
            "192.168.100.138",
        );
        assert_eq!(self_dial_host(None, None), "127.0.0.1");
    }

    /// The self-entry `.addr` uses the same address precedence as Announce:
    /// explicit pin, current public local address, then peer observation.
    #[tokio::test]
    async fn the_self_entry_address_picks_up_an_observed_public_address() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.network.set_observed_addr("203.0.113.5:9999");

        let peers = state.all_peers().await;
        let me = peers
            .iter()
            .find(|p| p.node_id == "eu")
            .expect("self entry");

        let expected_host = state
            .network
            .effective_host()
            .expect("self entry has an effective host");
        assert!(
            me.addr.starts_with(&format!("{expected_host}:")),
            "self entry addr should use the effective host, got {}",
            me.addr
        );
    }

    /// Live regression for the captain's Gemma4 request. Set
    /// `POM_FTK_E2E_URL` to a gateway before running this ignored test; it is
    /// deliberately not part of the default suite because it consumes a real
    /// model and GPU time.
    #[tokio::test]
    #[ignore = "requires a live Gemma4 MoE gateway"]
    async fn gemma4_hello_world_request_uses_moe_and_stops_on_its_turn() {
        let base = std::env::var("POM_FTK_E2E_URL")
            .expect("POM_FTK_E2E_URL must point at the live gateway");
        let model = std::env::var("POM_FTK_E2E_MODEL")
            .unwrap_or_else(|_| "Gemma4-26b-NVFP4.gguf".to_string());
        let response = reqwest::Client::new()
            .post(format!("{base}/api/generate"))
            .json(&json!({
                "model": model,
                "prompt": "Escreva hello world 10 vezes.",
                "max_tokens": 64
            }))
            .send()
            .await
            .expect("gateway request");
        assert!(
            response.status().is_success(),
            "gateway returned {}",
            response.status()
        );
        let body: Value = response.json().await.expect("JSON generation response");

        assert!(body["tokens_generated"].as_u64().unwrap_or(513) <= 64);
        assert!(body["trace"]["input_tokens"].as_u64().unwrap_or(0) > 0);
        assert!(body["trace"]["complete"].as_bool().unwrap_or(false));
        for phase in ["prefill", "decode"] {
            let phase = &body["trace"]["hops"][0][phase];
            assert_eq!(phase["executed_engine"], "moe");
            assert_eq!(phase["engine_mismatch"], false);
        }
        let text = body["response"].as_str().unwrap_or_default();
        for marker in ["<|im_start|>", "<|im_end|>", "<|channel>", "<channel|>"] {
            assert!(
                !text.contains(marker),
                "raw template marker leaked: {marker}"
            );
        }
    }

    #[test]
    fn token_estimate_is_pessimistic() {
        // 3 chars per token errs high, which is what a crash guard needs.
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens(&"a".repeat(3000)), 1000);
    }

    #[test]
    fn token_estimate_counts_characters_not_bytes() {
        // Accented Portuguese must not inflate the count through UTF-8 bytes.
        assert_eq!(estimate_tokens("ção"), 1);
    }

    #[test]
    fn prompt_over_capacity_is_rejected_without_local_runtime_config() {
        let prompt = "a".repeat(30_289);
        let error = prompt_budget_error_of(7_000, 200, &prompt)
            .expect("um prompt acima da capacidade do peer precisa ser recusado");

        assert!(error.contains("Prompt grande demais"));
        assert!(error.contains("~10097"));
        assert!(error.contains("limite de 6800"));
        assert!(error.contains("Aumente n_ctx"));
    }

    /// Reprodução do defeito de 2026-09-08 na ksia, ponta a ponta: um gateway
    /// **sem modelo local** recebe um prompt de 30 mil caracteres para um
    /// modelo que só o peer serve. Antes, a admissão só existia quando havia
    /// config local, o prompt seguia para o prefill e o decode do peer abortava
    /// o processo inteiro em `ggml-cuda.cu` com `CUDA error: out of memory`.
    /// Agora a capacidade anunciada pelo peer recusa antes de qualquer peso ser
    /// tocado, e o nó continua de pé.
    #[tokio::test]
    async fn gateway_sem_modelo_local_recusa_prompt_acima_da_capacidade_do_peer() {
        let mut peer = testkit::peer("peer-gpu", "ksia", 39_991);
        peer.model = "ornith".into();
        peer.inference_port = Some(39_992);
        peer.total_layers = 32;
        peer.end_layer = 31;
        let state = testkit::state_with("gateway", vec![peer]).await;
        assert!(
            state.runtime.config().await.is_none(),
            "este gateway não pode ter modelo local, senão o teste cobre o caminho antigo"
        );

        let error = crate::gateway::generate_blocking_with_stops(
            "/api/generate",
            crate::reqlog::Caller::operator(),
            crate::gateway::GenerationInput {
                state,
                inference_token: "test-api-key".into(),
                model: "ornith".into(),
                prompt: "a".repeat(30_289),
                images: Vec::new(),
                chat_template: false,
                thinking_enabled: true,
                budget: crate::gateway::OutputBudget::single(200),
                stops: Vec::new(),
                cache_key: None,
                preferred_head: None,
                request_details: serde_json::json!({}),
                trace_id: None,
            },
        )
        .await
        .expect_err("um prompt acima da capacidade do peer precisa ser recusado");

        assert!(
            error.contains("Prompt grande demais"),
            "a recusa precisa ser a mensagem acionável, e não uma falha de transporte: {error}"
        );
        assert!(error.contains("Aumente n_ctx"), "{error}");
    }

    #[test]
    fn rate_is_zero_for_an_instant_generation() {
        assert_eq!(rate(10, 0.0), 0.0);
        assert!((rate(10, 2.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn timing_metrics_separates_normal_prefill_from_generation_and_total() {
        let timings = timing_metrics(2_000, 1_200, 1_000, 500, 20);

        assert_eq!(timings.ttft_ms, 1_200);
        assert_eq!(timings.prefill_ms, 1_000);
        assert_eq!(timings.prefill_tokens, 500);
        assert_eq!(timings.generation_ms, 800);
        assert_eq!(timings.generation_tokens, 20);
        assert_eq!(timings.prefill_tokens_per_sec, Some(500.0));
        assert_eq!(timings.generation_tokens_per_sec, Some(25.0));
        assert_eq!(timings.total_ms, 2_000);
        assert_eq!(timings.total_tokens_per_sec, 10.0);
    }

    #[test]
    fn timing_metrics_reserves_independent_server_engine_and_client_ttft() {
        let timings = timing_metrics(2_000, 1_200, 1_000, 500, 20);

        assert_eq!(timings.server_ttft_ms, None);
        assert_eq!(timings.engine_ttft_ms, None);
        assert_eq!(timings.client_ttft_ms, None);
    }

    #[test]
    fn timing_metrics_does_not_divide_by_zero_after_full_prefix_reuse() {
        let timings = timing_metrics(1_000, 400, 0, 0, 12);

        assert_eq!(timings.prefill_tokens_per_sec, None);
        assert_eq!(timings.generation_ms, 600);
        assert_eq!(timings.generation_tokens_per_sec, Some(20.0));
    }

    #[test]
    fn timing_metrics_uses_the_aggregate_prefill_for_multiple_chunks() {
        let timings = timing_metrics(3_500, 2_000, 1_800, 900, 30);

        assert_eq!(timings.prefill_tokens, 900);
        assert_eq!(timings.prefill_ms, 1_800);
        assert_eq!(timings.prefill_tokens_per_sec, Some(500.0));
    }

    #[test]
    fn timing_metrics_keeps_a_single_output_token_unmeasurable_when_instant() {
        let timings = timing_metrics(900, 900, 700, 100, 1);

        assert_eq!(timings.generation_ms, 0);
        assert_eq!(timings.generation_tokens_per_sec, None);
    }

    #[test]
    fn generation_speed_does_not_change_with_prompt_size() {
        let short_prompt = timing_metrics(1_000, 700, 650, 20, 14);
        let long_prompt = timing_metrics(2_000, 1_700, 1_650, 2_000, 14);

        assert_eq!(short_prompt.generation_ms, long_prompt.generation_ms);
        assert_eq!(
            short_prompt.generation_tokens_per_sec,
            long_prompt.generation_tokens_per_sec
        );
        assert_ne!(short_prompt.prefill_tokens, long_prompt.prefill_tokens);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::runtime::NodeConfig;

    /// A reprodução do defeito, em forma de conta: a implantação **não** pode
    /// mais decidir quanto a requisição inteira gera. Era exatamente isto que
    /// devolvia 1000 tokens a quem pediu 4000, calado.
    #[test]
    fn the_deployment_ceiling_bounds_a_leg_and_no_longer_bounds_the_request() {
        let limit = output_token_limit(4_000, Some(1_000), Some(200_000), 18);
        assert_eq!(limit.requested, 4_000);
        assert_eq!(limit.deployment, Some(1_000));
        assert_eq!(limit.effective, 4_000, "o cliente recebe o que pediu");
        assert_eq!(limit.leg, 1_000, "uma geração de backend continua em 1000");
        assert!(!limit.was_clamped());
        assert!(limit.continuation_possible());
        assert_eq!(limit.effective.div_ceil(limit.leg), 4, "quatro pernas");
    }

    /// O contexto que sobrou continua apertando sem recusar - ele sai de uma
    /// **estimativa** do prompt, e recusar por estimativa rejeitaria pedido
    /// válido. O `x_pom` conta a verdade (`was_clamped`), que é o canal certo
    /// para um número aproximado.
    #[test]
    fn the_remaining_context_still_clamps_because_it_is_an_estimate() {
        let limit = output_token_limit(50_000, Some(32_768), Some(40_000), 39_000);
        assert_eq!(limit.context_available, Some(1_000));
        assert_eq!(limit.effective, 1_000);
        assert_eq!(limit.leg, 1_000, "a perna nunca passa do total");
        assert!(!limit.continuation_possible());
        assert!(limit.was_clamped());
    }

    #[test]
    fn requested_client_limit_is_effective_when_it_fits_under_every_ceiling() {
        let limit = output_token_limit(4_000, Some(32_768), Some(100_000), 2_000);
        assert_eq!(limit.requested, 4_000);
        assert_eq!(limit.effective, 4_000);
        assert_eq!(limit.leg, 4_000);
        assert!(!limit.continuation_possible());
        assert!(!limit.was_clamped());
    }

    #[test]
    fn output_limit_preserves_the_requested_value_when_no_deployment_is_loaded() {
        let limit = output_token_limit(8_192, None, None, 0);
        assert_eq!(limit.effective, 8_192);
        assert_eq!(limit.leg, 8_192);
        assert!(!limit.was_clamped());
    }

    #[test]
    fn output_limit_contract_has_no_removed_absolute_capacity_field() {
        let limit = output_token_limit(32_000, Some(32_000), Some(100_000), 0);
        let value = serde_json::to_value(limit).unwrap();
        let fields = value.as_object().expect("limites de saída serializados");
        let legacy_field = ["absolute", "max", "tokens"].join("_");

        assert_eq!(fields.len(), 5);
        assert!(!fields.contains_key(&legacy_field));
    }

    #[test]
    fn output_limit_reports_no_capacity_when_the_prompt_fills_the_context() {
        let limit = output_token_limit(32_768, Some(32_768), Some(10_000), 10_000);
        assert_eq!(limit.effective, 0);
        assert!(limit.was_clamped());
        assert!(limit
            .rejection_reason()
            .unwrap()
            .contains("todo o contexto"));
    }

    #[test]
    fn a_deployment_that_cannot_generate_is_still_a_refusal_not_a_silent_cut() {
        let limit = output_token_limit(4_000, Some(0), Some(100_000), 10);
        assert_eq!(limit.leg, 0);
        assert!(limit.rejection_reason().unwrap().contains("max_new_tokens"));
    }

    #[test]
    fn an_unconfirmed_peer_capacity_is_not_replaced_by_local_fallback() {
        let limit = OutputTokenLimit {
            requested: 32_000,
            effective: 0,
            deployment: None,
            context_available: None,
            leg: 0,
            capacity_pending: true,
        };
        assert!(limit.capacity_pending());
        assert!(limit
            .rejection_reason()
            .unwrap()
            .contains("todos os peers da pipeline planejada"));
    }

    #[tokio::test]
    async fn a_live_peer_without_capacity_blocks_model_specific_requests() {
        let mut peer = crate::gateway::testkit::peer("n1", "n1", 8080);
        peer.model = "qwen".into();
        peer.inference_port = Some(8181);
        peer.end_layer = 31;
        peer.total_layers = 32;
        peer.runtime_capacity = None;
        let state = crate::gateway::testkit::state_with("self", vec![peer]).await;

        let limit = state
            .request_output_limit_for_model(Some("qwen"), 32_000, "oi")
            .await;

        assert!(limit.capacity_pending());
        assert_eq!(limit.effective, 0);
        assert!(limit
            .rejection_reason()
            .unwrap()
            .contains("pipeline planejada"));
    }

    #[tokio::test]
    async fn capacity_gate_ignores_an_unplanned_peer_without_capacity() {
        let mut ready = crate::gateway::testkit::peer("a-ready", "ready", 8080);
        ready.model = "qwen".into();
        ready.inference_port = Some(8182);
        ready.start_layer = 0;
        ready.end_layer = 31;
        ready.total_layers = 32;
        ready.runtime_capacity = Some(RuntimeCapacity {
            n_ctx: 8_192,
            max_new_tokens: 256,
        });

        let mut alternative = crate::gateway::testkit::peer("z-alternative", "alternative", 8081);
        alternative.model = "qwen".into();
        alternative.inference_port = Some(8183);
        alternative.start_layer = 0;
        alternative.end_layer = 31;
        alternative.total_layers = 32;
        alternative.runtime_capacity = None;

        let state = crate::gateway::testkit::state_with("self", vec![ready, alternative]).await;
        let (_, pending) = state.runtime_capacity_for_model(Some("qwen")).await;

        assert!(
            !pending,
            "a route not selected by the planner cannot block the request"
        );
    }

    #[tokio::test]
    async fn capacity_gate_does_not_block_an_idle_peer_selected_by_the_pipeline() {
        let mut head = crate::gateway::testkit::peer("head", "head", 8080);
        head.model = "qwen".into();
        head.inference_port = Some(8184);
        head.start_layer = 0;
        head.end_layer = 15;
        head.total_layers = 32;
        head.runtime_capacity = Some(RuntimeCapacity {
            n_ctx: 8_192,
            max_new_tokens: 256,
        });

        let mut tail = crate::gateway::testkit::peer("tail", "tail", 8081);
        tail.model = "qwen".into();
        tail.inference_port = Some(8185);
        tail.start_layer = 16;
        tail.end_layer = 31;
        tail.total_layers = 32;
        tail.idle = true;
        tail.runtime_capacity = None;

        let state = crate::gateway::testkit::state_with("self", vec![head, tail]).await;
        let (_, pending) = state.runtime_capacity_for_model(Some("qwen")).await;

        assert!(
            !pending,
            "idle planned peers are woken by generation before inference"
        );
    }

    /// O laço tem que ficar dentro do orçamento total mesmo quando a perna não
    /// divide o total, e a perna nunca pode ser zero (divisão por zero no
    /// `note_leg_boundary`).
    #[test]
    fn the_budget_normalizes_to_usable_numbers() {
        assert_eq!(
            OutputBudget { total: 0, leg: 0 }.normalized(),
            OutputBudget { total: 1, leg: 1 }
        );
        assert_eq!(
            OutputBudget {
                total: 10,
                leg: 1_000
            }
            .normalized(),
            OutputBudget { total: 10, leg: 10 }
        );
        assert_eq!(
            OutputBudget::single(7).normalized(),
            OutputBudget { total: 7, leg: 7 }
        );
    }

    /// A contagem de pernas é o que torna a costura inspecionável. Ela avança
    /// ao fechar a perna, e **não** avança na última - `legs` é quantas
    /// pernas a resposta usou, não quantas caberiam.
    #[test]
    fn legs_are_counted_as_the_loop_crosses_each_deployment_budget() {
        let budget = OutputBudget { total: 10, leg: 4 };
        let mut legs = 1;
        let mut observed = Vec::new();
        for tokens in 1..=10u32 {
            note_leg_boundary(budget, tokens, &mut legs);
            observed.push(legs);
        }
        assert_eq!(observed, vec![1, 1, 1, 2, 2, 2, 2, 3, 3, 3]);
        assert_eq!(legs, 3);
    }

    #[test]
    fn a_generation_that_fits_one_leg_reports_a_single_leg() {
        let budget = OutputBudget {
            total: 100,
            leg: 100,
        };
        let mut legs = 1;
        for tokens in 1..=100u32 {
            note_leg_boundary(budget, tokens, &mut legs);
        }
        assert_eq!(legs, 1, "acabar o total não abre perna nenhuma");
    }

    fn cfg(n_ctx: u32, n_batch: u32, max_new: u32, max_sessions: u32) -> NodeConfig {
        NodeConfig {
            deploy_id: String::new(),
            model_path: String::new(),
            mmproj_path: None,
            mtp_draft_path: None,
            model_label: String::new(),
            deployment_name: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            peer_count: 1,
            peer_index: 0,
            primary: true,
            inference_mode: Default::default(),
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            slot_save_path: None,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch,
            max_sessions,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            prefill_max_inflight_chunks:
                crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS as u32,
            max_new_tokens: max_new,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            cycle_detection_enabled: true,
            cycle_detection_min_window_tokens: crate::cycle::DEFAULT_CYCLE_MIN_WINDOW_TOKENS,
            cycle_detection_max_window_tokens: crate::cycle::DEFAULT_CYCLE_MAX_WINDOW_TOKENS,
            cycle_detection_repetitions: crate::cycle::DEFAULT_CYCLE_REPETITIONS,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            mtp_cpu_fallback_enabled: true,
            unload_if_idle: false,
            idle_minutes: 60,
            tp: None,
            speculative_k: 4,
            speculative_lookup_window: 3,
        }
    }

    /// The whole point of the fix: one session means the configured context is
    /// the window, instead of an eighth of it.
    #[test]
    fn a_single_session_gets_the_whole_context() {
        let c = cfg(262_144, 262_144, 1024, 1);
        assert_eq!(prompt_budget(&c), 262_144 - 1024);
    }

    #[test]
    fn each_session_keeps_the_configured_window() {
        let c = cfg(262_144, 262_144, 1024, 8);
        assert_eq!(prompt_budget(&c), 262_144 - 1024);
    }

    /// The bug a harness hits on its first real conversation: with the prompt
    /// travelling in chunks, a small `n_batch` must not cap the prompt. The Pi
    /// sent ~23k tokens into a 32k window and was refused against a limit of
    /// 512, which was `n_batch`.
    #[test]
    fn n_batch_no_longer_caps_the_prompt() {
        let c = cfg(32_768, 512, 1024, 1);
        assert_eq!(prompt_budget(&c), 32_768 - 1024);
    }

    /// `n_batch` did not stop mattering — it moved to the chunk, which is what
    /// llama.cpp actually decodes in one go.
    #[test]
    fn n_batch_caps_the_chunk_instead() {
        let c = cfg(32_768, 512, 1024, 1);
        assert_eq!(prompt_chunk_tokens(&c), 512);

        let wide = cfg(32_768, 4096, 1024, 1);
        assert_eq!(
            prompt_chunk_tokens(&wide),
            crate::prefill::DEFAULT_CHUNK_TOKENS
        );
    }

    #[test]
    fn zero_sessions_does_not_divide_by_zero() {
        let c = cfg(8192, 8192, 256, 0);
        assert_eq!(prompt_budget(&c), 8192 - 256);
    }
}

#[cfg(test)]
mod remote_stream_selection_tests {
    use super::*;

    fn mtp_capabilities() -> Vec<String> {
        vec![
            crate::pipeline::CAP_SPECULATE_LOOKUP.into(),
            crate::pipeline::CAP_SPECULATE_MTP_SLICED.into(),
            crate::pipeline::CAP_SPECULATE_MTP.into(),
        ]
    }

    fn plan(node_id: &str, capabilities: Vec<String>) -> PipelinePlan {
        PipelinePlan {
            steps: vec![crate::pipeline::PipelineStep {
                node_id: node_id.into(),
                inference_url: "http://127.0.0.1:8181".into(),
                inference_candidates: Vec::new(),
                quic_addr_candidates: Vec::new(),
                start_layer: 0,
                end_layer: 31,
                exec_start: 0,
                exec_end: 31,
                inference_frame_capabilities: capabilities,
                libp2p_peer_id: None,
                libp2p_addr_candidates: Vec::new(),
            }],
            edges: Vec::new(),
        }
    }

    #[test]
    fn remote_stream_never_replaces_the_local_shortcut() {
        let plan = plan(
            "local",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        assert!(!remote_stream_capable(&plan, "local", true));
    }

    #[test]
    fn remote_stream_requires_the_new_capability_and_a_callback() {
        let without_cap = plan("remote", Vec::new());
        assert!(!remote_stream_capable(&without_cap, "local", true));
        let with_cap = plan(
            "remote",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        assert!(remote_stream_capable(&with_cap, "local", true));
        assert!(!remote_stream_capable(&with_cap, "local", false));
    }

    #[test]
    fn remote_stream_keeps_the_remaining_pipeline_inside_the_remote_head() {
        let mut multi = plan(
            "remote-head",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        multi.steps.push(crate::pipeline::PipelineStep {
            node_id: "remote-tail".into(),
            inference_url: "http://127.0.0.1:8182".into(),
            inference_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
            start_layer: 32,
            end_layer: 63,
            exec_start: 32,
            exec_end: 63,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        });
        assert!(remote_stream_capable(&multi, "local", true));
    }

    #[test]
    fn stream_remoto_de_dois_hops_desliga_mtp_sincrono_por_ciclo() {
        let mut multi = plan(
            "remote-head",
            [
                vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
                mtp_capabilities(),
            ]
            .concat(),
        );
        multi.steps[0]
            .inference_frame_capabilities
            .extend(mtp_capabilities());
        multi.steps.push(crate::pipeline::PipelineStep {
            node_id: "remote-tail".into(),
            inference_url: "http://127.0.0.1:8182".into(),
            inference_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
            start_layer: 32,
            end_layer: 63,
            exec_start: 32,
            exec_end: 63,
            inference_frame_capabilities: mtp_capabilities(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        });

        let speculate = remote_stream_speculation(
            &multi,
            true,
            false,
            4,
            3,
            4,
            crate::prefill::DraftSource::Lookup,
        );
        assert!(speculate.is_none());
    }

    #[test]
    fn stream_remoto_desliga_mtp_fatiado_mas_preserva_o_caminho_legado() {
        assert!(disable_sliced_mtp_for_remote_stream(true, true));
        assert!(!disable_sliced_mtp_for_remote_stream(false, true));
        assert!(!disable_sliced_mtp_for_remote_stream(true, false));

        let legacy = remote_stream_speculation(
            &multi_plan_with_mtp(),
            true,
            true,
            4,
            3,
            4,
            crate::prefill::DraftSource::Mtp,
        )
        .expect("o caminho legado deve continuar elegível a MTP");
        assert_eq!(legacy.source, crate::infer_server::DraftSource::Mtp);
        assert_eq!(legacy.k, 4);
    }

    fn multi_plan_with_mtp() -> PipelinePlan {
        let mut multi = plan(
            "remote-head",
            [
                vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
                mtp_capabilities(),
            ]
            .concat(),
        );
        multi.steps[0]
            .inference_frame_capabilities
            .extend(mtp_capabilities());
        multi.steps.push(crate::pipeline::PipelineStep {
            node_id: "remote-tail".into(),
            inference_url: "http://127.0.0.1:8182".into(),
            inference_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
            start_layer: 32,
            end_layer: 63,
            exec_start: 32,
            exec_end: 63,
            inference_frame_capabilities: mtp_capabilities(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        });
        multi
    }

    #[test]
    fn remote_stream_mtp_fatiado_e_omitido_por_cadencia_explicita() {
        let multi = multi_plan_with_mtp();
        let disabled = disable_sliced_mtp_for_remote_stream(true, true);
        assert!(disabled);
        assert!(remote_stream_speculation(
            &multi,
            true,
            !disabled,
            4,
            3,
            4,
            crate::prefill::DraftSource::Mtp,
        )
        .is_none());
    }

    #[test]
    fn remote_stream_de_um_hop_preserva_mtp_do_modelo_inteiro() {
        let single = plan("remote-head", mtp_capabilities());
        let speculate = remote_stream_speculation(
            &single,
            true,
            true,
            4,
            3,
            4,
            crate::prefill::DraftSource::Mtp,
        )
        .expect("MTP do modelo inteiro não depende de draft fatiado");
        assert_eq!(speculate.source, crate::infer_server::DraftSource::Mtp);
    }

    #[test]
    fn gateway_sem_runtime_local_usa_capacidade_mtp_remota() {
        assert!(mtp_enabled_for_plan(None, true));
        assert!(!mtp_enabled_for_plan(None, false));
        assert!(!mtp_enabled_for_plan(Some(false), true));
    }

    #[test]
    fn stream_de_dois_hops_nao_desliga_mtp_apos_o_token_de_aquecimento() {
        let step = crate::prefill::SpeculativeStep {
            accepted: vec![(4, "A".into())],
            done: false,
            accept_count: 1,
            batch_width: 1,
            speculated: false,
            speculation_active: true,
            speculation_disabled_reason: None,
            speculation_disabled_detail: None,
            next_draft: Some(vec![5, 6]),
            trace: crate::prefill::RequestTrace::new("qwen", &[("remote-head", 0, 31)]),
        };

        assert!(sliced_mtp_round_continues(true, false, &step));
    }

    #[test]
    fn stream_remoto_nao_pede_rollback_extra_por_lote() {
        assert!(!gateway_rollback_needed(true, 1, 4));
        assert!(gateway_rollback_needed(false, 1, 4));
        assert!(!gateway_rollback_needed(true, 4, 4));
    }

    #[test]
    fn peer_sem_capacidade_mtp_fatiado_degrada_com_razao() {
        let mut multi = plan(
            "remote-head",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        multi.steps.push(crate::pipeline::PipelineStep {
            node_id: "remote-tail".into(),
            inference_url: "http://127.0.0.1:8182".into(),
            inference_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
            start_layer: 32,
            end_layer: 63,
            exec_start: 32,
            exec_end: 63,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        });

        assert_eq!(
            sliced_mtp_disable_reason(&multi),
            crate::infer_server::SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE
        );
        assert!(remote_stream_speculation(
            &multi,
            true,
            true,
            4,
            3,
            4,
            crate::prefill::DraftSource::Mtp,
        )
        .is_none());
    }
}

#[cfg(test)]
mod peer_visibility_tests {
    use super::*;

    fn peer(id: &str) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: id.into(),
            pubkey: String::new(),
            authority_pubkey: String::new(),
            cluster_id: None,
            addr: "127.0.0.1:42777".into(),
            addr_candidates: Vec::new(),
            model: "qwen35".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: 32,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_quic_port: None,
            inference_frame_capabilities: Vec::new(),
            gateway_port: Some(8090),
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Node,
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            libp2p_peer_id: None,
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    #[test]
    fn a_blocked_peer_is_excluded_from_cluster_planning() {
        let visible =
            peers_visible_to_cluster(vec![peer("current"), peer("stale")], &["stale".to_string()]);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].node_id, "current");
    }

    #[tokio::test]
    async fn a_peer_whose_inference_health_times_out_is_excluded_from_the_plan() {
        let healthy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthy_port = healthy_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/health",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "status": "ok",
                        "model_loaded": true,
                    }))
                }),
            );
            axum::serve(healthy_listener, app).await.unwrap();
        });

        let stalled_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stalled_port = stalled_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _connection = stalled_listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut healthy = peer("healthy");
        healthy.inference_port = Some(healthy_port);
        let mut stalled = peer("stalled");
        stalled.inference_port = Some(stalled_port);

        let health = super::AdmissionHealth::default();
        let clientes = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));
        let reachable = ready_for_pipeline(
            &clientes,
            &health,
            vec![healthy.clone(), stalled.clone()],
            Duration::from_millis(25),
        )
        .await;

        assert_eq!(reachable.len(), 1);
        assert_eq!(reachable[0].node_id, "healthy");

        // A defesa tem que continuar de pé na requisição seguinte: o peer que
        // reprovou fica fora, agora sem custar sequer a sonda.
        let inicio = std::time::Instant::now();
        let reachable = ready_for_pipeline(
            &clientes,
            &health,
            vec![healthy, stalled],
            Duration::from_millis(25),
        )
        .await;
        let em_quarentena = inicio.elapsed();

        assert_eq!(reachable.len(), 1);
        assert_eq!(reachable[0].node_id, "healthy");
        assert!(
            em_quarentena < Duration::from_millis(25),
            "peer em quarentena voltou a ser sondado: {em_quarentena:?}"
        );
    }

    #[tokio::test]
    async fn a_health_probe_refreshes_stale_layer_coverage_before_planning() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/health",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "status": "ok",
                        "model_loaded": true,
                        "model_label": "qwen35",
                        "deploy_id": "d-1234abcd",
                        "start_layer": 0,
                        "end_layer": 31,
                        "total_layers": 32,
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        // The network announcement still carries the old one-layer slice, as
        // can happen between apply and the next authenticated announce. The
        // inference server already knows the complete current assignment.
        let mut stale = peer("qwen-head");
        stale.model = "old-model".into();
        stale.deploy_id = "d-old0000".into();
        stale.start_layer = 0;
        stale.end_layer = 0;
        stale.total_layers = 32;
        stale.inference_port = Some(port);

        let health = super::AdmissionHealth::default();
        let clients = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));
        let reachable =
            ready_for_pipeline(&clients, &health, vec![stale], Duration::from_millis(100)).await;

        let peers = reachable;
        assert_eq!(peers[0].end_layer, 31);
        let plan = crate::pipeline::plan_pipeline(
            &peers,
            "qwen35",
            Some("d-1234abcd"),
            32,
            &crate::pipeline::LoadTracker::new(),
        )
        .await;
        assert!(
            plan.is_some(),
            "a cobertura confirmada pelo health deve planejar"
        );
    }

    #[tokio::test]
    async fn local_peer_preserves_the_active_deployment_selector() {
        let state = crate::gateway::testkit::state_with("self", vec![]).await;
        *state.local_assignment.write().await = Some(LayerAssignment {
            deploy_id: "deployment-42".into(),
            model: "qwen".into(),
            total_layers: 32,
            start_layer: 0,
            end_layer: 15,
            layers_count: 16,
            memory_used_mb: 0.0,
            memory_allocated_mb: 0.0,
        });

        let peers = state.all_peers().await;
        let local = peers
            .iter()
            .find(|peer| peer.node_id == "self")
            .expect("o gateway sempre inclui o peer local");
        assert_eq!(local.deploy_id, "deployment-42");
    }
}

#[cfg(test)]
mod replicated_admin_logs_tests {
    use super::*;
    use crate::gateway::testkit::body_of;
    use tower::ServiceExt;

    #[tokio::test]
    async fn secondary_lista_replicas_locais_sem_principal() {
        let state = testkit::secondary_admin_state("secondary").await;
        assert!(state.network.peers.read().await.is_empty());

        assert!(state
            .cluster_logs
            .try_ingest_remote(crate::cluster_logs::LogBatch {
                source_node_id: "worker".into(),
                entries: vec![crate::cluster_logs::LogEntry {
                    node_id: "worker".into(),
                    node_name: "worker-1".into(),
                    deploy_id: "deploy-1".into(),
                    origin_at: "2026-09-11T12:00:00Z".into(),
                    level: "INFO".into(),
                    target: "worker::test".into(),
                    message: "processo iniciou".into(),
                }],
                dropped: 0,
            }));

        let request = crate::reqlog::AuditRequest {
            id: "request-1".into(),
            received_at: "2026-09-11T12:00:00Z".into(),
            route: "/v1/chat/completions".into(),
            model: "model".into(),
            caller: crate::reqlog::Caller::operator(),
            entry_node_id: "worker".into(),
            prompt: "hello".into(),
            request_details: serde_json::json!({"stream": false}),
        };
        let completion = crate::reqlog::AuditCompletion {
            completed_at: "2026-09-11T12:00:01Z".into(),
            result: Some("world".into()),
            telemetry: None,
            timings: None,
            source: "distributed".into(),
            hops: vec!["worker".into()],
            prompt_tokens: Some(1),
            completion_tokens: Some(1),
            duration_ms: Some(1000),
            status: crate::reqlog::AuditStatus::Ok,
            error: None,
        };
        assert!(state
            .audit
            .try_ingest_remote(crate::reqlog::AuditEvent::Complete {
                id: request.id.clone(),
                completion,
            }));
        assert!(state
            .audit
            .try_ingest_remote(crate::reqlog::AuditEvent::Begin(request)));

        state.cluster_logs.flush();
        state.audit.flush();
        let logs = state
            .cluster_logs
            .list(Some("deploy-1"), Some(10))
            .await
            .expect("logs locais");
        let requests = state
            .audit
            .list(crate::reqlog::AuditQuery::default())
            .await
            .expect("auditoria local");

        assert_eq!(logs.entries.len(), 1);
        assert_eq!(logs.entries[0].message, "processo iniciou");
        assert_eq!(requests.entries.len(), 1);
        assert_eq!(requests.entries[0].id, "request-1");
        assert_eq!(requests.entries[0].status, "ok");
    }

    #[tokio::test]
    async fn secondary_admin_can_open_both_log_surfaces() {
        let state = testkit::secondary_admin_state("secondary").await;
        let app = Router::new()
            .merge(crate::webui::routes())
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                state,
                require_admin_for_surface,
            ));
        for (path, expected) in [
            ("/api/ui/cluster-logs", StatusCode::OK),
            ("/api/ui/requests", StatusCode::FORBIDDEN),
        ] {
            let mut request = Request::get(path)
                .header("authorization", "Bearer chave")
                .body(axum::body::Body::empty())
                .expect("requisição de teste");
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("127.0.0.1:1234".parse().unwrap()));
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected, "rota={path}");
        }
    }

    #[tokio::test]
    async fn plugin_status_defaults_to_inactive_and_request_logs_are_gated() {
        let state = testkit::state_with("enterprise-free", vec![]).await;
        let app = Router::new()
            .merge(crate::webui::routes())
            .with_state(state);

        let status = app
            .clone()
            .oneshot(
                Request::get("/api/ui/plugins")
                    .body(axum::body::Body::empty())
                    .expect("status request"),
            )
            .await
            .expect("status response");
        assert_eq!(status.status(), StatusCode::OK);
        let plugins: serde_json::Value = body_of(status).await;
        let enterprise = plugins
            .as_array()
            .expect("plugin list")
            .iter()
            .find(|plugin| plugin["code"] == "enterprise")
            .expect("enterprise plugin entry");
        assert_eq!(enterprise["active"], false);
        assert_eq!(enterprise["state"], "needs_activation");
        assert_eq!(enterprise["manage_url"], "https://pieceofmind.cloud/enterprise");

        let check = app
            .clone()
            .oneshot(
                Request::post("/api/ui/pom-update/check")
                    .header("authorization", "Bearer chave")
                    .body(axum::body::Body::empty())
                    .expect("check request"),
            )
            .await
            .expect("check response");
        assert_eq!(check.status(), StatusCode::OK);
        let check: serde_json::Value = body_of(check).await;
        assert_eq!(check["core_update"], serde_json::Value::Null);

        let request_logs = app
            .clone()
            .oneshot(
                Request::get("/api/ui/requests")
                    .header("authorization", "Bearer chave")
                    .body(axum::body::Body::empty())
                    .expect("logs request"),
            )
            .await
            .expect("logs response");
        assert_eq!(request_logs.status(), StatusCode::FORBIDDEN);
        let error: serde_json::Value = body_of(request_logs).await;
        assert_eq!(error["code"], "enterprise_required");

        let statistics = app
            .oneshot(
                Request::get("/api/ui/requests/stats")
                    .header("authorization", "Bearer chave")
                    .body(axum::body::Body::empty())
                    .expect("statistics request"),
            )
            .await
            .expect("statistics response");
        assert_eq!(statistics.status(), StatusCode::FORBIDDEN);
        let error: serde_json::Value = body_of(statistics).await;
        assert_eq!(error["code"], "enterprise_required");
        assert_eq!(error["manage_url"], "https://pieceofmind.cloud/enterprise");
    }

    #[tokio::test]
    async fn private_node_lists_a_federated_record_without_private_p2p_candidates() {
        let state = testkit::state_with("private-node", vec![]).await;
        state.auth.write().await.network.visibility = crate::config::NetworkVisibility::Private;
        let trust_document = serde_json::to_string(&state.admin_trust.document())
            .expect("documento de teste deve serializar");
        let network = crate::public_directory::PublicDirectoryNetwork {
            cluster_id: "test-cluster".into(),
            name: "Rede federada".into(),
            fingerprint: crate::admin_trust::anchor_fingerprint(&state.admin_trust.document()),
            address: "203.0.113.10:8080".into(),
            bootstrap_address: "203.0.113.10:42777".into(),
            trust_document,
            organization: "Teste".into(),
            description: "Registro remoto".into(),
            rules: "- Respeite os peers".into(),
            channels: "[Discord](https://discord.example/pom)".into(),
            country_code: "BR".into(),
            country_name: "Brasil".into(),
            city: "Curitiba".into(),
            geo_label: "br.curitiba".into(),
            access_policy: crate::config::NetworkAccessPolicy::AnyoneWithAnchor,
            location: None,
            directory_replicas: Vec::new(),
        };
        let record = crate::public_directory::SignedPublicDirectoryRecord::sign(
            &state.authority_signing_key,
            1,
            crate::public_directory::current_time_secs().unwrap(),
            network.clone(),
        )
        .expect("registro de teste deve ser assinado");
        let attacker = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let forged = crate::public_directory::SignedPublicDirectoryRecord::sign(
            &attacker,
            1,
            crate::public_directory::current_time_secs().unwrap(),
            network,
        )
        .expect("registro forjado deve ter uma prova de trabalho válida");
        let response = accept_public_directory_record(State(state.clone()), Json(forged)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state
            .database
            .public_directory_records()
            .unwrap()
            .is_empty());

        let payload = serde_json::json!({
            "version": crate::public_directory::PUBLIC_DIRECTORY_VERSION,
            "records": [record],
        });
        let replica = Router::new()
            .route(
                "/api/public/directory/records",
                get(move || async move { Json(payload) }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, replica).await.unwrap();
        });
        state
            .database
            .save_public_directory_settings(
                &crate::public_directory::PublicDirectorySettings::from_urls([format!(
                    "http://{address}"
                )])
                .unwrap(),
            )
            .unwrap();

        let response = public_networks(State(state.clone()), Query(PublicNetworksQuery::default()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let networks: Vec<PublicNetworkView> = serde_json::from_slice(&body).unwrap();
        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0].name, "Rede federada");
        assert_eq!(networks[0].rules, "- Respeite os peers");
        assert_eq!(
            networks[0].channels,
            "[Discord](https://discord.example/pom)"
        );
        assert_eq!(networks[0].discovered_via, "directory");
        assert_eq!(state.database.public_directory_records().unwrap().len(), 1);
        server.abort();
    }
}

/// Um nó de verdade, montado à mão, para os testes que exercitam rotas.
///
/// Existe porque três módulos precisavam do mesmo andaime — `transfer`,
/// `deploy` e `webui` todos testam handlers que só falam a verdade com um
/// `GatewayState` inteiro atrás deles. A tabela de peers é semeada à mão e o
/// socket de teste fica preso à loopback, então um teste nunca anuncia na LAN
/// nem esbarra num cluster de verdade.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::config::Config;
    use tokio::sync::RwLock;

    /// Um peer alcançável no `gateway_port` dado, na loopback.
    pub(crate) fn peer(node_id: &str, hostname: &str, gateway_port: u16) -> PeerInfo {
        let authority_pubkey = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[7; 32])
                .verifying_key()
                .as_bytes(),
        );
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: node_id.into(),
            pubkey: "pk".into(),
            authority_pubkey,
            cluster_id: None,
            addr: format!("127.0.0.1:{gateway_port}"),
            addr_candidates: Vec::new(),
            model: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: None,
            inference_quic_port: None,
            inference_frame_capabilities: Vec::new(),
            gateway_port: Some(gateway_port),
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Node,
            hostname: hostname.into(),
            capabilities: Vec::new(),
            connection_verified: true,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            libp2p_peer_id: Some("peer-eu".into()),
            tp: None,
            runtime_capacity: Some(RuntimeCapacity {
                n_ctx: 8_192,
                max_new_tokens: 256,
            }),
            idle: false,
        }
    }

    /// Este nó, com `peers` já na tabela.
    pub(crate) async fn state_with(node_id: &str, peers: Vec<PeerInfo>) -> GatewayState {
        state_named(node_id, "", peers).await
    }

    /// Um estado cujo nó local é admin secundário de uma cadeia real de teste.
    /// A âncora pertence a outro nó e a concessão local é assinada pelo
    /// principal antes de ser aplicada, então mudar apenas `profile` não pode
    /// transformar este estado em principal.
    pub(crate) async fn state_with_secondary_admin(
        node_id: &str,
        peers: Vec<PeerInfo>,
    ) -> GatewayState {
        let mut state = state_with(node_id, peers).await;
        let primary_signing_key = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let primary = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            "primary",
            "peer-primary",
            &primary_signing_key,
        );
        let secondary = state.authority_principal.clone();
        let admin_trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "test-cluster",
            primary.clone(),
            &primary_signing_key,
        )
        .expect("concessão raiz de teste");
        let grant = admin_trust
            .signed_secondary_grant(&primary_signing_key, secondary.clone(), &primary.node_id)
            .expect("concessão secundária de teste");
        admin_trust
            .apply_secondary_grant(grant, &secondary, crate::profile::NodeProfile::Node)
            .expect("aplicação da concessão secundária de teste");

        *state
            .network
            .authority_pubkey
            .write()
            .expect("chave de autoridade do testkit") = secondary.pubkey.clone();
        state
            .network
            .set_profile(crate::profile::NodeProfile::Admin);
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        state.admin_trust = Arc::new(admin_trust);
        state.cluster_logs.set_admin_receiver(true);
        state.audit.set_admin_receiver(true);
        state
    }

    /// Um admin secundário com uma cadeia de confiança válida, usado para
    /// provar que as leituras dependem da réplica local e não do principal.
    pub(crate) async fn secondary_admin_state(node_id: &str) -> GatewayState {
        state_with_secondary_admin(node_id, vec![]).await
    }

    /// O mesmo, com um **nome anunciado** — o que `node_name` do `pom.json`
    /// produz em `with_identity`, e o nome pelo qual o cluster inteiro chama
    /// esta máquina. Distinto do hostname cru do sistema operacional.
    pub(crate) async fn state_named(
        node_id: &str,
        hostname: &str,
        peers: Vec<PeerInfo>,
    ) -> GatewayState {
        let net = Arc::new(
            Network::testkit(node_id.into(), "pk".into(), vec![], None, None, None)
                .await
                .expect("socket UDP em porta efêmera")
                .with_identity(crate::profile::NodeProfile::Node, hostname.into()),
        );
        {
            let mut table = net.peers.write().await;
            for p in peers {
                table.insert(p.node_id.clone(), p);
            }
        }
        let assignment = Arc::new(RwLock::new(None));
        let database_path = std::env::temp_dir().join(format!(
            "pom-gateway-test-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let database = Arc::new(crate::db::PomDb::open(&database_path).expect("banco de teste"));
        let (capture, receiver) =
            crate::cluster_logs::LogCapture::new(crate::cluster_logs::LOG_CAPTURE_QUEUE_CAPACITY);
        let cluster_logs =
            crate::cluster_logs::ClusterLogs::new(capture, receiver, database.clone());
        let authority_signing_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let authority_principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            node_id,
            "peer-eu",
            &authority_signing_key,
        );
        let admin_trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "test-cluster",
            authority_principal.clone(),
            &authority_signing_key,
        )
        .expect("concessão raiz de teste");
        let auth = Arc::new(RwLock::new(Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            profile: crate::profile::NodeProfile::Node,
            setup_pending: false,
            local_auto_discovery: false,
            tls_required: false,
            quic_enabled: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            cluster_logs_min_level: Default::default(),
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
            connection_key: "chave".into(),
            gateway_port: 8080,
            admin_exposure: Default::default(),
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
            network: Default::default(),
            resources: Default::default(),
            setup_model: None,
        }));
        let (network_chat, _) =
            crate::network_chat::NetworkChat::new(database.clone(), auth.clone(), false);
        let plugins = crate::plugins::PluginManager::runtime(node_id);
        let pom_core_update = crate::pom_core_update::PomCoreUpdater::runtime(plugins.clone());
        GatewayState {
            network: net.clone(),
            ollama: Arc::new(OllamaClient::new("http://127.0.0.1:1")),
            default_model: String::new(),
            local_assignment: assignment.clone(),
            loads: Arc::new(LoadTracker::new()),
            prefix_cache: crate::prefix_cache::PrefixCache::default_arc(),
            prefix_disk: None,
            peer_http: Arc::new(crate::peer_client::PeerClients::new(
                std::time::Duration::from_secs(600),
            )),
            admission_health: Default::default(),
            runtime: NodeRuntime::new(net, assignment),
            cycle_config: crate::cycle::CycleConfig::default(),
            audit: crate::reqlog::AuditLog::new(database.clone()),
            plugins,
            pom_core_update,
            database,
            gateway_port: 8080,
            auth,
            admin_trust: Arc::new(admin_trust),
            authority_signing_key: Arc::new(authority_signing_key),
            authority_principal,
            admin_sessions: Arc::new(tokio::sync::Mutex::new(Default::default())),
            transfers: Default::default(),
            reply_inbox: crate::direct::ReplyInbox::new(),
            tunnel_tx: None,
            tls_server: None,
            quic_server: None,
            quic_port: None,
            cluster_logs,
            network_chat,
            cluster_cache: Arc::new(Mutex::new(Default::default())),
            model_service: Arc::new(crate::model_provider_backend::ModelService::new(Arc::new(
                crate::huggingface::HuggingFaceProvider::new(),
            ))),
            pkarr_publisher: None,
        }
    }

    /// Uma configuração de nó com a decodificação especulativa ligada — o
    /// interruptor do operador que o gateway lê antes de cada geração.
    pub(crate) fn spec_config() -> crate::runtime::NodeConfig {
        crate::runtime::NodeConfig {
            deploy_id: String::new(),
            model_path: String::new(),
            mmproj_path: None,
            mtp_draft_path: None,
            model_label: "lab".into(),
            deployment_name: "lab".into(),
            start_layer: 0,
            end_layer: 1,
            total_layers: 2,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_mode: Default::default(),
            inference_port: 8181,
            n_gpu_layers: 0,
            n_ctx: 8192,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            slot_save_path: None,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            prefill_max_inflight_chunks:
                crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS as u32,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            cycle_detection_enabled: true,
            cycle_detection_min_window_tokens: crate::cycle::DEFAULT_CYCLE_MIN_WINDOW_TOKENS,
            cycle_detection_max_window_tokens: crate::cycle::DEFAULT_CYCLE_MAX_WINDOW_TOKENS,
            cycle_detection_repetitions: crate::cycle::DEFAULT_CYCLE_REPETITIONS,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: true,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            mtp_cpu_fallback_enabled: true,
            unload_if_idle: false,
            idle_minutes: 60,
            tp: None,
            speculative_k: 4,
            speculative_lookup_window: 3,
        }
    }

    /// Faz o estado saber um endereço de si mesmo que responde de verdade.
    ///
    /// A resposta direta só existe quando a cauda tem para onde entregar
    /// (`DirectReply::new`), e o endereço sai de `self_reply_hosts` + o
    /// `gateway_port`. Sem um servidor ali o modo direto de um teste esperaria
    /// `DIRECT_REPLY_DEADLINE` inteiro.
    pub(crate) async fn with_self_reachable_gateway(mut state: GatewayState) -> GatewayState {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let inbox = state.reply_inbox.clone();
        let router = axum::Router::new().route(
            crate::direct::RELAY_RESULT_PATH,
            axum::routing::post(move |body: Bytes| {
                let inbox = inbox.clone();
                async move { crate::direct::handle_relay_result(&inbox, body) }
            }),
        );
        let inbox = state.reply_inbox.clone();
        let router = router.route(
            crate::direct::REMOTE_STREAM_RESULT_PATH,
            axum::routing::post(move |body: Bytes| {
                let inbox = inbox.clone();
                async move { crate::direct::handle_remote_stream_result(&inbox, body).await }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });

        state.gateway_port = port;
        let net = Network::testkit("eu".into(), "pk".into(), vec![], None, None, Some(port))
            .await
            .expect("socket UDP em porta efêmera");
        // Primeiro da lista de `self_reply_hosts`, então a cauda acerta na
        // primeira tentativa em vez de esperar o timeout de um IP de LAN.
        net.set_advertise_addr(Some("127.0.0.1:42777"));
        {
            let antigos = state.network.peers.read().await.clone();
            *net.peers.write().await = antigos;
        }
        state.network = Arc::new(net);
        state
    }

    /// O corpo JSON de uma resposta, para o teste poder afirmar sobre ele.
    pub(crate) async fn body_of(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("corpo legível");
        serde_json::from_slice(&bytes).expect("corpo JSON")
    }
}
