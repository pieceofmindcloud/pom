#[cfg(test)]
use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_calcula_caminho_critico_sem_somar_spans_paralelos() {
        let spans = vec![
            TraceSpan::for_test("orchestration", "gateway", 0, 10, None),
            TraceSpan::for_test("route", "gateway", 10, 4, None),
            TraceSpan::for_test("prefill-a", "gateway", 14, 12, Some("prefill")),
            TraceSpan::for_test("prefill-b", "gateway", 14, 8, Some("prefill")),
            TraceSpan::for_test("node", "node-a", 0, 100, None),
        ];

        assert_eq!(critical_path_ms(&spans, "gateway"), 26);
    }

    #[test]
    fn trace_mantem_unknown_separado_e_nunca_fica_negativo() {
        let mut trace = RequestTrace::new("model", &[]);
        trace.set_outside_node_total_ms(20);
        trace.set_accounted_spans(vec![TraceSpan::for_test("dispatch", "gateway", 0, 7, None)]);
        assert_eq!(trace.outside_node_total_ms, Some(20));
        assert_eq!(trace.outside_node_accounted_ms, Some(7));
        assert_eq!(trace.outside_node_unaccounted_ms, Some(13));

        trace.set_outside_node_total_ms(3);
        assert_eq!(trace.outside_node_unaccounted_ms, Some(0));
    }

    #[test]
    fn trace_off_nao_emite_spans_nem_atribuicao() {
        let mut trace = RequestTrace::new("qwen", &[]);
        trace.profile = crate::telemetry::Profile::Off;
        trace.record_span(TraceSpan::for_test("validation", "gateway", 0, 4, None));
        trace.set_accounted_spans(vec![TraceSpan::for_test(
            "validation",
            "gateway",
            0,
            4,
            None,
        )]);

        assert!(trace.spans.is_empty());
        assert_eq!(trace.outside_node_accounted_ms, None);
        assert_eq!(trace.outside_node_unaccounted_ms, None);
    }

    #[test]
    fn refresh_nao_fabrica_outside_node_a_partir_do_e2e() {
        let mut trace = RequestTrace::new_with_trace_id(
            "trace",
            "qwen",
            &[("peer-a", 0, 15), ("peer-b", 16, 40)],
        );
        trace.complete = true;
        trace.hops[0].decode.processing_ms = 900;
        trace.hops[1].decode.processing_ms = 800;
        trace.refresh_telemetry(4_780);

        assert_eq!(trace.telemetry.wall_clock_ms, Some(4_780));
        assert_eq!(trace.telemetry.accumulated_compute_ms, 1_700);
        assert_eq!(trace.outside_node_total_ms, None);
        assert_eq!(trace.outside_node_accounted_ms, None);
        assert_eq!(trace.outside_node_unaccounted_ms, None);
    }

    #[tokio::test]
    async fn lote_oportunista_drena_o_que_ja_existe_sem_esperar() {
        let (sender, mut receiver) = mpsc::channel(REMOTE_STREAM_BATCH_LIMIT);
        for item in 0..REMOTE_STREAM_BATCH_LIMIT {
            sender.send(item).await.expect("item enfileirado");
        }
        let batch =
            drain_opportunistic_batch(receiver.recv().await.expect("primeiro item"), &mut receiver);
        assert_eq!(batch, (0..REMOTE_STREAM_BATCH_LIMIT).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn lote_oportunista_envia_um_item_quando_nao_ha_outro_pronto() {
        let (_sender, mut receiver) = mpsc::channel::<u32>(REMOTE_STREAM_BATCH_LIMIT);
        let batch = drain_opportunistic_batch(7, &mut receiver);
        assert_eq!(batch, vec![7]);
    }

    #[tokio::test]
    async fn cancelamento_acorda_chamada_em_voo() {
        let cancellation = Cancellation::new();
        cancellation.cancel();

        let error = cancelable(&cancellation, async {
            std::future::pending::<Result<(), anyhow::Error>>().await
        })
        .await
        .unwrap_err();
        assert!(is_cancelled(&error));
    }

    #[test]
    fn chat_head_frame_delivers_prompt_with_template_mode() {
        let meta = head_prefill_meta(
            "session",
            0,
            512,
            true,
            Some("Oi, teste"),
            true,
            false,
            false,
            None,
            Some("trace-a"),
        );
        assert_eq!(meta["prompt"], "Oi, teste");
        assert_eq!(meta["chat_template"], true);
        assert_eq!(meta["thinking_enabled"], false);
        assert_eq!(meta["trace_id"], "trace-a");
    }

    #[test]
    fn hub_preserva_posicoes_mrope_ao_encaminhar_estado_oculto() {
        let positions = json!([[0, 0, 0, 0], [0, 1, 1, 0]]);
        let meta = json!({"mrope_positions": positions.clone()});
        let mut body_meta = json!({"chunk_index": 0});

        copy_mrope_positions(&meta, &mut body_meta);

        assert_eq!(body_meta["mrope_positions"], positions);
    }

    #[test]
    fn missing_absolute_prefill_position_is_an_error_not_a_chunk_index() {
        let error = required_prefill_position(&json!({"start_pos": 42}), "next_pos", 1)
            .expect_err("a continuation without next_pos must not invent position 512");

        assert!(error.to_string().contains("next_pos"));
        assert!(error.to_string().contains("chunk 1"));
    }

    fn route_item(
        item_id: &str,
        model: &str,
        deployment_id: &str,
        phase: Phase,
        config_key: &str,
        next_hop_id: &str,
    ) -> BatchRouteItem {
        BatchRouteItem {
            item_id: item_id.into(),
            session_id: format!("session-{item_id}"),
            model: model.into(),
            deployment_id: deployment_id.into(),
            phase,
            config_key: config_key.into(),
            next_hop_id: next_hop_id.into(),
        }
    }

    #[test]
    fn batch_route_groups_only_items_with_the_same_hop_contract() {
        let groups = BatchRoute::group([
            route_item("a", "qwen", "deploy-1", Phase::Decode, "ctx-1", "peer-a"),
            route_item("b", "qwen", "deploy-1", Phase::Decode, "ctx-1", "peer-a"),
            route_item("c", "qwen", "deploy-1", Phase::Decode, "ctx-1", "peer-b"),
            route_item("d", "qwen", "deploy-2", Phase::Decode, "ctx-1", "peer-a"),
            route_item("e", "qwen", "deploy-1", Phase::Prefill, "ctx-1", "peer-a"),
            route_item("f", "qwen", "deploy-1", Phase::Decode, "ctx-2", "peer-a"),
        ]);

        assert_eq!(groups.len(), 5);
        assert_eq!(
            groups[0]
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(groups[0].next_hop_id, "peer-a");
        assert_eq!(groups[1].items[0].item_id, "c");
        assert_eq!(groups[2].items[0].item_id, "d");
        assert_eq!(groups[3].items[0].item_id, "e");
        assert_eq!(groups[4].items[0].item_id, "f");
    }

    #[test]
    fn batch_route_preserves_session_identity_inside_the_group() {
        let item = route_item(
            "request-1",
            "qwen",
            "deploy-1",
            Phase::Decode,
            "ctx",
            "peer-a",
        );
        let session_id = item.session_id.clone();
        let groups = BatchRoute::group([item]);
        assert_eq!(groups[0].items[0].session_id, session_id);
    }

    // O número de pedaços não é calculado em lugar nenhum: `run` avança até o
    // peer da cabeça dizer que não há mais (`more_chunks`), que é o único que
    // sabe quantos tokens o prompt virou. A aritmética que existia aqui, e os
    // testes dela, não tinham chamador na produção.

    #[test]
    fn hop_trace_separates_peer_compute_from_transport_time() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Prefill,
            18,
            7,
            1_024,
            2_048,
            1,
            2,
            3,
            4,
            TlsHop::default(),
            &json!({"decode_ms": 5}),
        );

        assert_eq!(trace.hops[0].prefill.processing_ms, 7);
        assert_eq!(trace.hops[0].prefill.network_ms, 11);
        assert_eq!(trace.hops[0].prefill.bytes_sent, 1_024);
        assert_eq!(trace.hops[0].prefill.bytes_received, 2_048);
    }

    #[test]
    fn trace_preserva_marcos_locais_do_primeiro_token_do_peer() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Prefill,
            18,
            7,
            1_024,
            2_048,
            1,
            2,
            3,
            4,
            TlsHop::default(),
            &json!({
                "first_token_produced_ms": 6.5,
                "first_token_serialized_ms": 7.0,
                "first_token_produced_at_ms": 1_700_000_000_006_u64,
                "first_token_serialized_at_ms": 1_700_000_000_007_u64,
            }),
        );

        assert_eq!(trace.engine_ttft_ms, Some(7));
        assert_eq!(trace.first_token.engine_ttft_ms, Some(7));
        assert_eq!(trace.first_token.node_serialized_ms, Some(7));
        assert_eq!(trace.first_token.node_to_gateway_ms, Some(7));
        assert_eq!(
            trace.first_token.engine_produced_at_ms,
            Some(1_700_000_000_006)
        );
        assert_eq!(
            trace.first_token.node_serialized_at_ms,
            Some(1_700_000_000_007)
        );
    }

    #[test]
    fn trace_nao_inventa_primeiro_token_em_hop_sem_marca() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);

        trace.record_hop(
            0,
            Phase::Prefill,
            18,
            7,
            1_024,
            2_048,
            1,
            2,
            3,
            4,
            TlsHop::default(),
            &json!({ "decode_ms": 5 }),
        );

        assert_eq!(trace.engine_ttft_ms, None);
        assert_eq!(trace.first_token, FirstTokenTrace::default());
    }

    #[test]
    fn remote_stream_aggregation_is_order_independent_and_deduplicates_frames() {
        let mut trace = RequestTrace::new(
            "qwen",
            &[("head", 0, 15), ("middle", 16, 31), ("tail", 32, 47)],
        );
        let frame = |index: u64, accepted: usize| {
            json!({
                "trace_id": "trace-1",
                "request_id": "request-1",
                "pipeline_id": "pipeline-1",
                "stream_id": "stream-1",
                "remote_stream_frame_index": index,
                "accepted": (0..accepted).map(|_| 1).collect::<Vec<_>>(),
                "stream_batch_bytes": 128,
                "remote_stream_frames_sent": index + 1,
                "remote_stream_frame_latency_ms": 9 + index,
                "remote_stream_remote_compute_ms": 4,
                "remote_stream_wait_ms": 20 + index,
                "remote_stream_producer_wait_ms": 7 + index,
                "remote_stream_channel_send_wait_ms": 1 + index,
                "remote_stream_downstream_wait_ms": 15 + index,
                "remote_stream_queue_wait_ms": 2 + index,
                "remote_stream_delivery_ms": 3 + index,
            })
        };
        let hop = |span_count: u64, processing_ms: u64| {
            json!({
                "processing_ms": processing_ms,
                "round_trip_ms": processing_ms + 2,
                "span_count": span_count,
            })
        };

        // Frame 2 arrives before frame 1. Arrival order must not change the
        // expected count or peer coverage, and a repeated frame is ignored.
        assert!(trace.record_remote_stream_batch(&frame(2, 1)));
        assert!(trace.record_remote_stream_batch(&frame(1, 2)));
        assert!(!trace.record_remote_stream_batch(&frame(1, 2)));
        for (span_count, processing_ms) in [(1, 7), (2, 8)] {
            for hop_index in 0..trace.hops.len() {
                trace.record_hop_from_json(
                    hop_index,
                    Phase::Decode,
                    &hop(span_count, processing_ms),
                );
            }
        }

        trace.complete = true;
        trace.refresh_telemetry(123);

        assert_eq!(trace.telemetry.expected_peers, 3);
        assert_eq!(trace.telemetry.reported_peers, 3);
        assert_eq!(trace.telemetry.expected_decode_spans, 9);
        assert_eq!(trace.telemetry.reported_decode_spans, 9);
        assert_eq!(trace.telemetry.completeness_percent, 100.0);
        assert_eq!(trace.telemetry.duplicate_spans, 1);
        assert_eq!(trace.telemetry.remote_stream.frames_received, 2);
        assert_eq!(trace.telemetry.remote_stream.bytes_received, 256);
        assert_eq!(
            trace.telemetry.remote_stream.remote_stream_wait_total_ms,
            43
        );
        assert_eq!(trace.telemetry.remote_stream.remote_stream_wait_p95_ms, 22);
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_producer_wait_total_ms,
            17
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_producer_wait_p95_ms,
            9
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_channel_send_wait_total_ms,
            5
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_channel_send_wait_p95_ms,
            3
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_downstream_wait_total_ms,
            33
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_downstream_wait_p95_ms,
            17
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_queue_wait_total_ms,
            7
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_queue_wait_p95_ms,
            4
        );
        assert_eq!(
            trace
                .telemetry
                .remote_stream
                .remote_stream_delivery_total_ms,
            9
        );
        assert_eq!(
            trace.telemetry.remote_stream.remote_stream_delivery_p95_ms,
            5
        );
        assert_eq!(trace.telemetry.status, "complete");
    }

    #[test]
    fn missing_remote_peer_stays_response_complete_but_not_trace_complete() {
        let mut trace = RequestTrace::new(
            "qwen",
            &[("head", 0, 15), ("middle", 16, 31), ("tail", 32, 47)],
        );
        trace.record_remote_stream_batch(&json!({
            "stream_id": "stream-2",
            "remote_stream_frame_index": 0,
            "accepted": [1, 2],
        }));
        for hop_index in 0..2 {
            trace.record_hop_from_json(
                hop_index,
                Phase::Decode,
                &json!({"span_count": 2, "node_timings": {}}),
            );
        }

        trace.complete = true;
        trace.refresh_telemetry(800);

        assert_eq!(trace.telemetry.status, "response_complete");
        assert!(trace.telemetry.response_complete);
        assert_eq!(trace.telemetry.reported_peers, 2);
        assert_eq!(trace.telemetry.missing_peers, vec!["tail"]);
        assert_eq!(trace.telemetry.expected_decode_spans, 6);
        assert_eq!(trace.telemetry.reported_decode_spans, 4);
        assert!(trace.telemetry.completeness_percent < 100.0);
    }

    #[test]
    fn remote_stream_open_ack_is_not_counted_as_a_decode_span() {
        let mut trace = RequestTrace::new("qwen", &[("head", 0, 15), ("tail", 16, 31)]);
        trace.record_hop(
            1,
            Phase::Prefill,
            12,
            5,
            100,
            200,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({}),
        );
        trace.record_control_transport(
            0,
            Phase::Decode,
            TransportTrace {
                transport: crate::relay::InferenceTransport::Libp2pTunnel,
                fallback: false,
                round_trip_ms: 20,
                network_ms: 20,
                bytes_sent: 40,
                bytes_received: 60,
                request_body_send_us: None,
            },
            40,
            60,
        );
        trace.record_remote_stream_batch(&json!({
            "stream_id": "stream-3",
            "remote_stream_frame_index": 0,
            "accepted": [1],
        }));
        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({"span_count": 1, "node_timings": {}}),
        );

        trace.complete = true;
        trace.refresh_telemetry(100);

        assert_eq!(trace.hops[0].decode.calls, 1);
        assert_eq!(trace.telemetry.expected_decode_spans, 2);
        assert_eq!(trace.telemetry.reported_decode_spans, 1);
        assert_eq!(trace.telemetry.reported_peers, 1);
        assert_eq!(trace.telemetry.missing_peers, vec!["tail"]);
        assert_eq!(trace.telemetry.status, "response_complete");
    }

    #[test]
    fn remote_stream_https_fallback_preserves_count_and_reason() {
        let mut trace = RequestTrace::new("qwen", &[("head", 0, 15)]);
        trace.record_remote_stream_batch(&json!({
            "stream_id": "stream-fallback",
            "remote_stream_frame_index": 0,
            "accepted": [1],
            "remote_stream_transport_fallback": true,
            "remote_stream_transport_fallback_reason": "https-link: canal expirou",
        }));

        assert_eq!(trace.https_fallback_calls, 1);
        assert_eq!(
            trace.https_fallback_reasons,
            vec!["https-link: canal expirou".to_string()]
        );
    }

    #[test]
    fn profiling_trace_keeps_frame_parent_and_token_events_bounded() {
        let mut trace = RequestTrace::new("qwen", &[("head", 0, 15)]);
        trace.profiling_spans = Some(Vec::new());
        trace.record_remote_stream_batch(&json!({
            "trace_id": "trace-profile",
            "request_id": "request-profile",
            "pipeline_id": "head",
            "stream_id": "stream-profile",
            "remote_stream_frame_index": 4,
            "remote_stream_lifetime_ms": 12,
            "remote_stream_frame_latency_ms": 3,
            "accepted": [1, 2],
        }));

        let spans = trace.profiling_spans.as_ref().unwrap();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].token_index, None);
        assert_eq!(spans[1].token_index, Some(0));
        assert_eq!(spans[2].token_index, Some(1));
        assert_eq!(spans[1].parent_span_id, Some(spans[0].span_id.clone()));
        assert_eq!(spans[1].duration_ns, 0);
        assert_eq!(spans[2].duration_ns, 0);
    }

    #[test]
    fn profiling_token_indices_continue_across_remote_stream_callbacks() {
        let frame = |index: u64| {
            json!({
                "trace_id": "trace-profile-merge",
                "request_id": "request-profile-merge",
                "pipeline_id": "head",
                "stream_id": "stream-profile-merge",
                "remote_stream_frame_index": index,
                "accepted": [1, 2],
            })
        };
        let mut first = RequestTrace::new("qwen", &[("head", 0, 15)]);
        first.profiling_spans = Some(Vec::new());
        first.record_remote_stream_batch(&frame(0));
        let mut second = RequestTrace::new("qwen", &[("head", 0, 15)]);
        second.profiling_spans = Some(Vec::new());
        second.record_remote_stream_batch(&frame(1));

        first.merge(&second);

        let token_indices = first
            .profiling_spans
            .unwrap()
            .into_iter()
            .filter_map(|span| span.token_index)
            .collect::<Vec<_>>();
        assert_eq!(token_indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn profiling_events_nameiam_o_caminho_critico_de_um_token() {
        let spans = ["decode", "transport-round-trip", "sampling", "client-emit"]
            .into_iter()
            .map(|name| TelemetrySpan {
                trace_id: "trace-critical".into(),
                request_id: "request-critical".into(),
                pipeline_id: "head>tail".into(),
                peer_id: "head".into(),
                span_id: format!("span-{name}"),
                parent_span_id: None,
                name: name.into(),
                phase: Phase::Decode,
                token_index: Some(7),
                start_ns: 100,
                end_ns: 200,
                duration_ns: 100,
                transport: Some("https-link".into()),
                clock_domain: "peer-monotonic".into(),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            spans
                .iter()
                .map(|span| span.name.as_str())
                .collect::<Vec<_>>(),
            vec!["decode", "transport-round-trip", "sampling", "client-emit"]
        );
        assert!(spans.iter().all(|span| span.token_index == Some(7)));
    }

    #[test]
    fn trace_agrega_aceitacao_mtp_por_requisicao() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        let node_timings = |proposed, accepted, rejected| {
            json!({
                "mtp_draft_tokens_proposed": proposed,
                "mtp_draft_tokens_accepted": accepted,
                "mtp_draft_tokens_rejected": rejected,
            })
        };

        trace.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &node_timings(7, 5, 2),
        );
        trace.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &node_timings(3, 2, 1),
        );

        assert_eq!(trace.mtp_draft_tokens_proposed, 10);
        assert_eq!(trace.mtp_draft_tokens_accepted, 7);
        assert_eq!(trace.mtp_draft_tokens_rejected, 3);
        assert_eq!(trace.mtp_acceptance_rate, Some(0.7));

        let serialized = serde_json::to_value(&trace).unwrap();
        assert_eq!(serialized["mtp_draft_tokens_proposed"], 10);
        assert_eq!(serialized["mtp_draft_tokens_accepted"], 7);
        assert_eq!(serialized["mtp_draft_tokens_rejected"], 3);
        assert_eq!(serialized["mtp_acceptance_rate"], 0.7);

        let no_mtp = serde_json::to_value(RequestTrace::new("qwen", &[("no1", 0, 15)])).unwrap();
        assert!(no_mtp["mtp_acceptance_rate"].is_null());
    }

    /// O resumo dizia "não medido" enquanto a tabela por salto mostrava as
    /// chamadas: o laço de continuação funde uma perna na outra, e a fusão
    /// carregava `PhaseTrace::calls` mas não o contador solto do resumo.
    #[test]
    fn trace_deriva_chamadas_de_decode_dos_saltos_depois_da_fusao() {
        fn perna(chamadas: u32) -> RequestTrace {
            let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15), ("no2", 16, 31)]);
            for _ in 0..chamadas {
                for hop in 0..2 {
                    trace.record_hop(
                        hop,
                        Phase::Decode,
                        1,
                        1,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        TlsHop::default(),
                        &json!({}),
                    );
                }
            }
            trace
        }

        let mut trace = perna(40);
        trace.merge(&perna(24));
        trace.set_generated_tokens(119);

        // Máximo entre os saltos, não a soma: uma rodada de decode toca os dois
        // saltos, e somar dobraria as chamadas num pipeline de dois nós.
        assert_eq!(trace.decode_calls, 64);
        assert_eq!(trace.hops[0].decode.calls, 64);
        assert_eq!(
            trace.tokens_per_decode_call,
            Some(119.0 / 64.0),
            "o resumo tem que trazer o rendimento por chamada, não `None`"
        );

        let serializado = serde_json::to_value(&trace).unwrap();
        assert_eq!(serializado["decode_calls"], 64);
        assert_eq!(serializado["generated_tokens"], 119);
    }

    #[test]
    fn trace_funde_pernas_mtp_com_taxa_ponderada() {
        let mut first = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        first.record_hop(
            0,
            Phase::Decode,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({
                "mtp_draft_tokens_proposed": 10,
                "mtp_draft_tokens_accepted": 7,
                "mtp_draft_tokens_rejected": 3,
            }),
        );
        let mut second = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        second.record_hop(
            0,
            Phase::Decode,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({
                "mtp_draft_tokens_proposed": 10,
                "mtp_draft_tokens_accepted": 10,
                "mtp_draft_tokens_rejected": 0,
            }),
        );

        first.merge(&second);

        assert_eq!(first.mtp_draft_tokens_proposed, 20);
        assert_eq!(first.mtp_draft_tokens_accepted, 17);
        assert_eq!(first.mtp_draft_tokens_rejected, 3);
        assert_eq!(first.mtp_acceptance_rate, Some(0.85));
    }

    #[test]
    fn hop_trace_does_not_claim_a_dtype_when_no_hidden_state_crossed() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);

        trace.record_hidden_state(0, Phase::Decode, 0, 0, Some(WireDtype::F8));

        assert_eq!(trace.hops[0].decode.hidden_state_dtype, None);
        assert_eq!(trace.hops[0].decode.hidden_state_bytes_sent, 0);
        assert_eq!(trace.hops[0].decode.hidden_state_bytes_received, 0);
    }

    /// A espera pelo mutex do motor tem que chegar a telemetria com nome.
    ///
    /// Numa medicao de 2026-08-14 ela era 194 ms por token - 63% do tempo da
    /// requisicao - e aparecia na tela apenas como "nao atribuido", porque
    /// caia no vao entre a entrada do handler e o primeiro medidor. Um numero
    /// grande sem nome e pior que um numero grande: manda procurar no lugar
    /// errado.
    #[test]
    fn the_wait_for_the_engine_lock_reaches_the_trace() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Decode,
            250,
            240,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({"decode_ms": 34, "lock_wait_ms": 194}),
        );

        assert_eq!(trace.hops[0].decode.node_lock_wait_ms, 194.0);
        assert_eq!(trace.hops[0].decode.node_decode_ms, 34.0);
    }

    #[test]
    fn mtp_draft_is_reported_as_synchronous_node_compute() {
        let mut trace = RequestTrace::new("qwen", &[("tail", 0, 15)]);
        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 78,
                "processing_ms": 78,
                "node_timings": {
                    "decode_ms": 19.0,
                    "sampling_ms": 2.0,
                    "mtp_draft_ms": 54.0,
                    "encode_ms": 1.0,
                },
            }),
        );

        assert_eq!(trace.hops[0].decode.node_mtp_draft_ms, 54.0);
        assert_eq!(trace.hops[0].decode.node_decode_ms, 19.0);
    }

    #[test]
    fn hop_trace_preserva_fracao_dos_tempos_do_no() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);

        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 2,
                "processing_ms": 1,
                "bytes_sent": 1,
                "bytes_received": 2,
                "node_timings": {
                    "decode_ms": 0.999,
                    "backend_sync_ms": 0.999,
                },
            }),
        );

        let phase = &trace.hops[0].decode;
        assert!((phase.node_decode_ms as f64 - 0.999).abs() < 1e-12);
        assert!((phase.node_backend_sync_ms as f64 - 0.999).abs() < 1e-12);
    }

    #[test]
    fn hop_trace_publica_o_motor_real_e_nao_inventa_o_de_peer_antigo() {
        let mut moe = RequestTrace::new("qwen", &[("moe", 0, 15)]);
        moe.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({
                "selected_path": "moe/q4_0",
                "executed_engine": "moe",
            }),
        );
        assert_eq!(
            moe.hops[0].decode.executed_engine,
            crate::executor::ExecutionEngine::Moe
        );
        assert_eq!(
            moe.hops[0].decode.selected_path.as_deref(),
            Some("moe/q4_0")
        );

        let mut delegated = RequestTrace::new("qwen", &[("delegated", 0, 15)]);
        delegated.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({
                "selected_path": "moe/q4_0",
                "executed_engine": "llama.cpp",
            }),
        );
        assert_eq!(
            delegated.hops[0].decode.executed_engine,
            crate::executor::ExecutionEngine::LlamaCpp
        );
        assert!(delegated.hops[0].decode.engine_mismatch);

        let mut old = RequestTrace::new("qwen", &[("old", 0, 15)]);
        old.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({}),
        );
        assert_eq!(
            old.hops[0].decode.executed_engine,
            crate::executor::ExecutionEngine::Unknown
        );
        assert!(!old.hops[0].decode.engine_mismatch);
    }

    #[test]
    fn hop_trace_propaga_executor_moe_dtype_e_motivo_por_fase() {
        let mut trace = RequestTrace::new("qwen", &[("moe", 0, 39)]);
        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 10,
                "processing_ms": 9,
                "bytes_sent": 1,
                "bytes_received": 2,
                "node_timings": {
                    "selected_path": "moe/q4_0",
                    "executed_engine": "moe",
                    "moe_executor": "cpu-reference",
                    "moe_executors": ["cpu-reference"],
                    "moe_executor_dtypes": [21, 22, 23],
                    "moe_executor_reason": "dtypes IQ3_S, IQ2_S e IQ4_XS sem ponte CUDA; CPUReference selecionado"
                }
            }),
        );

        let phase = &trace.hops[0].decode;
        assert_eq!(phase.moe_executor.as_deref(), Some("cpu-reference"));
        assert_eq!(phase.moe_executors, vec!["cpu-reference"]);
        assert_eq!(phase.moe_executor_dtypes, vec![21, 22, 23]);
        assert_eq!(
            phase.moe_executor_reason.as_deref(),
            Some("dtypes IQ3_S, IQ2_S e IQ4_XS sem ponte CUDA; CPUReference selecionado")
        );
    }

    #[test]
    fn hop_trace_fica_desconhecido_se_as_chamadas_do_mesmo_hop_divergirem() {
        let mut trace = RequestTrace::new("qwen", &[("mixed", 0, 15)]);
        for (path, engine) in [("moe/q4_0", "llama.cpp"), ("moe/q4_0", "moe")] {
            trace.record_hop(
                0,
                Phase::Prefill,
                1,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                TlsHop::default(),
                &json!({
                    "selected_path": path,
                    "executed_engine": engine,
                }),
            );
        }
        assert_eq!(
            trace.hops[0].prefill.executed_engine,
            crate::executor::ExecutionEngine::Unknown
        );
        assert!(trace.hops[0].prefill.mixed_execution);
        assert!(!trace.hops[0].prefill.engine_mismatch);
    }

    /// Ring mode reconstructs a trace from a bubbled `hop_timings` array
    /// entry (`record_hop_from_json`) instead of from a live HTTP response
    /// (`record_hop`) — the two have to agree exactly, or ring and hub mode
    /// would report different numbers for the identical request.
    #[test]
    fn record_hop_from_json_matches_record_hop() {
        let node = json!({"decode_ms": 10, "residual_ms": 2});

        let mut direct = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        direct.record_hop(
            0,
            Phase::Decode,
            70,
            13,
            100,
            200,
            1,
            2,
            3,
            4,
            TlsHop::default(),
            &node,
        );

        let mut from_json = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        from_json.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 70,
                "processing_ms": 13,
                "bytes_sent": 100,
                "bytes_received": 200,
                "client_encode_ms": 1,
                "response_headers_ms": 2,
                "response_body_ms": 3,
                "client_decode_ms": 4,
                "node_timings": node,
            }),
        );

        assert_eq!(direct.hops[0], from_json.hops[0]);
        assert_eq!(direct.total_bytes_sent, from_json.total_bytes_sent);
        assert_eq!(direct.total_bytes_received, from_json.total_bytes_received);
    }

    /// A missing `hop_timings` entry (index past the plan's step count) must
    /// be a no-op, same as `record_hop` already is — a malformed or short
    /// bubble must not panic on `self.hops[hop]`.
    #[test]
    fn record_hop_from_json_on_an_unknown_index_is_a_no_op() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop_from_json(5, Phase::Decode, &json!({"round_trip_ms": 70}));
        assert_eq!(trace.total_bytes_sent, 0);
    }

    #[test]
    fn merged_token_traces_keep_per_call_timings_and_percentiles() {
        let mut request = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        let mut token = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        token.record_hop(
            0,
            Phase::Decode,
            30,
            20,
            100,
            200,
            2,
            25,
            1,
            1,
            TlsHop::default(),
            &json!({"decode_ms": 11, "residual_ms": 4, "top_p_ms": 3, "backend_sync_ms": 9}),
        );

        request.merge(&token);

        let hop = &request.hops[0];
        assert_eq!(hop.decode.calls, 1);
        assert_eq!(hop.decode.client_encode_ms, 2);
        assert_eq!(hop.decode.node_decode_ms, 11.0);
        assert_eq!(hop.decode.node_residual_ms, 4.0);
        assert_eq!(hop.decode.node_top_p_ms, 3.0);
        assert_eq!(hop.decode.node_backend_sync_ms, 9.0);
        assert_eq!(hop.decode.node_p50_ms, 11.0);
        assert_eq!(hop.decode.node_p95_ms, 11.0);
        // A decode-only trace must leave prefill untouched.
        assert_eq!(hop.prefill.calls, 0);
        assert_eq!(hop.prefill.node_decode_ms, 0.0);
    }

    #[test]
    fn hop_trace_keeps_prefill_and_decode_transport_independent() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Prefill,
            10,
            6,
            100,
            200,
            1,
            8,
            1,
            0,
            TlsHop::default(),
            &json!({}),
        );
        trace.record_hop(
            0,
            Phase::Decode,
            20,
            9,
            10,
            20,
            1,
            18,
            1,
            0,
            TlsHop::default(),
            &json!({}),
        );

        assert_eq!(trace.hops[0].prefill.network_ms, 4);
        assert_eq!(trace.hops[0].decode.network_ms, 11);
    }

    #[test]
    fn hop_trace_keeps_the_observed_round_trip_for_execution_summary() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Prefill,
            18,
            7,
            100,
            200,
            1,
            8,
            1,
            0,
            TlsHop::default(),
            &json!({}),
        );

        assert_eq!(trace.hops[0].prefill.round_trip_ms, 18);
    }

    #[test]
    fn quic_transport_rtt_is_separate_from_remote_processing() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop_from_json(
            0,
            Phase::Prefill,
            &json!({
                "transport": "quic",
                "round_trip_ms": 76,
                "processing_ms": 75,
                "transport_rtt_us": 500,
                "request_body_send_us": 1_417,
                "node_timings": {},
            }),
        );
        let phase = &trace.hops[0].prefill;
        assert_eq!(phase.round_trip_ms, 76);
        assert_eq!(phase.processing_ms, 75);
        assert_eq!(phase.network_ms, 1);
        assert_eq!(phase.transport_summary.transports[0].rtt_median_ms, 76);
        assert_eq!(phase.transport_summary.transports[0].net_rtt_median_ms, 1);
        assert_eq!(
            phase.transport_summary.transports[0].quic_request_body_send_us,
            1_417
        );
        assert_eq!(
            phase.transport_summary.transports[0].quic_request_body_send_calls,
            1
        );
    }

    #[test]
    fn residual_capture_stages_aggregate_without_double_counting() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        for _ in 0..2 {
            trace.record_hop_from_json(
                0,
                Phase::Decode,
                &json!({
                    "round_trip_ms": 2,
                    "processing_ms": 2,
                    "node_timings": {
                        "residual_capture_d2h_sync_ms": 1.5,
                        "residual_capture_ready_wait_ms": 1.0,
                        "residual_capture_tensor_get_ms": 0.5,
                        "residual_capture_postprocess_ms": 0.1,
                        "residual_capture_bytes": 4096,
                        "residual_capture_copies": 1,
                        "residual_wire_f8_conversion_ms": 0.2,
                        "residual_wire_frame_serialize_ms": 0.1,
                        "residual_wire_encode_calls": 1
                    }
                }),
            );
        }

        let phase = &trace.hops[0].decode;
        assert_eq!(phase.node_residual_capture_d2h_sync_ms, 3.0);
        assert_eq!(phase.node_residual_capture_ready_wait_ms, 2.0);
        assert_eq!(phase.node_residual_capture_tensor_get_ms, 1.0);
        assert_eq!(
            phase.node_residual_capture_d2h_sync_ms,
            phase.node_residual_capture_ready_wait_ms + phase.node_residual_capture_tensor_get_ms
        );
        assert_eq!(phase.node_residual_capture_postprocess_ms, 0.2);
        assert_eq!(phase.node_residual_capture_bytes, 8192);
        assert_eq!(phase.node_residual_capture_copies, 2);
        assert_eq!(phase.node_residual_wire_f8_conversion_ms, 0.4);
        assert_eq!(phase.node_residual_wire_frame_serialize_ms, 0.2);
        assert_eq!(phase.node_residual_wire_encode_calls, 2);
    }

    #[test]
    fn direct_transport_uses_processing_from_hop_timing_when_ack_has_none() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        super::pipeline::record_direct_hops(
            &mut trace,
            Phase::Prefill,
            &json!({
                "hop_timings": [{
                    "round_trip_ms": 20,
                    "processing_ms": 17,
                    "node_timings": {}
                }],
                "transport_traversals": [{
                    "hop_index": 0,
                    "transport": "http",
                    "round_trip_ms": 20,
                    "processing_ms": 0
                }]
            }),
            None,
        );

        let phase = &trace.hops[0].prefill;
        assert_eq!(phase.processing_ms, 17);
        assert_eq!(phase.network_ms, 3);
        assert_eq!(phase.transport_summary.transports[0].rtt_median_ms, 20);
        assert_eq!(phase.transport_summary.transports[0].net_rtt_median_ms, 3);
    }

    #[tokio::test]
    async fn late_direct_transport_receipts_are_reconciled_without_waiting_on_the_reply() {
        let inbox = crate::direct::ReplyInbox::new();
        let direct = crate::direct::DirectReply::new(
            inbox.clone(),
            vec!["http://127.0.0.1:8080".into()],
            "gateway".into(),
        )
        .unwrap();
        let ticket = direct.ticket_for_hops(1);
        let ticket_id = ticket.id().to_owned();
        let mut meta = json!({
            "token_id": 7,
            "hop_timings": [{
                "round_trip_ms": 23,
                "processing_ms": 20,
                "node_timings": {}
            }]
        });
        assert!(inbox.deliver(&ticket_id, meta.clone()));
        meta = inbox.trace_snapshot(&ticket_id).unwrap();

        let mut trace = RequestTrace::new("qwen", &[("peer-1", 0, 15)]);
        trace.track_direct_receipt(direct.trace_inbox(), ticket_id.clone(), Phase::Decode);
        super::pipeline::record_direct_hops(&mut trace, Phase::Decode, &meta, Some(&ticket_id));
        assert!(trace.transport_trace_complete);

        assert!(inbox.record_transport(
            &ticket_id,
            0,
            json!({
                "hop_index": 0,
                "transport": "https",
                "round_trip_ms": 23,
                "processing_ms": 0
            }),
        ));
        trace.reconcile_direct_receipts();

        assert!(trace.transport_trace_complete);
        assert_eq!(trace.hops[0].decode.transport_summary.transports.len(), 1);
        assert_eq!(
            trace.hops[0].decode.transport_summary.transports[0].net_rtt_median_ms,
            3
        );
    }

    #[test]
    fn decode_coverage_without_expected_total_is_explicitly_unavailable() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 76,
                "processing_ms": 75,
            }),
        );
        trace.complete = true;
        trace.refresh_telemetry(76);

        assert_eq!(trace.telemetry.reported_decode_spans, 1);
        assert_eq!(trace.telemetry.expected_decode_spans, 0);
        assert!(!trace.telemetry.decode_span_expectation_available);
        assert_eq!(trace.hops[0].decode.node_decode_ms, 0.0);
    }

    #[test]
    fn regular_decode_coverage_uses_output_tokens_per_pipeline_hop() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15), ("no2", 16, 31)]);
        trace.output_tokens = 3;
        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({"span_count": 3, "round_trip_ms": 76, "processing_ms": 75}),
        );
        trace.record_hop_from_json(
            1,
            Phase::Decode,
            &json!({"span_count": 2, "round_trip_ms": 76, "processing_ms": 75}),
        );
        trace.complete = true;
        trace.refresh_telemetry(76);

        assert_eq!(trace.telemetry.reported_decode_spans, 5);
        assert_eq!(trace.telemetry.expected_decode_spans, 6);
        assert!(trace.telemetry.decode_span_expectation_available);
        assert_eq!(trace.telemetry.completeness_percent, 83.33333333333334);
        assert_eq!(trace.telemetry.status, "response_complete");
    }

    #[test]
    fn gateway_overlap_and_residual_use_only_explicit_local_intervals() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.profile = crate::telemetry::Profile::Trace;
        trace.set_accounted_spans(vec![
            TraceSpan::for_test("validation", "gateway", 0, 10, None),
            TraceSpan::for_test("stream", "gateway", 5, 10, None),
            TraceSpan::for_test("remote", "peer", 0, 99, None),
        ]);
        trace.refresh_telemetry(30);

        assert_eq!(trace.telemetry.overlap_ms, Some(5));
        assert_eq!(trace.telemetry.true_residual_ms, Some(15));
    }

    #[test]
    fn transport_telemetry_is_aggregated_instead_of_serializing_every_call() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        for (transport, fallback, round_trip_ms) in [
            (crate::relay::InferenceTransport::Local, false, 10),
            (crate::relay::InferenceTransport::Local, false, 20),
            (crate::relay::InferenceTransport::Https, true, 30),
            (crate::relay::InferenceTransport::Https, false, 40),
            (crate::relay::InferenceTransport::Libp2pTunnel, false, 50),
        ] {
            trace.record_transport(
                0,
                Phase::Decode,
                TransportTrace {
                    transport,
                    fallback,
                    round_trip_ms,
                    network_ms: round_trip_ms,
                    bytes_sent: 100,
                    bytes_received: 200,
                    request_body_send_us: None,
                },
            );
        }

        for _ in 0..4_999 {
            trace.record_transport(
                0,
                Phase::Decode,
                TransportTrace {
                    transport: crate::relay::InferenceTransport::Local,
                    fallback: false,
                    round_trip_ms: 10,
                    network_ms: 10,
                    bytes_sent: 1818,
                    bytes_received: 192,
                    request_body_send_us: None,
                },
            );
        }

        let value = serde_json::to_value(&trace).unwrap();
        let phase = &value["hops"][0]["decode"];
        assert!(phase.get("transport_calls").is_none());
        assert_eq!(phase["transport_summary"]["fallback_calls"], 1);
        assert_eq!(
            phase["transport_summary"]["transports"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(phase["transport_summary"]["transports"][0]["calls"], 5_001);
        assert_eq!(
            phase["transport_summary"]["transports"][0]["bytes_sent"],
            9_088_382
        );
        assert_eq!(
            phase["transport_summary"]["transports"][0]["bytes_received"],
            960_208
        );
        assert_eq!(
            phase["transport_summary"]["transports"][0]["rtt_min_ms"],
            10
        );
        assert_eq!(
            phase["transport_summary"]["transports"][0]["rtt_median_ms"],
            10
        );
        assert_eq!(
            phase["transport_summary"]["transports"][0]["rtt_p95_ms"],
            10
        );
        assert_eq!(
            phase["transport_summary"]["transports"][0]["rtt_max_ms"],
            20
        );
        assert_eq!(
            phase["transport_summary"]["transports"][1]["fallback_calls"],
            1
        );
        assert_eq!(
            phase["transport_summary"]["transports"][1]["rtt_median_ms"],
            40
        );
        assert_eq!(
            phase["transport_summary"]["transports"][1]["rtt_p95_ms"],
            40
        );
        assert_eq!(
            phase["transport_summary"]["transport_switches"],
            serde_json::json!([
                {"from": "local", "to": "https", "calls": 1},
                {"from": "https", "to": "libp2p-tunnel", "calls": 1},
                {"from": "libp2p-tunnel", "to": "local", "calls": 1},
            ])
        );
        // The normal trace now carries request-level integrity and remote
        // stream aggregates, but it must still avoid one serialized item per
        // token. The detailed span vector is profiling-only.
        assert!(trace.profiling_spans.is_none());
        assert!(serde_json::to_string(&trace).unwrap().len() < 12_000);
    }

    #[test]
    fn percentil_de_decode_mantem_amostras_com_teto() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        for value in 0..(TELEMETRY_SAMPLE_LIMIT * 4) {
            trace.record_hop(
                0,
                Phase::Decode,
                1,
                1,
                1,
                1,
                0,
                0,
                0,
                0,
                TlsHop::default(),
                &json!({"decode_ms": value as f64}),
            );
        }

        assert_eq!(
            trace.hops[0].decode.node_samples_ms.len(),
            TELEMETRY_SAMPLE_LIMIT
        );
        assert!(trace.hops[0].decode.node_p50_ms > 0.0);
        assert!(trace.hops[0].decode.node_p95_ms > trace.hops[0].decode.node_p50_ms);
    }

    // ── As duas formas legítimas de resposta a uma rodada especulativa ──────
    //
    // Ler só a primeira era o defeito de 2026-08-15: a cabeça que recusa
    // rascunhar responde na segunda, e o gateway matava a geração no primeiro
    // token. A prova ponta a ponta está em `spec_lab.rs`.

    fn trace_vazio() -> RequestTrace {
        RequestTrace::new("qwen", &[("no1", 0, 15)])
    }

    #[test]
    fn uma_resposta_de_um_token_e_uma_rodada_de_um_token() {
        let passo = speculative_step_from_meta(
            &json!({"done": false, "token_id": 42, "text": "x"}),
            trace_vazio(),
        )
        .expect("a resposta de um token é legítima, não erro");

        assert_eq!(passo.accepted, vec![(42u32, "x".to_string())]);
        assert_eq!(passo.accept_count, 1);
        assert_eq!(
            passo.batch_width, 1,
            "sem rascunho nada foi decodificado além do token semeado"
        );
        assert!(!passo.done);
        assert!(!passo.speculated, "esta rodada não especulou");
        assert!(!passo.speculation_active);
    }

    /// Sem isto a geração ignoraria o EOS e só pararia no teto de tokens.
    #[test]
    fn o_fim_da_resposta_de_um_token_e_respeitado() {
        let passo = speculative_step_from_meta(
            &json!({"done": true, "token_id": 7, "text": ""}),
            trace_vazio(),
        )
        .expect("legítima");
        assert!(passo.done);
    }

    #[test]
    fn uma_rodada_verificada_continua_sendo_lida_como_especulativa() {
        let passo = speculative_step_from_meta(
            &json!({
                "done": false,
                "accepted": [{"token_id": 1, "text": "a"}, {"token_id": 2, "text": "b"}],
                "accept_count": 2,
                "batch_width": 4,
            }),
            trace_vazio(),
        )
        .expect("legítima");

        assert_eq!(passo.accepted.len(), 2);
        assert_eq!(passo.accept_count, 2);
        assert_eq!(passo.batch_width, 4);
        assert!(passo.speculated);
        assert!(passo.speculation_active);
    }

    #[test]
    fn callback_remoto_reporta_estado_da_especulacao_explicitamente() {
        let passo = speculative_step_from_meta(
            &json!({
                "done": false,
                "accepted": [{"token_id": 4, "text": "A"}],
                "speculated": false,
                "speculation_active": true,
                "next_draft": [5, 6],
            }),
            trace_vazio(),
        )
        .expect("callback remoto válido");

        assert!(!passo.speculated);
        assert!(passo.speculation_active);
        assert_eq!(passo.next_draft, Some(vec![5, 6]));
        assert!(passo.speculation_disabled_reason.is_none());
    }

    #[test]
    fn callback_remoto_reporta_a_razao_de_desativacao() {
        let passo = speculative_step_from_meta(
            &json!({
                "done": false,
                "token_id": 4,
                "text": "A",
                "speculated": false,
                "speculation_active": false,
                "speculation_disabled_reason": "model_without_mtp",
            }),
            trace_vazio(),
        )
        .expect("callback remoto válido");

        assert!(!passo.speculated);
        assert!(!passo.speculation_active);
        assert_eq!(
            passo.speculation_disabled_reason.as_deref(),
            Some("model_without_mtp")
        );
    }

    /// Uma resposta que não é nem uma coisa nem outra continua sendo erro:
    /// seguir em silêncio com token nenhum seria pior que a falha.
    #[test]
    fn uma_resposta_sem_token_nenhum_continua_sendo_erro() {
        assert!(speculative_step_from_meta(&json!({"done": false}), trace_vazio()).is_err());
    }

    #[test]
    fn a_trace_keeps_its_issue_budget() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        for attempt in 1..=33 {
            trace.record_issue(TraceIssue::new(
                Some(0),
                "no1",
                Phase::Prefill,
                TraceIssueKind::Transport,
                attempt,
                "offline",
            ));
        }
        assert_eq!(trace.issues.len(), 32);
        assert_eq!(trace.dropped_issues, 1);
    }

    #[tokio::test]
    async fn limite_de_pedacos_em_voo_e_respeitado() {
        let window = PrefillOverlapWindow::new(2);
        assert_eq!(PrefillOverlapWindow::new(0).max_inflight_chunks(), 1);
        assert_eq!(PrefillOverlapWindow::new(99).max_inflight_chunks(), 8);
        let first = window.acquire().await.unwrap();
        let second = window.acquire().await.unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), window.acquire())
                .await
                .is_err()
        );

        drop(first);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), window.acquire())
                .await
                .is_ok()
        );
        drop(second);
    }

    #[tokio::test]
    async fn orcamento_de_bytes_segura_um_pedaco_novo_ate_o_anterior_soltar() {
        let window = PrefillOverlapWindow::new(8);
        let budget = PrefillOverlapWindow::INFLIGHT_BYTE_BUDGET;

        // Metade do orçamento cada: os dois passam, o terceiro espera.
        let a = window.reserve_bytes(budget / 2).await;
        let b = window.reserve_bytes(budget / 2).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                window.reserve_bytes(budget / 2),
            )
            .await
            .is_err(),
            "sem folga no orçamento, o próximo pedaço espera"
        );
        drop(a);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                window.reserve_bytes(budget / 2),
            )
            .await
            .is_ok(),
            "liberado um pedaço, o orçamento acomoda o próximo"
        );
        drop(b);

        // Um pedaço sozinho maior que o orçamento é limitado a ele e não trava.
        let huge = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            window.reserve_bytes(budget.saturating_mul(4)),
        )
        .await;
        assert!(
            huge.is_ok(),
            "um pedaço gigante passa segurando a janela inteira"
        );
    }

    #[test]
    fn trace_distingue_prefixo_candidato_de_tokens_realmente_reaproveitados() {
        let mut trace = RequestTrace::new("qwen", &[("head", 0, 15)]);
        trace.record_prefix_decision(
            &json!({
                "reuse_prefix": false,
                "cached_tokens": 0,
                "prefix_match_tokens": 2,
            }),
            false,
        );
        trace.finish_prompt(5, 0);

        assert_eq!(trace.prefix_match_tokens, 2);
        assert_eq!(trace.cached_tokens, 0);
        assert_eq!(trace.prompt_tokens_transmitted, 5);
    }
}
