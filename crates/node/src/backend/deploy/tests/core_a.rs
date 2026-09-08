    use crate::deployment::{DeploymentIdentity, DeploymentModel};

    #[test]
    fn model_identity_requires_proof_and_allows_same_peer_path_fallback() {
        let desired = super::RequestedModelIdentity {
            proof: Some(DeploymentIdentity::Sha256 {
                sha256: "a".repeat(64),
            }),
            paths_by_peer: [("peer-a".into(), "/models/qwen.gguf".into())]
                .into_iter()
                .collect(),
        };
        let same = DeploymentModel {
            label: "qwen".into(),
            total_layers: 32,
            pom_id: None,
            identity: desired.proof.clone(),
        };
        assert!(super::deployment_identity_matches(
            &desired,
            &same,
            "peer-a",
            Some("/other/path.gguf")
        ));

        let unknown = DeploymentModel {
            identity: None,
            ..same.clone()
        };
        assert!(super::deployment_identity_matches(
            &desired,
            &unknown,
            "peer-a",
            Some("/models/qwen.gguf")
        ));
        assert!(!super::deployment_identity_matches(
            &desired,
            &unknown,
            "peer-b",
            Some("/models/qwen.gguf")
        ));
        assert!(!super::deployment_identity_matches(
            &desired,
            &DeploymentModel {
                identity: Some(DeploymentIdentity::Sha256 {
                    sha256: "b".repeat(64),
                }),
                ..same
            },
            "peer-a",
            None
        ));
    }

    #[test]
    fn node_idle_requires_disabled_status_and_no_configuration() {
        assert!(super::node_is_idle(&serde_json::json!({
            "config": null,
            "status": {"state": "disabled"},
            "active_deployment": "",
            "active_model": ""
        })));
        assert!(!super::node_is_idle(&serde_json::json!({
            "config": null,
            "status": {"state": "starting"},
            "active_deployment": "",
            "active_model": ""
        })));
        assert!(!super::node_is_idle(&serde_json::json!({
            "config": {"model_path": "/models/qwen.gguf"},
            "status": {"state": "disabled"},
            "active_deployment": "",
            "active_model": ""
        })));
    }

    #[test]
    fn deployment_request_defaults_to_simple_and_normalizes_name() {
        let req: super::DeployRequest = serde_json::from_value(serde_json::json!({
            "group": "qwen35"
        }))
        .expect("request válida");

        assert_eq!(req.strategy, super::DeployStrategy::Simple);
        assert_eq!(
            super::normalize_deployment_name(req.deployment_name.as_deref(), &req.group)
                .expect("nome padrão"),
            "qwen35"
        );
    }

    #[test]
    fn deployment_name_rejects_blank_and_overlong_values() {
        assert_eq!(
            super::normalize_deployment_name(Some("  "), "qwen35").expect("fallback"),
            "qwen35"
        );
        assert!(super::normalize_deployment_name(Some(&"x".repeat(129)), "qwen35").is_err());
    }

    /// O tipo tensor-parallel nomeia o grupo, não uma fatia nem um modelo: a
    /// tela oferece o formato, e a recusa tem que falar a língua do formato.
    #[test]
    fn tensor_parallel_kind_names_the_group() {
        use super::DeployKind;
        assert_eq!(
            DeployKind::TensorParallel.unit("qwen", 0, 31),
            "o grupo tensor-parallel qwen"
        );
        assert_eq!(
            DeployKind::TensorParallel.indefinite(),
            "um grupo tensor-parallel"
        );
        assert!(!DeployKind::TensorParallel.whole(), "TP não é whole");
    }

    /// Um no somado no meio do pipeline NAO e o ultimo da divisao.
    ///
    /// `peer_index`/`peer_count` respondem a uma pergunta so - "sou o ultimo?" -
    /// e dela saem duas decisoes do llama.cpp: exportar o estado residual e
    /// pular os embeddings. O caminho incremental manda um alvo por vez, e
    /// deixar o `launch` deduzir a posicao da lista faria todo no somado se
    /// declarar o ultimo. O sintoma e o peer do meio responder
    /// "get_embeddings_ith: invalid embeddings id 0, reason: no embeddings",
    /// porque o motor subiu sem captura e a inferencia so descobre no primeiro
    /// token.
    #[test]
    fn a_node_added_in_the_middle_is_not_the_tail_of_the_split() {
        let (index, count) = super::split_position(&[(0, 11), (32, 39)], (12, 31));
        assert_eq!(count, 3, "tres faixas depois de somar esta");
        assert_eq!(index, 1, "12-31 fica no meio, entre 0-11 e 32-39");
        assert!(index + 1 < count, "nao pode se dizer o ultimo da divisao");
    }

    #[test]
    fn a_node_added_at_the_tail_is_the_tail_of_the_split() {
        let (index, count) = super::split_position(&[(0, 11), (12, 31)], (32, 39));
        assert_eq!((index, count), (2, 3));
    }

    /// Substituir quem ja serve aquela faixa nao inventa um peer a mais.
    #[test]
    fn replacing_the_same_range_keeps_the_count() {
        let (index, count) = super::split_position(&[(0, 11), (12, 31)], (12, 31));
        assert_eq!((index, count), (1, 2));
    }

    /// Faixa sobreposta no cluster deixa de ser erro: e assim que se soma uma
    /// rota paralela. A invariante "uma camada, um no" continua valendo, mas
    /// dentro de um plano, e quem a garante e `plan_pipeline`, que varre camada
    /// a camada escolhendo um peer por posicao.
    #[test]
    fn overlapping_ranges_are_reported_but_not_refused() {
        let sobrepostas = super::overlapping_layers(&[(0, 19), (20, 39)], (0, 19));
        assert_eq!(sobrepostas, (0..=19).collect::<Vec<u32>>());
    }

    #[test]
    fn a_range_that_fills_a_gap_overlaps_nothing() {
        assert!(super::overlapping_layers(&[(0, 19)], (20, 39)).is_empty());
    }

    #[test]
    fn a_partial_overlap_reports_only_the_shared_layers() {
        assert_eq!(
            super::overlapping_layers(&[(0, 19)], (15, 25)),
            vec![15, 16, 17, 18, 19]
        );
    }

    /// Tirar uma replica nao descobre camada nenhuma: a rede so perde uma rota
    /// paralela. Avisar de buraco aqui seria mentira.
    #[test]
    fn removing_a_replica_uncovers_nothing() {
        assert!(super::uncovered_after_removal(&[(0, 19), (20, 39)], (0, 19)).is_empty());
    }

    /// Tirar a ultima daquela faixa descobre as camadas dela, e o pipeline para
    /// de planejar ate alguem assumir.
    #[test]
    fn removing_the_last_holder_uncovers_its_layers() {
        assert_eq!(
            super::uncovered_after_removal(&[(20, 39)], (0, 19)),
            (0..=19).collect::<Vec<u32>>(),
        );
    }

    #[test]
    fn a_partially_replicated_range_uncovers_only_the_rest() {
        assert_eq!(
            super::uncovered_after_removal(&[(0, 9), (20, 39)], (0, 19)),
            (10..=19).collect::<Vec<u32>>()
        );
    }

    #[test]
    fn f8_request_is_blocked_when_peer_capability_is_unsupported() {
        let capability = crate::capabilities::supported("teste", "capacidade presente");
        let capabilities = crate::capabilities::DeploymentCapabilities {
            fit_memory: capability.clone(),
            no_thinking: capability.clone(),
            no_embeddings: capability.clone(),
            speculative_decoding: capability.clone(),
            mtp: capability,
            hidden_state_f8: crate::capabilities::unsupported(
                "capacidades runtime dos peers selecionados",
                "pelo menos um peer selecionado não anuncia estado oculto f8",
            ),
        };
        let no_embeddings = crate::capabilities::evaluate_no_embeddings(&[]);
        let mut params = RuntimeParams::default();
        params.hidden_state_dtype = crate::wire::WireDtype::F8;

        let error = super::requested_capability_block(&params, &capabilities, &no_embeddings)
            .expect("precisão f8 sem capacidade não pode seguir silenciosamente");
        assert!(error.contains("f8"));
        assert!(error.contains("não anuncia"));
    }

    use super::*;

    const GIB: u64 = 1_073_741_824;

    struct Spec {
        id: &'static str,
        mem_free_mb: u64,
        disk_free_mb: u64,
        held: Vec<(u32, u32)>,
        /// What each of this machine's slices declares as the model's size.
        /// `None` is a slice cut before `pom.original_block_count` existed.
        declares: Option<u32>,
    }

    fn m(id: &'static str, mem_free_mb: u64, held: &[(u32, u32)]) -> Spec {
        Spec {
            id,
            mem_free_mb,
            disk_free_mb: 500_000,
            held: held.to_vec(),
            declares: Some(32),
        }
    }

    /// A machine whose slices carry no layer total, like the captain's
    /// `qwen_0_15.gguf` / `qwen_16_31.gguf`.
    fn nameless_machine(id: &'static str, mem_free_mb: u64, held: &[(u32, u32)]) -> Spec {
        Spec {
            declares: None,
            ..m(id, mem_free_mb, held)
        }
    }

    fn build(specs: Vec<Spec>) -> Vec<Machine> {
        specs
            .into_iter()
            .map(|s| Machine {
                node_id: s.id.to_string(),
                hostname: s.id.to_string(),
                gateway_url: None,
                inference_url: None,
                inference_candidates: Vec::new(),
                addr_candidates: Vec::new(),
                libp2p_peer_id: None,
                supports_tunnel: false,
                inference_capabilities: Some(vec!["speculate/v1-lookup".to_string()]),
                mem_free_mb: s.mem_free_mb,
                gpu_free_mb: s.mem_free_mb,
                ram_free_mb: s.mem_free_mb,
                disk_free_mb: s.disk_free_mb,
                rtt_ms: None,
                load: 0,
                load_known: true,
                held: s
                    .held
                    .iter()
                    .map(|(a, b)| HeldShard {
                        start_layer: *a,
                        end_layer: *b,
                        total_layers: s.declares,
                        name: format!("m_{a}_{b}.gguf"),
                        path: format!("/models/m_{a}_{b}.gguf"),
                        sha256: None,
                        pom_id: None,
                        virtual_slice: false,
                        tp_rank: None,
                        tp_size: None,
                        is_moe: false,
                        model_capabilities: Some(crate::gguf::ModelCapabilityEvidence {
                            thinking: Some(true),
                            mtp: Some(false),
                        }),
                        size_bytes: GIB,
                    })
                    .collect(),
            })
            .collect()
    }

    fn pick(start: u32, end: u32, target: &str) -> SelectedSlice {
        SelectedSlice {
            start_layer: start,
            end_layer: end,
            target_node_id: target.to_string(),
            n_gpu_layers: None,
            offload_kqv: None,
        }
    }

    #[test]
    fn model_mtp_comes_from_the_final_selected_file_and_runtime_is_only_a_warning() {
        let mut machines = build(vec![
            m("first", 900_000, &[(0, 15)]),
            m("last", 900_000, &[(16, 31)]),
        ]);
        machines[0].held[0].model_capabilities.as_mut().unwrap().mtp = Some(false);
        machines[1].held[0].model_capabilities.as_mut().unwrap().mtp = Some(true);

        let p = plan(
            "model",
            &machines,
            Some(&[pick(0, 15, "first"), pick(16, 31, "last")]),
        );

        assert_eq!(
            p.capabilities.mtp.state,
            crate::capabilities::CapabilityState::Supported
        );
        assert!(p
            .capabilities
            .mtp
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("runtime")));
    }

    #[test]
    fn runtime_only_speculation_is_visible_with_a_runtime_limitation_warning() {
        let mut machines = build(vec![m("only", 900_000, &[(0, 31)])]);
        machines[0].inference_capabilities = Some(Vec::new());
        let p = plan("model", &machines, Some(&[pick(0, 31, "only")]));

        assert_ne!(
            p.capabilities.speculative_decoding.state,
            crate::capabilities::CapabilityState::Unsupported
        );
        assert!(p
            .capabilities
            .speculative_decoding
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("runtime")));
    }

    #[test]
    fn model_mtp_absence_is_the_only_case_that_hides_mtp() {
        let mut machines = build(vec![m("only", 900_000, &[(0, 31)])]);
        machines[0].held[0].model_capabilities.as_mut().unwrap().mtp = Some(false);
        let p = plan("model", &machines, Some(&[pick(0, 31, "only")]));

        assert_eq!(
            p.capabilities.mtp.state,
            crate::capabilities::CapabilityState::Unsupported
        );
        assert!(p.capabilities.mtp.warning.is_none());
    }

    #[test]
    fn model_mtp_unknown_stays_visible_and_actionable() {
        let mut machines = build(vec![m("only", 900_000, &[(0, 31)])]);
        machines[0].held[0].model_capabilities = None;
        let p = plan("model", &machines, Some(&[pick(0, 31, "only")]));

        assert_eq!(
            p.capabilities.mtp.state,
            crate::capabilities::CapabilityState::Unknown
        );
        assert!(p.capabilities.mtp.warning.is_some());
    }

    #[test]
    fn whole_model_cut_creates_virtual_ranges_for_local_materialization() {
        let mut machines = build(vec![
            m("source", 900_000, &[(0, 31)]),
            m("target", 900_000, &[]),
        ]);
        let options = WholeLayerOptions {
            source_node_id: "source".to_string(),
            total_layers: 32,
            cuts: vec![LayerCut {
                end_layer: 3,
                layers: 4,
                size_bytes: GIB,
                remainder_bytes: GIB,
            }],
            granularity: Some(4),
            rule: "fronteiras reais".to_string(),
            reason: None,
        };
        super::add_virtual_whole_slices(
            &mut machines,
            "model",
            &WholeSliceSelection {
                source_node_id: "source".to_string(),
                target_node_id: "target".to_string(),
                end_layer: 3,
            },
            &options,
        )
        .expect("corte válido");

        assert!(machines[0]
            .holds(0, 3)
            .is_some_and(|shard| shard.virtual_slice));
        assert!(machines[0]
            .holds(4, 31)
            .is_some_and(|shard| shard.virtual_slice));
        let selection = [pick(0, 3, "target"), pick(4, 31, "source")];
        let plan = super::plan(
            "model",
            &machines,
            Some(&selection),
            &RuntimeParams::default(),
            false,
            DeployKind::Whole,
            None,
        );
        assert!(plan.blocked.is_none(), "{:?}", plan.blocked);
        assert_eq!(plan.steps.len(), 2);
        assert!(plan.steps.iter().all(|step| !step.ready));
        assert!(plan.steps[0].source_needs_split);
        assert!(plan.steps[1].source_needs_split);
    }

    /// The knobs are their own axis; every test below is about the placement,
    /// so they all run on the defaults unless they say otherwise.
    fn plan(group: &str, machines: &[Machine], selection: Option<&[SelectedSlice]>) -> Plan {
        super::plan(
            group,
            machines,
            selection,
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
        )
    }

    /// The merge axis, flipped on; everything else still defaults.
    fn merged(group: &str, machines: &[Machine], selection: &[SelectedSlice]) -> Plan {
        super::plan(
            group,
            machines,
            Some(selection),
            &RuntimeParams::default(),
            true,
            DeployKind::Shards,
            None,
        )
    }

    /// The whole-model axis: the same planner, told to read one un-sliced GGUF.
    fn whole(group: &str, machines: &[Machine], selection: Option<&[SelectedSlice]>) -> Plan {
        super::plan(
            group,
            machines,
            selection,
            &RuntimeParams::default(),
            false,
            DeployKind::Whole,
            None,
        )
    }

    // ── The suggestion ──────────────────────────────────────────────────────

    /// The happy shape: each machine already has one of the slices, so the
    /// deploy is pure launch and copies nothing.
    #[test]
    fn slices_already_in_place_need_no_transfer() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 200_000, &[(16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "0-15 e 16-31 cobrem 32 camadas");
        assert_eq!(p.transfer_bytes, 0);
        assert!(p.steps.iter().all(|s| s.ready));
        assert!(p.auto);
    }

    #[test]
    fn complete_strategy_prefers_existing_finer_cuts() {
        let ms = build(vec![
            m("whole", 900_000, &[(0, 31)]),
            m("left", 500_000, &[(0, 15)]),
            m("right", 500_000, &[(16, 31)]),
        ]);
        let p = super::plan_with_options(
            "m",
            &ms,
            None,
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
            DeployStrategy::Complete,
            "chat-prod",
            &[],
        );
        assert_eq!(p.strategy, DeployStrategy::Complete);
        assert_eq!(p.deployment_name, "chat-prod");
        assert_eq!(
            p.steps
                .iter()
                .map(|step| (step.start_layer, step.end_layer))
                .collect::<Vec<_>>(),
            vec![(0, 15), (16, 31)]
        );
    }

    /// An unused eligible peer is preferred for the second copy when it has
    /// capacity, so the automatic plan can spread work without refusing when
    /// a machine already holds every slice.
    #[test]
    fn automatic_planning_can_copy_to_an_idle_peer() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 200_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);

        assert_eq!(p.steps.len(), 2);
        let copied: Vec<&Step> = p.steps.iter().filter(|s| !s.ready).collect();
        assert_eq!(copied.len(), 1, "só uma fatia precisa viajar");
        assert_eq!(copied[0].source_node_id.as_deref(), Some("a"));
        assert_eq!(copied[0].target_node_id, "b");
        assert_eq!(p.transfer_bytes, GIB);
    }

    #[test]
    fn auto_plan_with_one_machine_can_cover_all_slices() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 2);
        assert!(p.steps.iter().all(|step| step.target_node_id == "a"));
    }

    /// Layer 0 runs every prompt, so it goes to the machine with the most room
    /// — the one place the assignment is not arbitrary.
    #[test]
    fn layer_zero_goes_to_the_roomiest_machine() {
        let ms = build(vec![
            m("pequena", 10_000, &[(0, 15), (16, 31)]),
            m("grande", 900_000, &[(0, 15), (16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        let first = p.steps.iter().find(|s| s.start_layer == 0).unwrap();
        assert_eq!(first.target_node_id, "grande");
        assert!(first.primary, "quem tem a camada 0 recebe o prompt");
        assert_eq!(first.position, 1);
    }

    /// Fewer machines than ranges is valid when the selected coverage is
    /// complete. The planner spreads to an idle peer, then reuses a peer.
    #[test]
    fn more_slices_than_machines_can_share_a_machine() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 9), (10, 19), (20, 31)]),
            m("b", 100_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 3);
    }

    /// A gap in the set is reported as incomplete rather than deployed into a
    /// pipeline that can never plan.
    #[test]
    fn a_gap_in_the_set_is_reported() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(20, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert!(!p.complete, "faltam as camadas 16-19");
        assert_eq!(p.missing, vec![[16, 19]]);
        assert!(p.warnings.iter().any(|w| w.contains("L16-19")));
    }

    /// Two cuts of the same model live side by side. The suggestion has to pick
    /// one coherent chain instead of stacking 0-4, 0-10, 5-31 and 11-31 on top
    /// of each other.
    #[test]
    fn overlapping_slicings_are_not_stacked_by_the_planner() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 4), (5, 31)]),
            m("b", 100_000, &[(0, 10), (11, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.overlap.is_empty(), "{:?}", p.overlap);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 2, "duas fatias, não quatro");
        // Longest first from layer 0: 0-10 beats 0-4.
        assert_eq!((p.steps[0].start_layer, p.steps[0].end_layer), (0, 10));
        assert_eq!((p.steps[1].start_layer, p.steps[1].end_layer), (11, 31));
        assert_eq!(p.available.len(), 4, "as quatro continuam à disposição");
        assert_eq!(p.available.iter().filter(|c| c.selected).count(), 2);
    }

    #[test]
    fn a_set_nobody_holds_is_refused() {
        let ms = build(vec![m("a", 100_000, &[])]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.unwrap().contains("nenhuma máquina"));
    }

    // ── The order the steps come back in ────────────────────────────────────
    //
    // `position` says "1-based place in the pipeline", the pipeline order *is*
    // the layer order, and the screen compares the plan it got back against the
    // rows it has on screen - which it keeps in layer order. Every one of those
    // three depends on the same promise, so it is asserted here rather than
    // left to fall out of the grouping loop.

    /// Steps come back in layer order when a machine takes two ranges.
    #[test]
    fn steps_stay_in_layer_order_when_a_machine_takes_two_ranges() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (16, 23)]),
            m("b", 100_000, &[(8, 15)]),
            m("c", 100_000, &[(24, 31)]),
        ]);
        let sel = [
            pick(0, 7, "a"),
            pick(8, 15, "b"),
            pick(16, 23, "a"),
            pick(24, 31, "c"),
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        let order: Vec<(u32, u32)> = p
            .steps
            .iter()
            .map(|s| (s.start_layer, s.end_layer))
            .collect();
        assert_eq!(order, vec![(0, 7), (8, 15), (16, 23), (24, 31)]);
        let positions: Vec<u32> = p.steps.iter().map(|s| s.position).collect();
        assert_eq!(positions, vec![1, 2, 3, 4], "posição é o lugar no pipeline");
    }

    /// The auto planner reuses an eligible machine after all peers have been
    /// considered and still answers in layer order.
    #[test]
    fn auto_ranges_can_reuse_a_machine_without_empty_targets() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (8, 15), (16, 23), (24, 31)]),
            m("b", 100_000, &[(0, 7), (8, 15), (16, 23), (24, 31)]),
        ]);
        let p = plan("m", &ms, None);
        let order: Vec<(u32, u32)> = p
            .steps
            .iter()
            .map(|s| (s.start_layer, s.end_layer))
            .collect();
        assert_eq!(order, vec![(0, 7), (8, 15), (16, 23), (24, 31)]);

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert!(p.steps.iter().all(|step| !step.target_node_id.is_empty()));
    }

    /// Same thing from the operator's side: two ranges deliberately left
    /// without a machine must not read as one machine holding both.
    #[test]
    fn two_hand_picked_ranges_without_a_machine_keep_their_own_rows() {
        let ms = build(vec![m(
            "a",
            100_000,
            &[(0, 7), (8, 15), (16, 23), (24, 31)],
        )]);
        let sel = [
            pick(0, 7, "a"),
            pick(8, 15, ""),
            pick(16, 23, ""),
            pick(24, 31, ""),
        ];
        let p = plan("m", &ms, Some(&sel));
        let order: Vec<(u32, u32)> = p
            .steps
            .iter()
            .map(|s| (s.start_layer, s.end_layer))
            .collect();
        assert_eq!(order, vec![(0, 7), (8, 15), (16, 23), (24, 31)]);
        let blocked = p.blocked.expect("faixa sem máquina é recusa");
        assert!(blocked.contains("sem máquina"), "{blocked}");
    }

    // ── The operator's own choice ───────────────────────────────────────────

    /// Reassigning by hand is honoured, not silently re-planned: the roomy
    /// machine would have taken layer 0, and the operator says otherwise.
    #[test]
    fn an_explicit_assignment_is_honoured() {
        let ms = build(vec![
            m("pequena", 10_000, &[(0, 15), (16, 31)]),
            m("grande", 900_000, &[(0, 15), (16, 31)]),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "pequena"), pick(16, 31, "grande")]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(!p.auto);
        assert_eq!(p.steps[0].target_node_id, "pequena");
        assert_eq!(p.steps[1].target_node_id, "grande");
    }

    #[test]
    fn complete_mode_can_transfer_a_range_to_a_peer_with_capacity() {
        let ms = build(vec![
            m("holder", 100_000, &[(0, 15), (16, 31)]),
            m("capacity", 900_000, &[]),
        ]);
        let p = super::plan_with_options(
            "m",
            &ms,
            None,
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
            DeployStrategy::Complete,
            "m",
            &[],
        );

        assert!(
            p.blocked.is_none(),
            "capacidade suficiente não deve bloquear: {:?}",
            p.blocked
        );
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.steps[0].target_node_id, "holder");
        assert_eq!(p.steps[1].target_node_id, "capacity");
        assert_eq!(p.transfer_bytes, GIB);
    }

    /// Mixing two slicings is exactly the thing that has to be refused: layers
    /// 5-10 would run twice and the answer would be quietly wrong.
    #[test]
    fn overlapping_selection_is_refused() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 10), (5, 31)]),
            m("b", 100_000, &[(0, 10), (5, 31)]),
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 10, "a"), pick(5, 31, "b")]));
        assert_eq!(p.overlap, vec![[5, 10]]);
        let blocked = p.blocked.expect("sobreposição tem que barrar");
        assert!(blocked.contains("L5-10"), "{blocked}");
        assert!(!p.complete);
    }

    /// A mesma faixa em máquinas diferentes é uma réplica deliberada da rota,
    /// não uma sobreposição de cortes incompatíveis. O pipeline escolhe uma
    /// das cópias quando chega à camada.
    #[test]
    fn an_exact_range_on_two_machines_is_a_parallel_route() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(0, 15)]),
            m("c", 100_000, &[(16, 31)]),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "a"), pick(0, 15, "b"), pick(16, 31, "c")]),
        );

        assert!(
            p.blocked.is_none(),
            "réplica exata não deve bloquear: {:?}",
            p.blocked
        );
        assert!(p.complete);
        assert_eq!(p.overlap, Vec::<[u32; 2]>::new());
        assert_eq!(p.steps.len(), 3);
        assert_eq!(
            p.steps
                .iter()
                .take(2)
                .map(|step| step.target_node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
        );
        let targets =
            launch_targets(&p.steps, &ms, DeployKind::Shards, None, &[]).expect("targets");
        assert_eq!(
            targets
                .iter()
                .map(|target| (
                    target.node_id.as_str(),
                    target.peer_index,
                    target.peer_count
                ))
                .collect::<Vec<_>>(),
            vec![
                ("a", Some(0), Some(2)),
                ("b", Some(0), Some(2)),
                ("c", Some(1), Some(2))
            ],
        );
    }

    /// A hole in the selected coverage refuses the plan before any launch.
    #[test]
    fn a_selection_with_a_hole_is_refused() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (20, 31)]),
            m("b", 100_000, &[(0, 15), (20, 31)]),
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(20, 31, "b")]));
        let blocked = p.blocked.expect("um gap não pode ser implantado");
        assert!(blocked.contains("L16-19"), "{blocked}");
        assert!(!p.complete);
        assert_eq!(p.missing, vec![[16, 19]]);
    }

    /// Choosing part of the model leaves a global gap and is refused before
    /// launch, while still reporting the exact missing range.
    #[test]
    fn a_partial_selection_reports_what_is_left_out() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a")]));
        let blocked = p.blocked.expect("a cobertura incompleta é inválida");
        assert!(blocked.contains("L16-31"), "{blocked}");
        assert_eq!(p.missing, vec![[16, 31]]);
        assert_eq!(p.steps.len(), 1);
    }

    #[test]
    fn non_contiguous_ranges_on_one_machine_are_valid() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 3), (32, 35)]),
            m(
                "b",
                100_000,
                &[
                    (4, 7),
                    (8, 11),
                    (12, 15),
                    (16, 19),
                    (20, 23),
                    (24, 27),
                    (28, 31),
                ],
            ),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[
                pick(0, 3, "a"),
                pick(4, 7, "b"),
                pick(8, 11, "b"),
                pick(12, 15, "b"),
                pick(16, 19, "b"),
                pick(20, 23, "b"),
                pick(24, 27, "b"),
                pick(28, 31, "b"),
                pick(32, 35, "a"),
            ]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(
            p.steps
                .iter()
                .map(|step| {
                    (
                        step.start_layer,
                        step.end_layer,
                        step.target_node_id.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (0, 3, "a"),
                (4, 7, "b"),
                (8, 11, "b"),
                (12, 15, "b"),
                (16, 19, "b"),
                (20, 23, "b"),
                (24, 27, "b"),
                (28, 31, "b"),
                (32, 35, "a"),
            ]
        );
    }

    #[test]
    fn all_ranges_on_one_machine_are_valid() {
        let ms = build(vec![m(
            "a",
            100_000,
            &[
                (0, 3),
                (4, 7),
                (8, 11),
                (12, 15),
                (16, 19),
                (20, 23),
                (24, 27),
                (28, 31),
                (32, 35),
            ],
        )]);
        let p = plan(
            "m",
            &ms,
            Some(&[
                pick(0, 3, "a"),
                pick(4, 7, "a"),
                pick(8, 11, "a"),
                pick(12, 15, "a"),
                pick(16, 19, "a"),
                pick(20, 23, "a"),
                pick(24, 27, "a"),
                pick(28, 31, "a"),
                pick(32, 35, "a"),
            ]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 9);
    }

    // ── Merged units ─────────────────────────────────────────────────────────

    /// The whole point of the toggle: adjacent slices on the same machine
    /// become one file, so the machine serves the union. Everything is already
    /// there, so nothing travels and the plan is complete with one step fewer.
    #[test]
    fn merge_joins_adjacent_slices_on_one_machine() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (8, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[pick(0, 7, "a"), pick(8, 15, "a"), pick(16, 31, "b")],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "0-7 + 8-15 + 16-31 cobrem 32 camadas");
        assert_eq!(p.transfer_bytes, 0, "a já tem as duas fatias");
        assert_eq!(p.steps.len(), 2);

        let joined = p.steps.iter().find(|s| s.merged).unwrap();
        assert_eq!((joined.start_layer, joined.end_layer), (0, 15));
        assert!(joined.ready);
        assert_eq!(joined.parts.len(), 2, "duas fatias entram no arquivo");
        assert!(joined.parts.iter().all(|part| part.ready));
        // Rótulo de tela, não nome de arquivo: o arquivo junto só ganha nome
        // depois de existir, e o nome dele é o hash.
        assert_eq!(joined.name, "m L0-15", "rótulo do passo de junção");

        let solo = p.steps.iter().find(|s| !s.merged).unwrap();
        assert_eq!((solo.start_layer, solo.end_layer), (16, 31));
        // The merged machine's table row names the union.
        let a = p.machines.iter().find(|m| m.node_id == "a").unwrap();
        assert_eq!(a.taking, Some([0, 15]));
    }

    /// Without the toggle, adjacent ranges remain independent pipeline steps.
    /// The machine is still allowed to serve both; only the file join is off.
    #[test]
    fn without_merge_adjacent_ranges_remain_independent_steps() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (8, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 7, "a"), pick(8, 15, "a"), pick(16, 31, "b")]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 3);
        assert!(p.steps.iter().all(|step| !step.merged));
    }

    /// `splitter::merge` only joins neighbours. Separated ranges remain
    /// independent steps even when the merge toggle is on.
    #[test]
    fn merge_only_joins_adjacent_ranges() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (9, 15)]),
            m("b", 100_000, &[(8, 8)]),
            m("c", 100_000, &[(16, 31)]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[
                pick(0, 7, "a"),
                pick(8, 8, "b"),
                pick(9, 15, "a"),
                pick(16, 31, "c"),
            ],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 4);
        assert!(p.steps.iter().all(|step| !step.merged));
    }

    /// A merged unit may need copies: each part that is missing has its own
    /// source, and the transfer byte count is the union of the missing ones.
    #[test]
    fn merged_parts_keep_their_sources() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7)]),
            m("b", 100_000, &[(8, 15)]),
            m("d", 100_000, &[(0, 7), (8, 15)]),
            m("e", 100_000, &[(16, 31)]),
            m("c", 100_000, &[]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[pick(0, 7, "c"), pick(8, 15, "c"), pick(16, 31, "e")],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let joined = p.steps.iter().find(|s| s.merged).unwrap();
        assert!(!joined.ready);
        assert_eq!(p.transfer_bytes, 2 * GIB, "as duas fatias viajam");
        let first = &joined.parts[0];
        assert_eq!(first.source_node_id.as_deref(), Some("a"));
        let second = &joined.parts[1];
        assert_eq!(second.source_node_id.as_deref(), Some("b"));
        // A machine holding every part is offered as ready for the merged unit.
        let option = joined
            .options
            .iter()
            .find(|o| o.node_id == "d" && o.has)
            .expect("d tem as duas fatias, logo está pronta para juntar");
        assert!(option.ok);
    }

    /// A merged unit whose target holds no part and no machine holds a part is
    /// a dead end, reported with the offending range named.
    #[test]
    fn a_merged_unit_with_an_unobtainable_part_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 7)])]);
        let p = merged("m", &ms, &[pick(0, 7, "a"), pick(8, 15, "a")]);
        let blocked = p.blocked.expect("L8-15 não existe em máquina nenhuma");
        assert!(blocked.contains("L8-15"), "{blocked}");
    }

    #[test]
    fn a_slice_with_no_machine_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "")]));
        assert!(p.blocked.unwrap().contains("sem máquina"));
    }

    #[test]
    fn a_machine_that_left_the_cluster_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "a"), pick(16, 31, "fantasma")]),
        );
        assert!(p.blocked.unwrap().contains("não está no cluster"));
    }

    /// A range nobody in the cluster has cannot be deployed however the
    /// machines are shuffled.
    #[test]
    fn a_selected_range_nobody_holds_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "a")]));
        assert!(p.blocked.unwrap().contains("não existe em máquina nenhuma"));
    }

    /// Disk is a hard fact — `transfer.rs` would refuse the copy anyway, so it
    /// is better refused before anything moves.
    #[test]
    fn a_target_without_disk_room_for_the_copy_is_refused() {
        let mut ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 100_000, &[]),
        ]);
        ms[1].disk_free_mb = 100; // 100 MB for a 1 GiB slice
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "b")]));
        let blocked = p.blocked.expect("não cabe no disco");
        assert!(blocked.contains("disco"), "{blocked}");
    }

    /// Memory is an estimate, so it warns and marks the row instead of
    /// refusing: the reading is honest on Linux and unified on macOS, and a
    /// wrong stop would be worse than a loud line.
    #[test]
    fn a_target_short_on_memory_warns_and_still_deploys() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 200, &[(16, 31)]), // 200 MB livres para 1 GiB
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "b")]));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let tight = p.steps.iter().find(|s| s.target_node_id == "b").unwrap();
        assert!(tight.note.as_deref().unwrap().contains("memória"));
        assert!(p.warnings.iter().any(|w| w.contains("memória")));
    }

    /// Every machine is offered for every range, including the one already busy
    /// elsewhere — that is the machine a swap needs.
    #[test]
    fn every_machine_is_offered_as_a_target() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
            m("c", 100_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        let first = &p.steps[0];
        assert_eq!(first.options.len(), 3);
        let a = first.options.iter().find(|o| o.node_id == "a").unwrap();
        assert!(a.has && a.ok);
        let c = first.options.iter().find(|o| o.node_id == "c").unwrap();
        assert!(!c.has && c.ok, "c não tem a fatia mas pode receber");
    }

    #[test]
    fn route_options_only_bridge_consecutive_selected_stages() {
        let mut ms = build(vec![
            m("p0", 100_000, &[(0, 1)]),
            m("p1", 100_000, &[(2, 4)]),
            m("p2", 100_000, &[(5, 6)]),
            m("p3", 100_000, &[(7, 10)]),
        ]);
        for machine in &mut ms {
            machine.inference_url = Some(format!("http://{}:8181", machine.node_id));
        }

        let selection = [
            pick(0, 1, "p0"),
            pick(2, 4, "p1"),
            pick(5, 6, "p2"),
            pick(7, 10, "p3"),
        ];
        let p = plan("qwen", &ms, Some(&selection));
        let pairs: Vec<_> = p
            .route_options
            .iter()
            .map(|route| (route.from_node_id.as_str(), route.to_node_id.as_str()))
            .collect();

        assert_eq!(pairs, vec![("p0", "p1"), ("p1", "p2"), ("p2", "p3")]);
    }

    #[test]
    fn route_options_use_the_directional_rtt_of_each_handoff() {
        let mut ms = build(vec![
            m("p0", 100_000, &[(0, 1)]),
            m("p1", 100_000, &[(2, 4)]),
            m("p2", 100_000, &[(5, 6)]),
            m("p3", 100_000, &[(7, 10)]),
        ]);
        for machine in &mut ms {
            machine.inference_url = Some(format!("http://{}:8181", machine.node_id));
        }
        let selection = [
            pick(0, 1, "p0"),
            pick(2, 4, "p1"),
            pick(5, 6, "p2"),
            pick(7, 10, "p3"),
        ];
        let mut rtts = PairwiseRtt::default();
        rtts.values.insert(("p0".into(), "p1".into()), 11.5);
        rtts.values.insert(("p1".into(), "p2".into()), 22.5);
        rtts.values.insert(("p2".into(), "p3".into()), 33.5);

        let p = plan_with_rtt_options(
            "qwen",
            &ms,
            Some(&selection),
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
            DeployStrategy::Simple,
            "qwen",
            &[],
            &rtts,
            None,
        );
        let values: Vec<_> = p.route_options.iter().map(|route| route.rtt_ms).collect();
        assert_eq!(values, vec![Some(11.5), Some(22.5), Some(33.5)]);
    }
