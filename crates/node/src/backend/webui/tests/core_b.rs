/// **A máquina tem um nome, e é um só.**
///
/// `node_name` do `pom.json` é como o operador batiza a máquina, e é o nome
/// que o nó anuncia aos peers (`network.hostname`). O catálogo respondia com
/// `sysmon`, o hostname cru do sistema operacional — e o seletor "Enviar
/// para..." da tela de fatias, que lê daí, era o único lugar do aplicativo que
/// chamava a máquina por outro nome. Numa máquina com dois nós era pior: os
/// dois relatavam o mesmo hostname do SO, a tela via colisão e desempatava
/// com o prefixo do `node_id`.
#[cfg(test)]
mod node_name_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_named};

    #[tokio::test]
    async fn o_catalogo_local_se_apresenta_pelo_nome_do_no() {
        let state = state_named("eu", "atena", vec![]).await;

        let corpo = body_of(models(State(state)).await.into_response()).await;

        assert_eq!(
            corpo["hostname"], "atena",
            "o catálogo tem que dizer o nome do nó, não o hostname do SO"
        );
    }

    #[tokio::test]
    async fn o_catalogo_do_cluster_tambem() {
        let state = state_named("eu", "atena", vec![]).await;

        let corpo = body_of(cluster_models(State(state)).await.into_response()).await;

        let eu = &corpo["peers"][0];
        assert_eq!(eu["is_self"], true);
        assert_eq!(eu["hostname"], "atena");
    }

    /// Um peer que não respondeu continua sendo uma máquina com nome: o que
    /// ele anunciou está na tabela local. Sem isto o objeto saía **sem
    /// `hostname`** e a tela caía no `node_id` — um prefixo hexadecimal no
    /// lugar do nome da máquina, que é o rótulo errado do relato.
    #[tokio::test]
    async fn um_peer_que_nao_respondeu_ainda_tem_nome() {
        // A porta do peer falso não tem ninguém ouvindo: a consulta falha.
        let mut mudo = peer("outro", "ksia", 1);
        mudo.hostname = "ksia".into();
        let state = state_named("eu", "atena", vec![mudo]).await;

        let corpo = body_of(cluster_models(State(state)).await.into_response()).await;

        let outro = corpo["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["node_id"] == "outro")
            .expect("o peer entra na lista mesmo sem responder");
        assert!(!outro["error"].is_null(), "e a falha continua reportada");
        assert_eq!(
            outro["hostname"], "ksia",
            "sem nome a tela cai no node_id, que é um prefixo hexadecimal"
        );
    }
}

/// `/api/ui/peer/:node_id/runtime` - como aquela máquina carregou o modelo.
#[cfg(test)]
mod peer_runtime_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};

    /// Um servidor de inferência que responde `/health` com a colocação.
    async fn fake_inference() -> u16 {
        let app = Router::new().route(
            "/health",
            get(|| async {
                Json(json!({
                    "status": "ok",
                    "offload_kqv": true,
                    "mtp_ready": false,
                    "mtp_allocation": null,
                    "mtp_unavailable_reason": "MTP context allocation failed",
                    "placement": { "n_layer": 1, "layers": [] },
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        port
    }

    #[tokio::test]
    async fn unknown_machine_is_a_404_and_not_an_empty_answer() {
        let state = state_with("eu", vec![]).await;
        let r = peer_runtime(State(state), Path("ninguem".to_string())).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    /// Máquina na rede sem servidor de inferência não é falha de rede: é o
    /// estado normal de quem não recebeu fatia. A tela precisa distinguir os
    /// dois, então isto responde 200 dizendo que não há o que mostrar.
    #[tokio::test]
    async fn a_machine_without_an_inference_server_is_not_an_error() {
        let state = state_with("eu", vec![peer("outro", "ksia", 9)]).await;
        let r = peer_runtime(State(state), Path("outro".to_string())).await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(body["loaded"], false);
        assert!(body["reason"].is_string(), "a tela precisa do motivo");
    }

    /// O `/health` do peer chega inteiro, sem passar por um tipo intermediário:
    /// um campo novo no servidor de inferência aparece na tela sem que esta
    /// rota mude.
    #[tokio::test]
    async fn the_peers_health_is_forwarded_whole() {
        let port = fake_inference().await;
        let mut p = peer("outro", "ksia", 9);
        p.inference_port = Some(port);
        let state = state_with("eu", vec![p]).await;

        let r = peer_runtime(State(state), Path("outro".to_string())).await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(body["loaded"], true);
        assert_eq!(body["health"]["offload_kqv"], true);
        assert_eq!(body["health"]["mtp_ready"], false);
        assert_eq!(body["health"]["mtp_allocation"], serde_json::Value::Null);
        assert_eq!(
            body["health"]["mtp_unavailable_reason"],
            "MTP context allocation failed"
        );
        assert_eq!(body["health"]["placement"]["n_layer"], 1);
    }
}

#[cfg(test)]
mod state_transport_tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header::{ETAG, IF_NONE_MATCH};

    #[test]
    fn the_etag_changes_when_the_ui_payload_changes() {
        let before = etag_for_json(&json!({ "jobs": [] }));
        let after = etag_for_json(&json!({
            "jobs": [{ "id": "job-1", "state": "running" }]
        }));

        assert_ne!(
            before, after,
            "uma mudança visível precisa invalidar a versão"
        );
    }

    #[test]
    fn the_etag_stays_when_the_ui_payload_does_not_change() {
        let first = etag_for_json(&json!({ "offers": [] }));
        let second = etag_for_json(&json!({ "offers": [] }));

        assert_eq!(first, second, "o mesmo retrato precisa manter a versão");
    }

    #[test]
    fn cluster_version_ignores_only_the_sampling_timestamp() {
        let first = json!({
            "node": { "system": { "sampled_at": "2026-01-01T00:00:00Z", "ram": { "used_mb": 10 } } }
        });
        let second = json!({
            "node": { "system": { "sampled_at": "2026-01-01T00:00:01Z", "ram": { "used_mb": 10 } } }
        });
        let changed = json!({
            "node": { "system": { "sampled_at": "2026-01-01T00:00:01Z", "ram": { "used_mb": 11 } } }
        });

        assert_eq!(cluster_etag(&first), cluster_etag(&second));
        assert_ne!(cluster_etag(&second), cluster_etag(&changed));
    }

    #[tokio::test]
    async fn an_matching_etag_returns_304_without_a_body() {
        let payload = json!({ "jobs": [] });
        let first = conditional_json(&HeaderMap::new(), &payload);
        let etag = first.headers().get(ETAG).cloned().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag);

        let second = conditional_json(&headers, &payload);
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert!(
            to_bytes(second.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty(),
            "304 não pode remandar o JSON"
        );
    }
}

#[cfg(test)]
mod chat_telemetry_tests {
    use super::*;
    use crate::gateway::testkit::peer;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn served_peer(node_id: &str, start_layer: u32, end_layer: u32) -> crate::network::PeerInfo {
        let mut peer = peer(node_id, node_id, 0);
        peer.model = "qwen35".into();
        peer.start_layer = start_layer;
        peer.end_layer = end_layer;
        peer.total_layers = 32;
        peer.inference_port = Some(8181);
        peer
    }

    #[test]
    fn execution_telemetry_keeps_only_requested_adjacent_edges() {
        let requested = ["a", "b", "c"].into_iter().map(str::to_string).collect();
        let trace = json!({
            "hops": [
                {"node_id": "a", "decode": {"transport_summary": {"transports": [{"transport": "https-link", "calls": 2, "rtt_median_ms": 8}]} }},
                {"node_id": "b", "decode": {"transport_summary": {"transports": [{"transport": "https-link", "calls": 2, "rtt_median_ms": 9}]} }},
                {"node_id": "outside-pipeline", "decode": {"transport_summary": {"transports": [{"transport": "libp2p-tunnel", "calls": 5, "rtt_median_ms": 99}]} }}
            ]
        });
        assert_eq!(
            execution_transport_measurements(&trace, &requested),
            vec![ChatTelemetryMeasurement {
                from_node_id: "a".into(),
                to_node_id: "b".into(),
                rtt_ms: 9.0,
                transport: "https-link".into(),
                relayed: false,
            }]
        );
    }

    #[test]
    fn execution_telemetry_uses_downstream_edge_not_gateway_probe() {
        let requested = ["head", "tail"].into_iter().map(str::to_string).collect();
        let trace = json!({
            "hops": [
                {
                    "node_id": "head",
                    "decode": {
                        "transport_summary": {
                            "transports": [{
                                "transport": "libp2p-tunnel",
                                "calls": 1,
                                "rtt_median_ms": 91
                            }]
                        }
                    }
                },
                {
                    "node_id": "tail",
                    "decode": {
                        "transport_summary": {
                            "transports": [{
                                "transport": "https-link",
                                "calls": 12,
                                "rtt_median_ms": 7
                            }]
                        }
                    }
                }
            ]
        });

        assert_eq!(
            execution_transport_measurements(&trace, &requested),
            vec![ChatTelemetryMeasurement {
                from_node_id: "head".into(),
                to_node_id: "tail".into(),
                rtt_ms: 7.0,
                transport: "https-link".into(),
                relayed: false,
            }]
        );
    }

    #[test]
    fn execution_telemetry_uses_the_real_directional_cycle() {
        let requested = ["a", "b", "c"].into_iter().map(str::to_string).collect();
        let trace = json!({
            "hops": [
                {"node_id": "a", "decode": {"processing_ms": 10}},
                {"node_id": "b", "decode": {"processing_ms": 12}},
                {"node_id": "c", "decode": {"processing_ms": 14}}
            ],
            "pipeline_cycle": {
                "decode": [
                    {"from_node_id": "gateway", "to_node_id": "a", "transport_summary": {"transports": [{"transport": "http", "calls": 1, "rtt_median_ms": 18}]}},
                    {"from_node_id": "a", "to_node_id": "b", "transport_summary": {"transports": [{"transport": "http", "calls": 4, "rtt_median_ms": 30}]}},
                    {"from_node_id": "b", "to_node_id": "c", "transport_summary": {"transports": [{"transport": "quic", "calls": 4, "rtt_median_ms": 31}]}},
                    {"from_node_id": "c", "to_node_id": "gateway", "transport_summary": {"transports": [{"transport": "https", "calls": 4, "rtt_median_ms": 29}]}}
                ]
            }
        });

        assert_eq!(
            execution_transport_measurements(&trace, &requested),
            vec![
                ChatTelemetryMeasurement {
                    from_node_id: "a".into(),
                    to_node_id: "b".into(),
                    rtt_ms: 30.0,
                    transport: "http".into(),
                    relayed: false,
                },
                ChatTelemetryMeasurement {
                    from_node_id: "b".into(),
                    to_node_id: "c".into(),
                    rtt_ms: 31.0,
                    transport: "quic".into(),
                    relayed: false,
                },
                ChatTelemetryMeasurement {
                    from_node_id: "c".into(),
                    to_node_id: "gateway".into(),
                    rtt_ms: 29.0,
                    transport: "https".into(),
                    relayed: false,
                },
            ]
        );
        assert!(execution_transport_measurements(&trace, &requested)
            .iter()
            .all(|measurement| measurement.transport != "libp2p-tunnel"));
    }

    #[test]
    fn execution_telemetry_network_panel_uses_wire_rtt_not_remote_processing() {
        let requested = ["head", "tail"].into_iter().map(str::to_string).collect();
        let trace = json!({
            "hops": [
                {"node_id": "head", "decode": {"processing_ms": 34}},
                {"node_id": "tail", "decode": {"processing_ms": 94}}
            ],
            "pipeline_cycle": {
                "decode": [
                    {"from_node_id": "head", "to_node_id": "tail", "transport_summary": {
                        "transports": [{
                            "transport": "quic",
                            "calls": 10,
                            "rtt_median_ms": 77,
                            "net_rtt_median_ms": 1
                        }]
                    }}
                ]
            }
        });

        assert_eq!(
            execution_transport_measurements(&trace, &requested),
            vec![ChatTelemetryMeasurement {
                from_node_id: "head".into(),
                to_node_id: "tail".into(),
                rtt_ms: 1.0,
                transport: "quic".into(),
                relayed: false,
            }]
        );
    }

    #[test]
    fn execution_pipeline_cost_sums_each_cycle_leg_once_and_adds_compute() {
        let trace = json!({
            "hops": [
                {"node_id": "a", "decode": {"processing_ms": 10}},
                {"node_id": "b", "decode": {"processing_ms": 12}},
                {"node_id": "c", "decode": {"processing_ms": 14}}
            ],
            "pipeline_cycle": {
                "decode": [
                    {"from_node_id": "gateway", "to_node_id": "a", "transport_summary": {"transports": [{"transport": "http", "calls": 1, "rtt_median_ms": 18}]}},
                    {"from_node_id": "a", "to_node_id": "b", "transport_summary": {"transports": [{"transport": "http", "calls": 4, "rtt_median_ms": 30}]}},
                    {"from_node_id": "b", "to_node_id": "c", "transport_summary": {"transports": [{"transport": "quic", "calls": 4, "rtt_median_ms": 31}]}},
                    {"from_node_id": "c", "to_node_id": "gateway", "transport_summary": {"transports": [{"transport": "https", "calls": 4, "rtt_median_ms": 29}]}}
                ]
            }
        });

        assert_eq!(
            execution_pipeline_cost(&trace),
            Some(ChatTelemetryPipelineCost {
                legs_ms: vec![Some(18.0), Some(30.0), Some(31.0), Some(29.0)],
                compute_ms: Some(36.0),
            })
        );
    }

    fn route_perf(
        decode_ms: f64,
        route: &[(&str, f64)],
        back: &[(&str, f64)],
    ) -> crate::fastest::PeerPerf {
        crate::fastest::PeerPerf {
            decode_ms_per_step: Some(decode_ms),
            route_rtt_ms: route.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            gateway_rtt_ms: back.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            route_kinds: route.iter().map(|(k, _)| (k.to_string(), "tunnel".to_string())).collect(),
            gateway_route_kinds: back.iter().map(|(k, _)| (k.to_string(), "quic".to_string())).collect(),
            ..Default::default()
        }
    }

    fn route_inputs<'a>(
        requester: &'a str,
        requester_rtt: &'a HashMap<String, f64>,
        perf: &'a HashMap<String, crate::fastest::PeerPerf>,
        load: &'a HashMap<String, u32>,
    ) -> crate::fastest::CostInputs<'a> {
        crate::fastest::CostInputs {
            requester,
            requester_rtt_ms: requester_rtt,
            perf,
            local_load: load,
            prompt_tokens: 1.0,
            decode_tokens: 1.0,
            forced_return: None,
        }
    }

    #[test]
    fn pipeline_routes_include_each_head_and_cost_the_network_like_the_planner() {
        let peers = vec![
            served_peer("head-fast", 0, 15),
            served_peer("head-slow", 0, 15),
            served_peer("tail", 16, 31),
        ];
        let requester_rtt = HashMap::from([
            ("head-fast".to_string(), 4.0),
            ("head-slow".to_string(), 18.0),
        ]);
        let perf = HashMap::from([
            ("head-fast".to_string(), route_perf(10.0, &[("tail", 12.0)], &[])),
            ("head-slow".to_string(), route_perf(10.0, &[("tail", 12.0)], &[])),
            ("tail".to_string(), route_perf(20.0, &[], &[("gateway", 3.0)])),
        ]);
        let load = HashMap::new();
        let inputs = route_inputs("gateway", &requester_rtt, &perf, &load);

        let routes = pipeline_routes_for(&peers, "qwen35", None, 32, &inputs);

        // Resposta direta: meio RTT de cada perna da ida e meio da volta.
        // head-fast: (4 + 12) / 2 + 3 / 2 = 9.5 ms, abaixo do anel (16 ms).
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].node_ids, vec!["head-fast", "tail", "gateway"]);
        assert_eq!(routes[0].legs_ms, vec![Some(4.0), Some(12.0), Some(3.0)]);
        assert_eq!(routes[0].rtt_ms, Some(19.0));
        assert_eq!(routes[0].network_ms, Some(9.5));
        assert_eq!(routes[0].return_mode, Some(crate::fastest::ReturnMode::Direct));
        assert_eq!(routes[0].compute_ms, Some(30.0));
        assert_eq!(routes[0].total_ms, Some(39.5));
        assert_eq!(routes[1].node_ids, vec!["head-slow", "tail", "gateway"]);
        assert_eq!(routes[1].total_ms, Some(46.5));

        let measurements = planned_route_measurements(&routes, &inputs);
        assert!(measurements.contains(&ChatTelemetryMeasurement {
            from_node_id: "head-fast".into(),
            to_node_id: "tail".into(),
            rtt_ms: 12.0,
            transport: "libp2p-tunnel".into(),
            relayed: false,
        }));
        assert!(measurements.contains(&ChatTelemetryMeasurement {
            from_node_id: "tail".into(),
            to_node_id: "gateway".into(),
            rtt_ms: 3.0,
            transport: "quic".into(),
            relayed: false,
        }));
    }

    #[test]
    fn a_slow_return_leg_is_costed_as_the_ring() {
        // O gateway é a cabeça; a volta da cauda é tão cara quanto a ida. O
        // card somava ida e volta inteiras (67 + 67); o planejador paga um
        // RTT por token, pelo anel.
        let peers = vec![served_peer("gateway", 0, 15), served_peer("tail", 16, 31)];
        let requester_rtt = HashMap::from([("tail".to_string(), 67.0)]);
        let perf = HashMap::from([
            ("gateway".to_string(), route_perf(25.0, &[("tail", 67.0)], &[])),
            ("tail".to_string(), route_perf(66.0, &[], &[("gateway", 80.0)])),
        ]);
        let load = HashMap::new();
        let inputs = route_inputs("gateway", &requester_rtt, &perf, &load);

        let routes = pipeline_routes_for(&peers, "qwen35", None, 32, &inputs);

        assert_eq!(routes[0].node_ids, vec!["gateway", "tail", "gateway"]);
        assert_eq!(routes[0].legs_ms, vec![Some(0.0), Some(67.0), Some(80.0)]);
        assert_eq!(routes[0].return_mode, Some(crate::fastest::ReturnMode::Ring));
        assert_eq!(routes[0].network_ms, Some(67.0));
        assert_eq!(routes[0].total_ms, Some(158.0));
    }

    #[test]
    fn a_tunnel_leg_through_a_relay_marks_the_route_and_the_edge() {
        let peers = vec![served_peer("head", 0, 15), served_peer("tail", 16, 31)];
        let requester_rtt = HashMap::from([("head".to_string(), 5.0)]);
        let mut head = route_perf(10.0, &[("tail", 67.0)], &[]);
        head.relayed_peers.insert("tail".into());
        let perf = HashMap::from([
            ("head".to_string(), head),
            ("tail".to_string(), route_perf(20.0, &[], &[("gateway", 3.0)])),
        ]);
        let load = HashMap::new();
        let inputs = route_inputs("gateway", &requester_rtt, &perf, &load);

        let routes = pipeline_routes_for(&peers, "qwen35", None, 32, &inputs);
        assert!(routes[0].relayed);
        let measurements = planned_route_measurements(&routes, &inputs);
        let edge = measurements
            .iter()
            .find(|m| m.from_node_id == "head" && m.to_node_id == "tail")
            .unwrap();
        assert!(edge.relayed);
        // A volta da cauda é QUIC direto: não é relé.
        let back = measurements
            .iter()
            .find(|m| m.from_node_id == "tail")
            .unwrap();
        assert!(!back.relayed);

        // Túnel com conexão direta (hole punch fechou) não é relé.
        let mut direct = perf.clone();
        direct.get_mut("head").unwrap().relayed_peers.clear();
        let inputs = route_inputs("gateway", &requester_rtt, &direct, &load);
        assert!(!pipeline_routes_for(&peers, "qwen35", None, 32, &inputs)[0].relayed);
    }

    #[test]
    fn pipeline_route_does_not_append_gateway_when_gateway_is_already_the_tail() {
        let peers = vec![served_peer("head", 0, 15), served_peer("gw", 16, 31)];
        let requester_rtt = HashMap::from([("head".to_string(), 23.0)]);
        let perf = HashMap::from([("head".to_string(), route_perf(10.0, &[("gw", 24.0)], &[]))]);
        let load = HashMap::new();
        let inputs = route_inputs("gw", &requester_rtt, &perf, &load);

        let routes = pipeline_routes_for(&peers, "qwen35", None, 32, &inputs);

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].node_ids, vec!["head", "gw"]);
        assert_eq!(routes[0].legs_ms, vec![Some(23.0), Some(24.0)]);
        assert_eq!(routes[0].rtt_ms, Some(47.0));
        // A última aresta é passagem de ida, não volta: no trace ela aparece
        // com o nó de destino, não como `gateway`.
        let measurements = planned_route_measurements(&routes, &inputs);
        assert_eq!(measurements.len(), 1);
        assert_eq!(measurements[0].to_node_id, "gw");
        assert_eq!(measurements[0].transport, "libp2p-tunnel");
    }

    #[tokio::test]
    async fn chat_telemetry_ignores_libp2p_probe_when_execution_trace_exists() {
        let state = crate::gateway::testkit::state_with("gateway", vec![]).await;
        let trace = json!({
            "hops": [
                {"node_id": "head", "decode": {"transport_summary": {"transports": [{"transport": "libp2p-tunnel", "calls": 1, "rtt_median_ms": 90}]} }},
                {"node_id": "tail", "decode": {"transport_summary": {"transports": [{"transport": "https-link", "calls": 4, "rtt_median_ms": 6}]} }}
            ]
        });
        state.audit.begin(crate::reqlog::AuditRequest {
            id: "chatcmpl-execution".into(),
            received_at: "2026-09-10T12:00:00Z".into(),
            route: "/v1/chat/completions".into(),
            model: "model.gguf".into(),
            caller: crate::reqlog::Caller::operator(),
            entry_node_id: "gateway".into(),
            prompt: "teste".into(),
            request_details: json!({"stream": true}),
        });
        state.audit.complete(
            "chatcmpl-execution".into(),
            crate::reqlog::AuditCompletion {
                completed_at: "2026-09-10T12:00:01Z".into(),
                result: Some("ok".into()),
                telemetry: Some(trace),
                timings: None,
                source: "pipeline:head->tail".into(),
                hops: vec!["head".into(), "tail".into()],
                prompt_tokens: Some(1),
                completion_tokens: Some(1),
                duration_ms: Some(6),
                status: crate::reqlog::AuditStatus::Ok,
                error: None,
            },
        );
        state.audit.begin(crate::reqlog::AuditRequest {
            id: "chatcmpl-unrelated".into(),
            received_at: "2026-09-10T12:00:02Z".into(),
            route: "/v1/chat/completions".into(),
            model: "other-model.gguf".into(),
            caller: crate::reqlog::Caller::operator(),
            entry_node_id: "gateway".into(),
            prompt: "outro teste".into(),
            request_details: json!({"stream": true}),
        });
        state.audit.complete(
            "chatcmpl-unrelated".into(),
            crate::reqlog::AuditCompletion {
                completed_at: "2026-09-10T12:00:02Z".into(),
                result: Some("ok".into()),
                telemetry: Some(json!({
                    "hops": [{"node_id": "unrelated", "decode": {}}
                    ]
                })),
                timings: None,
                source: "pipeline:unrelated".into(),
                hops: vec!["unrelated".into()],
                prompt_tokens: Some(1),
                completion_tokens: Some(1),
                duration_ms: Some(1),
                status: crate::reqlog::AuditStatus::Ok,
                error: None,
            },
        );
        state.audit.flush();
        state
            .network
            .rtts
            .remember_for_test("tail", 900.0, "libp2p");

        let app = routes().with_state(state);
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/ui/chat/telemetry")
                    .header("authorization", "Bearer chave")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "node_ids": ["head", "tail"],
                            "execution_id": "chatcmpl-execution"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(
            body["measurements"],
            json!([{
                "from_node_id": "head",
                "to_node_id": "tail",
                "rtt_ms": 6.0,
                "transport": "https-link"
            }])
        );

        let dashboard_response = app
            .oneshot(
                Request::post("/api/ui/chat/telemetry")
                    .header("authorization", "Bearer chave")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"node_ids": ["head", "tail"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(dashboard_response.status(), StatusCode::OK);
        let dashboard_body = crate::gateway::testkit::body_of(dashboard_response).await;
        assert_eq!(
            dashboard_body,
            json!({
                "measurements": [{
                    "from_node_id": "head",
                    "to_node_id": "tail",
                    "rtt_ms": 6.0,
                    "transport": "https-link"
                }]
            })
        );
    }
}

#[cfg(test)]
mod chat_history_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn chat_history_route_persists_and_deletes_authenticated_sessions() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let app = routes().with_state(state.clone());
        let payload = serde_json::json!({
            "id": "conversation-1",
            "title": "Teste",
            "titled": true,
            "model": "model.gguf",
            "modelAtStart": "model.gguf",
            "updatedAt": "2026-09-02T12:00:00Z",
            "messages": [{
                "id": "message-1",
                "role": "user",
                "content": "Olá",
                "at": "2026-09-02T11:59:00Z"
            }]
        });
        let saved = app
            .clone()
            .oneshot(
                Request::post("/api/ui/chat/history")
                    .header("authorization", "Bearer chave")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::OK);
        assert_eq!(state.database.chat_sessions().unwrap().len(), 1);

        let listed = app
            .clone()
            .oneshot(
                Request::get("/api/ui/chat/history")
                    .header("authorization", "Bearer chave")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(listed).await;
        assert_eq!(body[0]["messages"][0]["content"], "Olá");

        let deleted = app
            .oneshot(
                Request::delete("/api/ui/chat/history/conversation-1")
                    .header("authorization", "Bearer chave")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert!(state.database.chat_sessions().unwrap().is_empty());
    }
}

#[cfg(test)]
mod capacity_preview_tests {
    use super::*;

    fn gpu(index: u32, total_mb: f64, used_mb: f64) -> sysmon::GpuStat {
        sysmon::GpuStat {
            index,
            name: format!("gpu{index}"),
            vendor: sysmon::GpuVendor::Nvidia,
            vram_total_mb: Some(total_mb),
            vram_used_mb: Some(used_mb),
            vram_used_pct: None,
            utilization_pct: None,
            power_watts: None,
            temperature_c: None,
        }
    }

    #[test]
    fn single_gpu_takes_the_whole_projection() {
        let gpus = vec![gpu(0, 16_000.0, 2_000.0)];
        let (projections, fits, note) = split_projection(4_000 * 1024 * 1024, &gpus);
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].vram_used_mb, 2_000);
        assert_eq!(projections[0].vram_projected_mb, 6_000);
        assert!(fits);
        // Uma única GPU não precisa de nota de divisão estimada - não há
        // divisão nenhuma a fazer.
        assert!(note.is_none());
    }

    #[test]
    fn multiple_gpus_use_the_analytic_free_capacity_split() {
        let gpus = vec![gpu(0, 8_000.0, 0.0), gpu(1, 8_000.0, 0.0)];
        let (projections, fits, note) = split_projection(4_000 * 1024 * 1024, &gpus);
        assert_eq!(projections[0].vram_projected_mb, 2_000);
        assert_eq!(projections[1].vram_projected_mb, 2_000);
        assert!(fits);
        assert!(
            note.is_some(),
            "a divisão analítica precisa ser identificada"
        );
    }

    #[test]
    fn without_a_native_split_the_estimate_follows_free_capacity_and_says_so() {
        // GPU 0 já tem menos livre que a GPU 1, então recebe menos do peso
        // novo - e a nota tem que dizer que é estimativa.
        let gpus = vec![gpu(0, 8_000.0, 6_000.0), gpu(1, 8_000.0, 0.0)];
        let (projections, _fits, note) = split_projection(2_000 * 1024 * 1024, &gpus);
        let added0 = projections[0].vram_projected_mb - projections[0].vram_used_mb;
        let added1 = projections[1].vram_projected_mb - projections[1].vram_used_mb;
        // GPU 0 tinha 2 GiB livres contra 8 GiB da GPU 1, então recebe uma
        // fatia proporcionalmente menor do peso novo.
        assert!(added0 < added1, "added0={added0} added1={added1}");
        assert!(note.is_some());
    }

    #[test]
    fn a_projection_over_capacity_does_not_fit() {
        let gpus = vec![gpu(0, 4_000.0, 3_500.0)];
        let (projections, fits, _note) = split_projection(1_000 * 1024 * 1024, &gpus);
        assert!(projections[0].vram_projected_mb > projections[0].vram_total_mb);
        assert!(!fits);
    }

    #[test]
    fn a_machine_with_no_gpu_always_fits() {
        let (projections, fits, note) = split_projection(999 * 1024 * 1024, &[]);
        assert!(projections.is_empty());
        assert!(fits);
        assert!(note.is_none());
    }

    #[test]
    fn analytic_capacity_uses_amber_only_for_adjustable_default_layers() {
        assert_eq!(capacity_verdict(true, true, 99, 32), (true, false));
        assert_eq!(capacity_verdict(false, true, 99, 32), (true, true));
        assert_eq!(capacity_verdict(false, true, 16, 32), (false, false));
        assert_eq!(capacity_verdict(false, false, 99, 32), (false, false));
    }

    #[test]
    fn preview_admission_is_bounded_without_waiting() {
        let permits: Vec<_> = (0..CAPACITY_PREVIEW_MAX_IN_FLIGHT)
            .map(|_| try_capacity_preview_slot().expect("a vaga deveria estar disponível"))
            .collect();

        assert!(
            try_capacity_preview_slot().is_none(),
            "uma prévia excedente precisa ser recusada sem formar fila"
        );

        drop(permits);
        assert!(try_capacity_preview_slot().is_some());
    }
}
