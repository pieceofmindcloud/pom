    /// The machine list carries what the screen needs to explain a refusal.
    #[test]
    fn the_plan_carries_the_machines_and_what_they_take() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert_eq!(p.machines.len(), 2);
        let a = p.machines.iter().find(|v| v.node_id == "a").unwrap();
        assert_eq!(a.holds, vec![[0, 15]]);
        assert_eq!(a.taking, Some([0, 15]));
    }

    #[test]
    fn sliced_vision_uses_only_the_selected_pipeline_head() {
        let mut machines = build(vec![
            m("unselected-head", 100_000, &[(0, 15)]),
            m("selected-head", 100_000, &[(0, 15)]),
            m("tail", 100_000, &[(16, 31)]),
        ]);
        machines[0].held[0].mmproj_path = Some("/unselected/mmproj.gguf".into());
        machines[1].held[0].mmproj_path = Some("/selected/mmproj.gguf".into());

        let selection = [pick(0, 15, "selected-head"), pick(16, 31, "tail")];
        let planned = plan("vision", &machines, Some(&selection));
        assert_eq!(planned.mmproj_path.as_deref(), Some("/selected/mmproj.gguf"));

        let targets = super::launch_targets(
            &planned.steps,
            &machines,
            DeployKind::Shards,
            None,
            &[],
        )
        .expect("targets should be launchable");
        assert_eq!(targets[0].mmproj_path.as_deref(), Some("/selected/mmproj.gguf"));
        assert_eq!(targets[1].mmproj_path, None);

        let selection = [pick(0, 15, "unselected-head"), pick(16, 31, "tail")];
        let planned = plan("vision", &machines, Some(&selection));
        assert_eq!(planned.mmproj_path.as_deref(), Some("/unselected/mmproj.gguf"));

        machines[0].held[0].mmproj_path = Some("/unselected/mmproj.gguf".into());
        machines[1].held[0].mmproj_path = None;
        let selection = [pick(0, 15, "selected-head"), pick(16, 31, "tail")];
        let planned = plan("vision", &machines, Some(&selection));
        assert_eq!(planned.mmproj_path, None);
    }

    // ── Execution settings ──────────────────────────────────────────────────

    /// The screen fills its form from the plan, so the plan has to carry the
    /// settings back — including the two derived numbers that decide whether a
    /// harness's prompt fits.
    #[test]
    fn the_plan_echoes_the_settings_and_the_prompt_budget() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let params = RuntimeParams {
            n_ctx: 32768,
            n_batch: 3072,
            max_sessions: 2,
            max_new_tokens: 256,
            prefill_chunk_tokens: 4096,
            ..RuntimeParams::default()
        };
        let p = super::plan("m", &ms, None, &params, false, DeployKind::Shards, None);

        assert_eq!(p.params.n_ctx, 32768);
        // Cada sessão recebe 32768; 256 ficam reservados para a resposta.
        assert_eq!(p.prompt_tokens, 32512);
        // O pedaço nunca passa de n_batch, por mais alto que seja o pedido.
        assert_eq!(p.chunk_tokens, 3072);
    }

    #[test]
    fn tensor_parallel_plans_disable_llama_fit() {
        let params = RuntimeParams::default();
        assert!(params.fit_memory);
        assert!(!effective_params_for_kind(DeployKind::TensorParallel, &params).fit_memory);
        assert!(effective_params_for_kind(DeployKind::Shards, &params).fit_memory);
    }

    #[test]
    fn inference_mode_round_trips_for_whole_shards_and_tensor_parallel() {
        let mut requested = RuntimeParams::default();
        requested.inference_mode = InferenceMode::Moe;
        requested.cycle_detection_enabled = true;
        requested.cycle_detection_min_window_tokens = 7;
        requested.cycle_detection_max_window_tokens = 9;
        requested.cycle_detection_repetitions = 8;
        requested.mtp_cpu_fallback_enabled = false;
        requested.unload_if_idle = true;
        requested.idle_minutes = 2;
        for kind in [
            DeployKind::Whole,
            DeployKind::Shards,
            DeployKind::TensorParallel,
        ] {
            let effective = effective_params_for_kind(kind, &requested);
            let mut expected = requested.clone();
            if kind == DeployKind::TensorParallel {
                expected.fit_memory = false;
            }
            assert_eq!(effective, expected, "{kind:?} reescreveu o deploy");
        }
    }

    #[test]
    fn moe_capability_is_reported_for_whole_shards_and_tensor_parallel() {
        let mut params = RuntimeParams::default();
        params.inference_mode = InferenceMode::Moe;

        let mut whole = whole_machine("whole", 900_000, true);
        whole.held[0].is_moe = true;
        let p_whole = super::plan(
            "qwen-moe",
            std::slice::from_ref(&whole),
            None,
            &params,
            false,
            DeployKind::Whole,
            None,
        );
        let p_shards = super::plan(
            "qwen-moe",
            std::slice::from_ref(&whole),
            None,
            &params,
            false,
            DeployKind::Shards,
            None,
        );
        for (kind, p) in [(DeployKind::Whole, p_whole), (DeployKind::Shards, p_shards)] {
            assert!(p.is_moe);
            assert!(
                p.inference_mode_supported,
                "{kind:?}: {:?}",
                p.inference_mode_reason
            );
            assert!(p.inference_mode_reason.is_none());
        }

        let mut primary = tp_machine("primary", 0);
        let mut worker = tp_machine("worker", 1);
        primary.held[0].is_moe = true;
        worker.held[0].is_moe = true;
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "primary".into(),
            worker_node_ids: vec!["worker".into()],
        };
        let p = super::plan(
            "qwen-moe",
            &[primary, worker],
            None,
            &params,
            false,
            DeployKind::TensorParallel,
            Some(tp),
        );
        assert!(p.is_moe);
        assert!(!p.inference_mode_supported);
        assert_eq!(
            p.inference_mode_reason.as_deref(),
            Some("Tensor Parallelism não suporta MoE nesta versão.")
        );
    }

    /// A cluster of unlike machines is the whole point of the per-peer offload:
    /// the number the operator typed for one peer must reach that peer and no
    /// other.
    #[test]
    fn per_machine_gpu_offload_rides_along_with_the_slice() {
        let ms = build(vec![
            m("com-placa", 100_000, &[(0, 15)]),
            m("so-cpu", 100_000, &[(16, 31)]),
        ]);
        let sel = [
            pick(0, 15, "com-placa"),
            SelectedSlice {
                n_gpu_layers: Some(0),
                ..pick(16, 31, "so-cpu")
            },
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.steps[0].n_gpu_layers, None, "herda o valor do cluster");
        assert_eq!(p.steps[1].n_gpu_layers, Some(0));
    }

    /// Onde o cache KV mora é decidido pela VRAM da máquina, e as máquinas do
    /// cluster não se parecem: a escolha feita para uma não pode aparecer na
    /// outra, nem apagar a que não foi escolhida.
    #[test]
    fn per_machine_kv_cache_rides_along_with_the_slice() {
        let ms = build(vec![
            m("com-placa", 100_000, &[(0, 15)]),
            m("so-cpu", 100_000, &[(16, 31)]),
        ]);
        let sel = [
            pick(0, 15, "com-placa"),
            SelectedSlice {
                offload_kqv: Some(false),
                ..pick(16, 31, "so-cpu")
            },
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.steps[0].offload_kqv, None, "herda a escolha do cluster");
        assert_eq!(p.steps[1].offload_kqv, Some(false));
        // Um eixo não mexe no outro: nenhuma das duas pediu offload.
        assert_eq!(p.steps[1].n_gpu_layers, None);
    }

    // ── Reading a machine's free memory ─────────────────────────────────────

    #[test]
    fn nvidia_memory_comes_from_vram_and_everything_else_from_ram() {
        let nvidia = json!({
            "gpu_kind": "nvidia",
            "ram": { "available_mb": 3000.0 },
            "gpus": [{ "vram_total_mb": 16000.0, "vram_used_mb": 7600.0 }],
        });
        assert_eq!(free_memory_mb(Some(&nvidia)), 8400);

        let unified = json!({
            "gpu_kind": "unified",
            "ram": { "available_mb": 12000.0 },
            "gpus": [],
        });
        assert_eq!(free_memory_mb(Some(&unified)), 12000);

        // A peer that did not answer says nothing, and nothing is inferred.
        assert_eq!(free_memory_mb(None), 0);
    }

    // ── How many layers the model has ───────────────────────────────────────
    //
    // Reproduced on the captain's own cluster: both machines answer
    // `"total_layers": null` for the `qwen` set (slices cut before
    // `split_model.py` wrote `pom.original_block_count`). That `null`
    // became a `0`, the plan called itself complete, and every peer's apply
    // died on `runtime::NodeConfig::validate` with "total_layers não pode ser
    // zero" — after `launch` had already stopped inference on all of them.

    /// The exact shape of the captain's `qwen` set: no metadata anywhere, but
    /// the two ranges plainly reach layer 32 between them.
    #[test]
    fn slices_without_metadata_take_the_total_from_their_coverage() {
        let ms = build(vec![
            nameless_machine("a", 100_000, &[(0, 15)]),
            nameless_machine("b", 200_000, &[(16, 31)]),
        ]);
        let p = plan("qwen", &ms, None);

        assert_eq!(
            p.total_layers,
            Some(32),
            "0-15 e 16-31 alcançam a camada 32"
        );
        assert!(p.total_layers_inferred, "veio da cobertura, não do arquivo");
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert!(
            p.warnings.iter().any(|w| w.contains("deduzido")),
            "a tela tem de dizer que o total foi deduzido: {:?}",
            p.warnings
        );
    }

    /// The plan is what `launch` is handed, so a zero here is a peer that stops
    /// serving for nothing. There is no path that produces one.
    #[test]
    fn a_plan_never_carries_a_total_of_zero() {
        for machines in [
            build(vec![nameless_machine("a", 100_000, &[(0, 15), (16, 31)])]),
            build(vec![nameless_machine("a", 100_000, &[(0, 31)])]),
            build(vec![m("a", 100_000, &[(0, 15)])]),
        ] {
            let p = plan("m", &machines, None);
            assert_ne!(p.total_layers, Some(0));
            assert!(p.total_layers.is_some() || p.blocked.is_some());
        }
    }

    /// Nothing declares a total and nothing starts at layer 0: the honest
    /// answer is a refusal that names the files, not a guess and not a zero.
    #[test]
    fn a_set_that_cannot_say_its_size_is_refused_by_name() {
        let ms = build(vec![nameless_machine("a", 100_000, &[(16, 31)])]);
        let p = plan("qwen", &ms, None);

        assert_eq!(p.total_layers, None);
        let reason = p.blocked.expect("sem total e sem camada 0 é recusa");
        assert!(
            reason.contains("m_16_31.gguf"),
            "a recusa tem de dizer qual fatia está sem o dado: {reason}"
        );
        assert!(
            reason.contains("Fatiar"),
            "e o que fazer a respeito: {reason}"
        );
    }

    /// Metadata is a fact and coverage is a deduction, so the fact wins — and
    /// the layers the set does not reach are reported as missing instead of
    /// disappearing with the total.
    #[test]
    fn a_declared_total_beats_the_coverage_and_exposes_the_gap() {
        let mut ms = build(vec![m("a", 100_000, &[(0, 15)])]);
        ms[0].held[0].total_layers = Some(40);
        let p = plan("m", &ms, None);

        assert_eq!(p.total_layers, Some(40));
        assert!(!p.total_layers_inferred);
        assert_eq!(p.missing, vec![[16, 39]]);
        assert!(!p.complete);
    }

    /// Half a nameless set is a half deploy, and before the fix it was the
    /// silent one: with the total at zero the tail check was skipped, the plan
    /// called itself complete and the screen armed the button.
    #[test]
    fn half_of_a_nameless_set_is_reported_as_a_gap() {
        let ms = build(vec![nameless_machine("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("qwen", &ms, Some(&[pick(0, 15, "a")]));

        assert_eq!(p.total_layers, Some(32));
        assert_eq!(p.missing, vec![[16, 31]], "L16-31 ficou de fora");
        assert!(!p.complete, "meia rede não é uma rede completa");
    }

    /// The deduction walks the reach from layer 0, so a model sliced twice with
    /// different cuts still resolves to one size, and a set that starts later
    /// resolves to none.
    #[test]
    fn the_deduction_walks_the_reach_from_layer_zero() {
        let size = |held: &[(u32, u32)]| {
            let ms = build(vec![nameless_machine("a", 100_000, held)]);
            model_size(&candidates(&ms))
        };
        assert_eq!(size(&[(0, 15), (16, 31)]), ModelSize::Inferred(32));
        // Two different cuts of the same model, side by side.
        assert_eq!(
            size(&[(0, 4), (5, 31), (0, 15), (16, 31)]),
            ModelSize::Inferred(32)
        );
        // A hole stops the reach where the hole is.
        assert_eq!(size(&[(0, 15), (20, 31)]), ModelSize::Inferred(16));
        assert_eq!(size(&[(4, 31)]), ModelSize::Unknown);
        assert_eq!(size(&[]), ModelSize::Unknown);
    }

    /// A peer running an older build answers `"total_layers": 0` where this one
    /// answers `null`. Both mean "this file does not say".
    #[test]
    fn a_shard_that_reports_zero_layers_is_a_shard_with_no_total() {
        let shard = parse_shard(
            &json!({
                "shard_range": [0, 15],
                "total_layers": 0,
                "name": "qwen_0_15.gguf",
                "path": "/models/qwen/qwen_0_15.gguf",
                "size_bytes": 2569314208u64,
            }),
            None,
            None,
            None,
            &[],
        )
        .expect("a fatia é válida, só não diz o total");
        assert_eq!(shard.total_layers, None);

        let declared = parse_shard(
            &json!({
                "shard_range": [0, 5],
                "total_layers": 40,
                "name": "phi_0_5.gguf",
                "path": "/models/phi/phi_0_5.gguf",
                "size_bytes": 1u64,
            }),
            None,
            None,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(declared.total_layers, Some(40));

        let inherited = parse_shard(
            &json!({
                "shard_range": [0, 15],
                "total_layers": 32,
                "name": "vision_0_15.gguf",
                "path": "/shards/vision_0_15.gguf",
                "size_bytes": 1u64,
            }),
            None,
            Some("/models/mmproj-vision.gguf"),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(inherited.mmproj_path.as_deref(), Some("/models/mmproj-vision.gguf"));
    }

    // ── Um modelo inteiro ───────────────────────────────────────────────────
    //
    // O caso degenerado: uma máquina, todas as camadas. O que estes testes
    // guardam não é uma segunda implementação - é que **não** existe uma. Um
    // modelo inteiro entra no mesmo `HeldShard`, sai no mesmo `Step` e é
    // lançado pelo mesmo `launch`; o que muda é de onde o inventário é lido e
    // qual substantivo a recusa usa.

    /// Uma máquina com (ou sem) o GGUF inteiro de 32 camadas.
    fn whole_machine(id: &'static str, mem_free_mb: u64, has: bool) -> Machine {
        Machine {
            node_id: id.to_string(),
            hostname: id.to_string(),
            gateway_url: Some(format!("http://{id}:8080")),
            inference_url: Some(format!("http://{id}:8181")),
            inference_candidates: vec![format!("http://{id}:8181")],
            addr_candidates: Vec::new(),
            libp2p_peer_id: None,
            supports_tunnel: false,
            inference_capabilities: Some(vec!["speculate/v1-lookup".to_string()]),
            mem_free_mb,
            gpu_free_mb: mem_free_mb,
            ram_free_mb: mem_free_mb,
            disk_free_mb: 500_000,
            rtt_ms: Some(0.0),
            load: 0,
            load_known: true,
            held: if has {
                vec![HeldShard {
                    kind: HeldKind::Whole,
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
                    start_layer: 0,
                    end_layer: 31,
                    total_layers: Some(32),
                    name: "qwen3.5-9b".into(),
                    path: format!("/{id}/qwen3.5-9b.gguf"),
                    mmproj_path: None,
                    mmproj_candidates: Vec::new(),
                    mmproj_reason: None,
                    size_bytes: 5 * GIB,
                }]
            } else {
                Vec::new()
            },
        }
    }

    fn tp_machine(id: &'static str, rank: u32) -> Machine {
        let mut machine = whole_machine(id, 900_000, false);
        machine.held.push(HeldShard {
            kind: HeldKind::Shard,
            start_layer: 0,
            end_layer: 31,
            total_layers: Some(32),
            name: format!("qwen · {}of2", rank + 1),
            path: format!("/{id}/rank-{rank}.gguf"),
            mmproj_path: None,
            mmproj_candidates: Vec::new(),
            mmproj_reason: None,
            size_bytes: 2 * GIB,
            sha256: Some(format!("{:064x}", rank + 1)),
            pom_id: None,
            virtual_slice: false,
            tp_rank: Some(rank),
            tp_size: Some(2),
            is_moe: false,
            model_capabilities: Some(crate::gguf::ModelCapabilityEvidence {
                thinking: Some(true),
                mtp: Some(false),
            }),
        });
        machine
    }

    /// O plano de um modelo inteiro é uma máquina cobrindo tudo - e é
    /// `complete`, sem buraco e sem sobreposição, porque a única faixa que
    /// existe é o modelo todo.
    #[test]
    fn a_whole_model_is_one_machine_covering_every_layer() {
        let ms = vec![whole_machine("a", 900_000, true)];
        let p = whole("qwen3.5-9b", &ms, None);

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "uma faixa 0-31 cobre um modelo de 32 camadas");
        assert!(p.missing.is_empty() && p.overlap.is_empty());
        assert_eq!(p.total_layers, Some(32));
        assert!(
            !p.total_layers_inferred,
            "o GGUF inteiro declara o total; nada foi deduzido"
        );
        assert_eq!(p.kind, DeployKind::Whole);

        assert_eq!(p.steps.len(), 1);
        let s = &p.steps[0];
        assert_eq!((s.start_layer, s.end_layer), (0, 31));
        assert_eq!(s.position, 1);
        assert!(s.primary, "quem tem a camada 0 recebe o prompt");
        assert!(s.ready, "o modelo já está na máquina");
        assert!(!s.merged && s.parts.is_empty());
        assert_eq!(s.target_node_id, "a");
        assert_eq!(p.transfer_bytes, 0);
    }

    /// O operador escolhe a máquina, e a escolha é validada como qualquer
    /// outra: a faixa é a do modelo inteiro.
    #[test]
    fn the_operator_picks_the_machine_for_a_whole_model() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "b")]));

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(!p.auto, "é a escolha do operador, não a sugestão do nó");
        assert_eq!(p.steps.len(), 1);
        assert_eq!(p.steps[0].target_node_id, "b");
        assert!(p.steps[0].ready);
    }

    /// Duas cópias do modelo inteiro são duas rotas válidas para a mesma
    /// faixa. O erro de modelo escolhido duas vezes só vale quando o mesmo
    /// destino aparece duas vezes na seleção.
    #[test]
    fn a_whole_model_on_two_machines_is_a_parallel_route() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let p = whole(
            "qwen3.5-9b",
            &ms,
            Some(&[pick(0, 31, "a"), pick(0, 31, "b")]),
        );

        assert!(
            p.blocked.is_none(),
            "réplica exata não deve bloquear: {:?}",
            p.blocked
        );
        assert!(p.complete);
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.steps[0].target_node_id, "a");
        assert_eq!(p.steps[1].target_node_id, "b");
    }

    /// Uma máquina que não tem o arquivo recebe uma cópia, exatamente como uma
    /// fatia - e o plano nomeia quem envia.
    #[test]
    fn a_machine_without_the_model_is_planned_as_a_copy() {
        let ms = vec![
            whole_machine("tem", 900_000, true),
            whole_machine("nao-tem", 900_000, false),
        ];
        let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "nao-tem")]));

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let s = &p.steps[0];
        assert!(!s.ready);
        assert_eq!(s.source_node_id.as_deref(), Some("tem"));
        assert_eq!(s.source_path.as_deref(), Some("/tem/qwen3.5-9b.gguf"));
        assert_eq!(p.transfer_bytes, 5 * GIB);
        // O aviso existe porque `transfer::safe_model_path` prende o sentido
        // "puxar" à pasta de fatias de quem envia, e um modelo inteiro mora
        // quase sempre fora dela.
        assert!(
            s.note
                .as_deref()
                .unwrap_or_default()
                .contains("Transferências"),
            "a cópia de um modelo inteiro tem que avisar de onde ela pode sair: {:?}",
            s.note
        );
    }

    /// A recusa fala do que está na tela. "a fatia L0-31" nomeia uma coisa que
    /// o operador que escolheu um modelo inteiro não vê em lugar nenhum.
    #[test]
    fn a_whole_model_refusal_names_the_model_and_not_a_slice() {
        let ms = vec![whole_machine("a", 900_000, true)];
        let sem_maquina = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "")]));
        let motivo = sem_maquina.blocked.expect("sem máquina é recusa");
        assert!(
            motivo.contains("o modelo qwen3.5-9b") && !motivo.contains("fatia"),
            "{motivo}"
        );

        let nada = whole("qwen3.5-9b", &ms, Some(&[]));
        let motivo = nada.blocked.expect("nada escolhido é recusa");
        assert_eq!(motivo, "nenhum modelo selecionado");

        let ninguem = whole("nao-existe", &[whole_machine("a", 900_000, false)], None);
        let motivo = ninguem.blocked.expect("ninguém tem o modelo");
        assert!(motivo.contains("modelo inteiro nao-existe") || motivo.contains("nao-existe"));
        assert!(!motivo.contains("fatias de"), "{motivo}");
    }

    /// O menu de destinos diz "já tem o modelo", não "já tem a fatia".
    #[test]
    fn the_target_menu_speaks_of_a_model() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, false),
        ];
        let p = whole("qwen3.5-9b", &ms, None);
        let opcoes = &p.steps[0].options;
        let a = opcoes.iter().find(|o| o.node_id == "a").unwrap();
        assert!(a.has);
        assert_eq!(a.note.as_deref(), Some("já tem o modelo"));
        let b = opcoes.iter().find(|o| o.node_id == "b").unwrap();
        assert!(!b.has);
        assert!(b.note.as_deref().unwrap().contains("receberia"));
    }

    /// A camada 0 é a que processa o prompt inteiro, e com um modelo inteiro
    /// ela é a máquina escolhida - que a sugestão manda para a mais folgada.
    #[test]
    fn the_suggestion_puts_a_whole_model_on_the_roomiest_machine() {
        let ms = vec![
            whole_machine("pequena", 10_000, true),
            whole_machine("grande", 900_000, true),
        ];
        let p = whole("qwen3.5-9b", &ms, None);
        assert_eq!(p.steps.len(), 1, "um modelo inteiro é um passo, sempre");
        assert_eq!(p.steps[0].target_node_id, "grande");
    }

    /// Um modelo cujo cabeçalho GGUF não deu o número de camadas não vira uma
    /// implantação com `total_layers` zero: ele simplesmente não entra no
    /// inventário. `NodeConfig::validate` recusaria o zero no destino, depois
    /// de `launch` já ter parado a inferência de lá.
    #[test]
    fn a_model_with_no_layer_count_never_becomes_a_candidate() {
        let sem_camadas = json!({
            "name": "quebrado",
            "path": "/m/quebrado.gguf",
            "size_bytes": 1u64,
            "total_layers": serde_json::Value::Null,
            "shard_range": serde_json::Value::Null,
        });
        assert!(parse_whole(&sem_camadas, "quebrado").is_none());

        let zero = json!({
            "name": "quebrado",
            "path": "/m/quebrado.gguf",
            "size_bytes": 1u64,
            "total_layers": 0,
            "shard_range": serde_json::Value::Null,
        });
        assert!(parse_whole(&zero, "quebrado").is_none());
    }

    /// Uma fatia não é um modelo inteiro, mesmo com o nome pedido - as duas
    /// listas saem da mesma varredura e `shard_range` é o que as separa.
    #[test]
    fn a_slice_is_never_read_as_a_whole_model() {
        let fatia = json!({
            "name": "qwen",
            "path": "/m/qwen_0_15.gguf",
            "size_bytes": 1u64,
            "total_layers": 32,
            "shard_range": [0, 15],
        });
        assert!(parse_whole(&fatia, "qwen").is_none());
    }

    /// A segunda metade da implantação: o que a máquina é mandada carregar.
    ///
    /// É aqui que "modelo inteiro" tem que sair como um alvo comum - faixa
    /// cheia, `skip_split`, sem `merge` e sem posição forçada -, porque é
    /// exatamente essa forma que faz o `apply` no destino se reconhecer como
    /// `single_peer` e subir sem extração de estado oculto. Um `skip_split`
    /// falso aqui mandaria a máquina fatiar um arquivo que já é o modelo todo.
    #[test]
    fn a_whole_model_is_launched_as_one_plain_target() {
        let ms = vec![whole_machine("a", 900_000, true)];
        let p = whole("qwen3.5-9b", &ms, None);
        let alvos = launch_targets(&p.steps, &ms, DeployKind::Whole, None, &[]).expect("um alvo");

        assert_eq!(alvos.len(), 1);
        let alvo = &alvos[0];
        assert_eq!(alvo.node_id, "a");
        // O caminho é o da máquina de destino, lido da varredura - nunca um
        // palpite sobre o sistema de arquivos alheio.
        assert_eq!(alvo.source_model_path, "/a/qwen3.5-9b.gguf");
        assert_eq!((alvo.start_layer, alvo.end_layer), (0, 31));
        assert!(alvo.skip_split, "o arquivo já é o modelo todo");
        assert!(alvo.merge.is_none());
        assert!(
            alvo.peer_index.is_none() && alvo.peer_count.is_none(),
            "a implantação inteira descreve o pipeline, então a posição sai da lista"
        );
    }

    /// Um grupo de tensor parallelism vira um alvo por rank: o primary (rank 0)
    /// e cada worker, todos apontando o mesmo rank0_url para a coletiva.
    #[test]
    fn a_tensor_parallel_group_launches_one_target_per_rank() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let p = whole("qwen3.5-9b", &ms, None);
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let alvos = launch_targets(&p.steps, &ms, DeployKind::TensorParallel, Some(&tp), &[])
            .expect("dois alvos");

        assert_eq!(alvos.len(), 2);

        let primary = &alvos[0];
        assert_eq!(primary.node_id, "a");
        assert_eq!(primary.tp.as_ref().unwrap().role, "primary");
        assert_eq!(primary.tp.as_ref().unwrap().rank, 0);
        // A URL da coletiva é derivada da porta de inferência anunciada pelo
        // primary - o operador só escolhe a máquina, não digita a URL.
        assert_eq!(
            primary.tp.as_ref().unwrap().rank0_url.as_deref(),
            Some("http://a:8181")
        );

        let worker = &alvos[1];
        assert_eq!(worker.node_id, "b");
        assert_eq!(worker.tp.as_ref().unwrap().role, "worker");
        assert_eq!(worker.tp.as_ref().unwrap().rank, 1);
        assert_eq!(
            worker.tp.as_ref().unwrap().rank0_url.as_deref(),
            Some("http://a:8181")
        );
    }

    /// A TP worker can also appear in the explicit selection sent by the UI.
    /// It is a rank in the collective, not a second pipeline stage, so the
    /// launch must still contain exactly one primary and one worker target.
    #[test]
    fn a_tensor_parallel_worker_is_not_launched_as_a_pipeline_stage() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let selection = [pick(0, 31, "a"), pick(0, 31, "b")];
        let p = super::plan(
            "qwen3.5-9b",
            &ms,
            Some(&selection),
            &RuntimeParams::default(),
            false,
            DeployKind::TensorParallel,
            Some(tp.clone()),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);

        let targets = launch_targets(&p.steps, &ms, DeployKind::TensorParallel, Some(&tp), &[])
            .expect("um alvo por rank");
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(targets[0].tp.as_ref().unwrap().role, "primary");
        assert_eq!(targets[1].tp.as_ref().unwrap().role, "worker");
    }

    #[test]
    fn pre_sliced_tensor_parallel_uses_one_existing_file_per_rank() {
        let ms = vec![tp_machine("a", 0), tp_machine("b", 1)];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let p = super::plan(
            "qwen",
            &ms,
            None,
            &RuntimeParams::default(),
            false,
            DeployKind::TensorParallel,
            Some(tp.clone()),
        );

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.tp_targets.len(), 2);
        assert!(p.tp_targets.iter().all(|target| target.ready));
        assert_eq!(p.tp_targets[0].name, "qwen · 1of2");

        let targets = launch_targets(
            &p.steps,
            &ms,
            DeployKind::TensorParallel,
            Some(&tp),
            &p.tp_targets,
        )
        .expect("um alvo por rank pré-fatiado");
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|target| target.skip_split));
        assert_eq!(
            targets[0].part_sha256.as_deref(),
            Some("0000000000000000000000000000000000000000000000000000000000000001")
        );
        assert_eq!(
            targets[1].part_sha256.as_deref(),
            Some("0000000000000000000000000000000000000000000000000000000000000002")
        );
        assert_eq!(targets[0].source_model_path, "/a/rank-0.gguf");
        assert_eq!(targets[1].source_model_path, "/b/rank-1.gguf");
    }

    #[test]
    fn selected_tp_metadata_gate_names_missing_ranks() {
        let machines = vec![tp_machine("a", 0)];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let reason = selected_metadata_pending_reason_for(
            DeployKind::TensorParallel,
            "qwen",
            &[(0, 31)],
            Some(&tp),
            &machines,
        )
        .expect("rank ausente deve manter a seleção bloqueada");
        assert!(reason.contains("ranks 2"), "{reason}");
    }

    #[test]
    fn selected_tp_metadata_gate_opens_when_all_ranks_are_ready() {
        let machines = vec![tp_machine("a", 0), tp_machine("b", 1)];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(selected_metadata_pending_reason_for(
            DeployKind::TensorParallel,
            "qwen",
            &[(0, 31)],
            Some(&tp),
            &machines,
        )
        .is_none());
    }

    /// As validações estáticas do grupo TP recusam o plano antes de qualquer
    /// checagem remota: tamanho, presença no cluster, modelo inteiro e contagem
    /// de workers.
    #[test]
    fn tp_group_checks_rejects_bad_groups() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];

        let ok = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(tp_group_checks(&ok, &ms, "qwen3.5-9b").is_none());

        let small = TpLaunch {
            size: 1,
            primary_node_id: "a".into(),
            worker_node_ids: vec![],
        };
        assert!(tp_group_checks(&small, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("2 ranks"));

        let ghost = TpLaunch {
            size: 2,
            primary_node_id: "zzz".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(tp_group_checks(&ghost, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("não está no cluster"));

        let count = TpLaunch {
            size: 3,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(tp_group_checks(&count, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("precisa de 2 workers"));

        let self_worker = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["a".into()],
        };
        assert!(tp_group_checks(&self_worker, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("próprio primary"));
    }

    /// A recusa do peer é repetida ao operador, em vez de virar palpite sobre a
    /// versão do binário dele. O caso que motivou: `/api/ui/tp/check` é
    /// autenticada, a chamada ia sem a chave, e o `{"error":...}` que voltava
    /// era anunciado como "binário antigo" - com as duas máquinas no mesmo
    /// commit.
    #[test]
    fn a_refusal_is_quoted_not_guessed_as_an_old_binary() {
        assert_eq!(tp_check_verdict(&json!({ "ok": true }), "ksia"), None);

        assert_eq!(
            tp_check_verdict(
                &json!({ "ok": false, "reason": "cabeças não dividem" }),
                "ksia"
            ),
            Some("cabeças não dividem".to_string())
        );

        let recusa =
            tp_check_verdict(&json!({ "error": "missing Authorization header" }), "ksia").unwrap();
        assert!(
            recusa.contains("missing Authorization header"),
            "a razão do peer tem que aparecer: {recusa}"
        );
        assert!(
            !recusa.contains("binário antigo"),
            "peer que recusou não é peer desatualizado: {recusa}"
        );

        // Só um corpo que não explica nada continua valendo como binário velho.
        assert!(tp_check_verdict(&json!({ "outra": 1 }), "ksia")
            .unwrap()
            .contains("binário antigo"));
    }

    /// A checagem tem que chegar autenticada no primary. Sem isto ela é 401 em
    /// qualquer máquina, e nenhuma implantação TP passa.
    #[tokio::test]
    async fn the_shardability_check_carries_the_operator_key() {
        use crate::gateway::testkit::state_with;
        use axum::routing::post as axum_post;
        use std::sync::{Arc, Mutex};

        let visto: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let capturado = visto.clone();
        let app = Router::new().route(
            "/api/ui/tp/check",
            axum_post(move |headers: axum::http::HeaderMap| {
                let capturado = capturado.clone();
                async move {
                    *capturado.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    Json(json!({ "ok": true }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let state = state_with("eu", vec![]).await;
        let machines = vec![Machine {
            node_id: "primary".into(),
            hostname: "ksia".into(),
            gateway_url: Some(format!("http://127.0.0.1:{port}")),
            inference_url: Some(format!("http://127.0.0.1:{port}")),
            inference_candidates: vec![format!("http://127.0.0.1:{port}")],
            addr_candidates: Vec::new(),
            libp2p_peer_id: None,
            supports_tunnel: false,
            inference_capabilities: Some(vec!["speculate/v1-lookup".to_string()]),
            mem_free_mb: 900_000,
            gpu_free_mb: 900_000,
            ram_free_mb: 900_000,
            disk_free_mb: 900_000,
            rtt_ms: Some(0.0),
            load: 0,
            load_known: true,
            held: vec![HeldShard {
                kind: HeldKind::Whole,
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
                start_layer: 0,
                end_layer: 31,
                total_layers: Some(32),
                name: "qwen3.5-9b.gguf".into(),
                path: "/modelos/qwen3.5-9b.gguf".into(),
                mmproj_path: None,
                mmproj_candidates: Vec::new(),
                mmproj_reason: None,
                size_bytes: 1,
            }],
        }];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "primary".into(),
            worker_node_ids: vec!["w".into()],
        };

        assert_eq!(tp_shardability_check(&state, &machines, &tp).await, None);
        let header = visto.lock().unwrap().clone();
        assert_eq!(
            header,
            Some("Bearer chave".to_string()),
            "a checagem chegou sem a chave de operador: {header:?}"
        );
    }

    /// Os membros saem em ordem de rank, e quem não anuncia porta de inferência
    /// é recusado **aqui**.
    ///
    /// O rank 0 dispara o decode de cada membro por essa URL; um membro sem ela
    /// travaria o grupo na primeira barreira, que é o pior lugar para descobrir
    /// o problema.
    #[test]
    fn tp_members_are_rank_ordered_and_a_memberless_url_is_refused() {
        let mut a = whole_machine("a", 900_000, true);
        a.inference_url = Some("https://a:8181".into());
        let mut b = whole_machine("b", 900_000, true);
        b.inference_url = Some("https://b:8181".into());
        let mut mudo = whole_machine("c", 900_000, true);
        mudo.inference_url = None;

        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        // A ordem no cluster não é a ordem dos ranks: o teste embaralha de
        // propósito, porque quem manda é `worker_node_ids`.
        let membros = tp_members(&tp, &[b, a]).unwrap();
        assert_eq!(membros.len(), 2);
        assert_eq!(membros[0].node_id, "a", "rank 0 é o primary");
        assert_eq!(membros[1].url, "https://b:8181");

        let com_mudo = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["c".into()],
        };
        let mut a2 = whole_machine("a", 900_000, true);
        a2.inference_url = Some("https://a:8181".into());
        let erro = tp_members(&com_mudo, &[a2, mudo]).unwrap_err();
        assert!(erro.contains("URL de inferência"), "{erro}");
    }

    /// Uma máquina que não achou o arquivo depois da cópia é erro nomeando o
    /// modelo, não uma implantação que sobe apontando para lugar nenhum.
    #[test]
    fn a_missing_file_after_the_copy_is_an_error_that_names_the_model() {
        let ms = vec![
            whole_machine("tem", 900_000, true),
            whole_machine("nao-tem", 900_000, false),
        ];
        let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "nao-tem")]));
        let erro = launch_targets(&p.steps, &ms, DeployKind::Whole, None, &[])
            .expect_err("o destino não tem o arquivo");
        assert!(
            erro.contains("nao-tem") && erro.contains("o modelo"),
            "{erro}"
        );
    }

    /// O caminho das fatias sai igualzinho pela mesma função - a extração não
    /// mudou o que o `launch` recebe.
    #[test]
    fn the_slice_path_still_launches_the_way_it_did() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 200_000, &[(16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        let alvos =
            launch_targets(&p.steps, &ms, DeployKind::Shards, None, &[]).expect("dois alvos");
        assert_eq!(alvos.len(), 2);
        assert_eq!(alvos[0].source_model_path, "/models/m_0_15.gguf");
        assert_eq!((alvos[0].start_layer, alvos[0].end_layer), (0, 15));
        assert_eq!((alvos[1].start_layer, alvos[1].end_layer), (16, 31));
        assert!(alvos.iter().all(|a| a.skip_split && a.merge.is_none()));
    }

    /// O modelo inteiro vira a faixa 0..total-1, que é o que faz todo o resto
    /// do módulo funcionar sem saber que ele é inteiro.
    #[test]
    fn a_whole_model_reads_as_the_range_it_covers() {
        let inteiro = parse_whole(
            &json!({
                "name": "qwen3.5-9b",
                "path": "/m/qwen3.5-9b.gguf",
                "size_bytes": 5_368_709_120u64,
                "total_layers": 32,
                "shard_range": serde_json::Value::Null,
            }),
            "qwen3.5-9b",
        )
        .expect("um GGUF inteiro com 32 camadas");
        assert_eq!((inteiro.start_layer, inteiro.end_layer), (0, 31));
        assert_eq!(inteiro.total_layers, Some(32));
        assert_eq!(inteiro.name, "qwen3.5-9b");

        // Outro modelo com outro nome não entra no lugar deste.
        assert!(parse_whole(
            &json!({
                "name": "llama3",
                "path": "/m/llama3.gguf",
                "size_bytes": 1u64,
                "total_layers": 32,
                "shard_range": serde_json::Value::Null,
            }),
            "qwen3.5-9b"
        )
        .is_none());
    }
