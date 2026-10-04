#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_preview_must_not_enter_the_native_fit_path() {
        let source = include_str!("webui.rs");
        let endpoint = source
            .split_once("async fn capacity_preview_endpoint(")
            .and_then(|(_, rest)| rest.split_once("Which inference engine the operator asks"))
            .map(|(body, _)| body)
            .expect("o teste precisa encontrar o endpoint de preview");
        assert!(
            !endpoint.contains("preview_fit"),
            "a pré-visualização não pode chamar o fit nativo do carregamento"
        );
        for native_symbol in ["llama_backend_init", "llama_rs_fit_params"] {
            assert!(
                !endpoint.contains(native_symbol),
                "a pré-visualização não pode mencionar a FFI nativa: {native_symbol}"
            );
        }
    }

    #[test]
    fn enrichment_survives_inventory_arc_swap_by_file_identity() {
        let root = std::env::temp_dir().join(format!(
            "pom-webui-catalog-reconcile-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let model_path = models.join("model.gguf");
        std::fs::write(&model_path, crate::splitter::test_gguf_bytes(1)).unwrap();

        let before_refresh = crate::catalog::discover_fast(std::slice::from_ref(&models), &shards);
        let enriched = crate::catalog::enrich(before_refresh);
        assert_eq!(enriched.models[0].architecture.as_deref(), Some("llama"));

        // A refresh always creates a new Arc. The completed work must still be
        // copied when the physical model has the same path, size and mtime.
        let mut after_refresh =
            crate::catalog::discover_fast(std::slice::from_ref(&models), &shards);
        let applied = reconcile_enriched_catalog(&mut after_refresh, &enriched);

        assert_eq!(applied, 1);
        assert!(after_refresh.metadata_complete);
        assert_eq!(
            after_refresh.models[0].architecture.as_deref(),
            Some("llama")
        );
        assert_eq!(after_refresh.models[0].path, model_path.to_string_lossy());

        std::fs::remove_dir_all(root).ok();
    }

    /// The inventory a refresh publishes has to carry the sidecars itself:
    /// the enrichment only runs while a model still lacks its headers, so a
    /// list filled only there was empty again after the next refresh - the
    /// MTP drafts in the shard folder never reached the deploy screen.
    #[tokio::test]
    async fn every_refresh_publishes_the_sidecars_of_the_inventory() {
        let root = std::env::temp_dir().join(format!(
            "pom-webui-catalog-sidecars-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(shards.join("MTP")).unwrap();
        std::fs::write(
            models.join("model.gguf"),
            crate::splitter::test_gguf_bytes(1),
        )
        .unwrap();
        std::fs::write(
            shards.join("MTP").join("mtp-model-shared-Q8_0.gguf"),
            crate::catalog::tests::mtp_draft_fixture("llama", 64, 32),
        )
        .unwrap();
        let roots = normalize_catalog_roots(vec![models.clone()], shards.clone());

        let first = scan_catalog(&roots).await;
        let enriched = crate::catalog::enrich(first.clone());
        let mut refreshed = scan_catalog(&roots).await;
        reconcile_enriched_catalog(&mut refreshed, &enriched);

        for catalog in [&first, &refreshed] {
            assert_eq!(
                catalog
                    .sidecars
                    .iter()
                    .map(|sidecar| sidecar.file_name.as_str())
                    .collect::<Vec<_>>(),
                vec!["mtp-model-shared-Q8_0.gguf"]
            );
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_refresh_claim_is_single_flight() {
        let roots = (vec!["/models".into()], "/slices".into());
        let empty = Arc::new(Catalog {
            models: Vec::new(),
            shard_sets: Vec::new(),
            scanned: Vec::new(),
            metadata_complete: false,
            sidecars: Vec::new(),
            sidecar_roots: Vec::new(),
        });
        let mut cached = CachedCatalog {
            at: Instant::now(),
            roots: roots.clone(),
            catalog: empty,
            refreshing: None,
            enriching: None,
            pending_refresh: None,
        };

        assert!(claim_refresh(&mut cached, roots.clone()));
        assert!(!claim_refresh(&mut cached, roots));

        let roots = (vec!["/models".into()], "/slices".into());
        assert!(claim_enrichment(&mut cached, roots.clone()));
        assert!(!claim_enrichment(&mut cached, roots));
    }

    #[test]
    fn catalog_roots_normalization_ignores_directory_order() {
        let first = normalize_catalog_roots(
            vec!["/models/b".into(), "/models/a".into()],
            "/models/slices".into(),
        );
        let second = normalize_catalog_roots(
            vec!["/models/a".into(), "/models/b".into()],
            "/models/slices".into(),
        );

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn model_metadata_distinguishes_pending_catalog_from_missing_model() {
        let root = std::env::temp_dir().join(format!(
            "pom-webui-catalog-pending-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let requested = models.join("pending.gguf");
        std::fs::write(&requested, b"not-yet-scanned").unwrap();

        let state = crate::gateway::testkit::state_with("catalog-pending", vec![]).await;
        {
            let mut config = state.auth.write().await;
            config.model_dirs = Some(vec![models.to_string_lossy().into_owned()]);
            config.shard_dir = Some(shards.to_string_lossy().into_owned());
        }
        let roots = normalize_catalog_roots(vec![models], shards);
        *catalog_cache().lock().await = Some(CachedCatalog {
            at: Instant::now(),
            roots: roots.clone(),
            catalog: Arc::new(Catalog {
                models: Vec::new(),
                shard_sets: Vec::new(),
                scanned: Vec::new(),
                metadata_complete: false,
                sidecars: Vec::new(),
                sidecar_roots: Vec::new(),
            }),
            refreshing: Some(roots),
            enriching: None,
            pending_refresh: None,
        });

        let error = model_metadata_local(&state, &requested.to_string_lossy())
            .await
            .expect_err("um catálogo pendente não pode ser tratado como pronto");
        assert!(
            error.contains("catálogo deste peer ainda está sendo descoberto"),
            "erro inesperado: {error}"
        );

        *catalog_cache().lock().await = None;
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn slice_folder_cannot_equal_or_nest_inside_a_whole_model_folder() {
        let root = std::env::temp_dir().join(format!("pom-paths-{}", std::process::id()));
        let whole = root.join("whole");
        let nested = whole.join("slices");
        let sibling = root.join("slices");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        let whole = whole.to_string_lossy().into_owned();
        assert!(reject_nested_shard_dir(&[whole.clone()], &whole).is_err());
        assert!(reject_nested_shard_dir(&[whole.clone()], &nested.to_string_lossy()).is_err());
        assert!(reject_nested_shard_dir(&[whole], &sibling.to_string_lossy()).is_ok());

        let _ = std::fs::remove_dir_all(root);
    }

    /// Um alvo TP runtime extrai o próprio rank, mas um alvo que já aponta para
    /// `tp_parts` usa o arquivo existente e não o fatia novamente.
    #[test]
    fn every_rank_of_a_tp_group_extracts_its_own_shard() {
        let req = |tp: Option<crate::network::TpIdentity>| ApplyRequest {
            deploy_id: String::new(),
            part_sha256: None,
            source_model_path: "/m/whole.gguf".into(),
            base_name: "whole".into(),
            model_label: None,
            deployment_name: "whole".into(),
            start_layer: 0,
            end_layer: 31,
            total_layers: 32,
            peer_count: 1,
            peer_index: 0,
            primary: true,
            tp,
            skip_split: false,
            merge: None,
            params: RuntimeParams::default(),
            use_mmproj: false,
            mmproj_path: None,
            mtp_draft_path: None,
            mtp_draft_file: None,
            mmproj_file: None,
        };
        let ident = |rank: u32, role: &str| crate::network::TpIdentity {
            role: role.into(),
            group_id: "g".into(),
            size: 2,
            rank,
            rank0_url: Some("http://127.0.0.1:8181".into()),
            rank0_node_id: Some("primary-id".into()),
            members: Vec::new(),
        };

        let primary = req(Some(ident(0, "primary")));
        let escolhido = tp_shard_to_extract(&primary).expect("o rank 0 runtime também fatia");
        assert_eq!(escolhido.rank, 0);
        assert_eq!(
            (escolhido.rank, escolhido.size),
            (0, 2),
            "o shard que faltava no disco"
        );

        let worker = req(Some(ident(1, "worker")));
        assert_eq!(tp_shard_to_extract(&worker).unwrap().rank, 1);

        let mut pre_sliced = req(Some(ident(0, "primary")));
        pre_sliced.skip_split = true;
        assert!(
            tp_shard_to_extract(&pre_sliced).is_none(),
            "rank TP já fatiado deve ser carregado diretamente"
        );

        assert!(
            tp_shard_to_extract(&req(None)).is_none(),
            "nó sem TP não fatia por tensor"
        );
    }

    /// A rota escolhida tem que chegar a interface: sem ela, "por onde este
    /// salto vai" so se responde com shell na maquina.
    #[test]
    fn the_cluster_view_carries_the_chosen_route() {
        let tabela = crate::route::RouteTable::new();
        let vista = route_view_for("n1", &tabela);
        assert!(vista.is_none(), "peer nao medido nao inventa rota");
    }

    /// Offload e cache KV sao as duas coisas que uma maquina pode discordar do
    /// cluster, e discordar nao pode mexer em mais nada de que o pipeline
    /// depende - contexto, batch e amostragem tem que continuar iguais nos dois
    /// lados do salto.
    #[test]
    fn only_the_hardware_settings_are_per_machine() {
        let base = RuntimeParams {
            n_gpu_layers: 99,
            n_ctx: 32768,
            offload_kqv: true,
            ..RuntimeParams::default()
        };
        let target = |gpu, kqv| LaunchTarget {
            part_sha256: None,
            node_id: "a".into(),
            source_model_path: "/m.gguf".into(),
            start_layer: 0,
            end_layer: 15,
            skip_split: true,
            merge: None,
            n_gpu_layers: gpu,
            offload_kqv: kqv,
            no_embeddings: None,
            peer_index: None,
            peer_count: None,
            tp: None,
            mmproj_path: None,
            mtp_draft_path: None,
            mtp_draft_file: None,
            mmproj_file: None,
            use_mmproj: false,
        };

        let cpu_only = params_for(&base, &target(Some(0), Some(false)));
        assert_eq!(cpu_only.n_gpu_layers, 0);
        assert!(!cpu_only.offload_kqv);
        assert_eq!(cpu_only.n_ctx, 32768, "o resto continua sendo do cluster");

        // Sem sobreposicao, o numero do cluster vale — e sobrepor um nao mexe
        // no outro.
        let herdado = params_for(&base, &target(None, None));
        assert_eq!(herdado.n_gpu_layers, 99);
        assert!(herdado.offload_kqv);
        assert!(params_for(&base, &target(Some(0), None)).offload_kqv);
        assert_eq!(
            params_for(&base, &target(None, Some(false))).n_gpu_layers,
            99
        );
    }

    fn req(start: u32, end: u32, total: u32, peers: u32) -> ApplyRequest {
        ApplyRequest {
            deploy_id: String::new(),
            part_sha256: None,
            source_model_path: "/m.gguf".into(),
            base_name: "m".into(),
            model_label: None,
            deployment_name: "m".into(),
            start_layer: start,
            end_layer: end,
            total_layers: total,
            peer_count: peers,
            peer_index: 0,
            primary: true,
            tp: None,
            skip_split: false,
            merge: None,
            params: RuntimeParams::default(),
            use_mmproj: false,
            mmproj_path: None,
            mtp_draft_path: None,
            mtp_draft_file: None,
            mmproj_file: None,
        }
    }

    #[test]
    fn first_and_last_follow_the_layer_range() {
        let r = req(0, 15, 32, 2);
        assert!(r.is_first());
        assert!(!r.is_last());

        let r = req(16, 31, 32, 2);
        assert!(!r.is_first());
        assert!(r.is_last());
    }

    #[test]
    fn single_peer_only_when_one_node_covers_everything() {
        assert!(req(0, 31, 32, 1).single_peer());
        assert!(!req(0, 15, 32, 2).single_peer());
        assert!(!req(0, 31, 32, 2).single_peer());
    }

    #[test]
    fn params_default_to_the_validated_cluster_settings() {
        let p: RuntimeParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.n_gpu_layers, 99);
        assert_eq!(p.n_ctx, 8192);
        assert_eq!(p.n_batch, 512);
        assert!(p.batching_enabled);
        assert_eq!(p.max_queue_requests, 128);
        assert_eq!(p.batch_window_ms, 0);
        assert_eq!(p.max_batch_sequences, u32::MAX);
        assert_eq!(p.max_batch_tokens, u32::MAX);
        assert!(p.no_thinking);
        assert!(p.offload_kqv);
        assert!(p.fit_memory);
        assert_eq!(
            p.fit_memory_margin_mb,
            crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB
        );
    }

    #[test]
    fn hidden_state_dtype_defaults_to_f16_and_round_trips_supported_formats() {
        let default: RuntimeParams = serde_json::from_str("{}").unwrap();
        assert_eq!(default.hidden_state_dtype, crate::wire::WireDtype::F16);

        let raw: RuntimeParams = serde_json::from_value(serde_json::json!({
            "hidden_state_dtype": "f32"
        }))
        .unwrap();
        assert_eq!(raw.hidden_state_dtype, crate::wire::WireDtype::F32);
        assert_eq!(
            serde_json::to_value(raw).unwrap()["hidden_state_dtype"],
            "f32"
        );

        let explicit: RuntimeParams = serde_json::from_value(serde_json::json!({
            "hidden_state_dtype": "f8"
        }))
        .unwrap();
        assert_eq!(explicit.hidden_state_dtype, crate::wire::WireDtype::F8);
        assert_eq!(
            serde_json::to_value(explicit).unwrap()["hidden_state_dtype"],
            "f8"
        );

        for dtype in ["i4g64", "i4g128"] {
            let explicit: RuntimeParams = serde_json::from_value(serde_json::json!({
                "hidden_state_dtype": dtype
            }))
            .unwrap();
            assert_eq!(explicit.hidden_state_dtype.as_str(), dtype);
            assert_eq!(
                serde_json::to_value(explicit).unwrap()["hidden_state_dtype"],
                dtype
            );
        }
    }

    #[test]
    fn inference_mode_defaults_to_dense_and_round_trips() {
        let default: RuntimeParams = serde_json::from_str("{}").unwrap();
        assert_eq!(default.inference_mode, InferenceMode::Dense);

        let explicit: RuntimeParams = serde_json::from_value(serde_json::json!({
            "inference_mode": "moe"
        }))
        .unwrap();
        assert_eq!(explicit.inference_mode, InferenceMode::Moe);
        let encoded = serde_json::to_value(explicit).unwrap();
        assert_eq!(encoded["inference_mode"], "moe");

        let explicit_auto: RuntimeParams = serde_json::from_value(serde_json::json!({
            "inference_mode": "auto"
        }))
        .unwrap();
        assert_eq!(explicit_auto.inference_mode, InferenceMode::Auto);
        assert_eq!(
            serde_json::to_value(explicit_auto).unwrap()["inference_mode"],
            "auto"
        );
    }

    #[test]
    fn params_accept_partial_overrides() {
        let p: RuntimeParams = serde_json::from_str(
            r#"{"n_ctx":250000,"no_thinking":false,"offload_kqv":false,"fit_memory":false,"fit_memory_margin_mb":512}"#,
        )
        .unwrap();
        assert_eq!(p.n_ctx, 250000);
        assert!(!p.no_thinking);
        assert!(!p.offload_kqv);
        assert!(!p.fit_memory);
        assert_eq!(p.fit_memory_margin_mb, 512);
        assert_eq!(p.top_k, 40, "campos ausentes mantêm o padrão");
    }

    #[test]
    fn selected_model_label_is_not_normalized_to_a_profile_name() {
        let mut request = req(0, 31, 32, 1);
        request.model_label = Some("Qwen3.5-9B-Instruct".into());

        assert_eq!(model_label(&request), "Qwen3.5-9B-Instruct");
    }

    #[test]
    fn sealing_layer_and_tp_parts_merges_them_under_the_source_model() {
        let root = std::env::temp_dir().join(format!(
            "pom-webui-seal-source-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let scratch = root.join(".scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let source_sha256 = "a".repeat(64);
        let layer_sha256 = "b".repeat(64);
        let tp_sha256 = "c".repeat(64);

        let mut layer_req = req(0, 14, 32, 2);
        layer_req.source_model_path = "/models/original.gguf".into();
        layer_req.base_name = "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into();
        std::fs::write(
            crate::pom::part_path(&scratch, &layer_sha256).unwrap(),
            b"layer",
        )
        .unwrap();
        seal_single_part(
            &root,
            &scratch,
            &layer_req,
            &plan::SliceOutcome {
                output_path: crate::pom::part_path(&scratch, &layer_sha256)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                bytes: 5,
                sha256: layer_sha256.clone(),
                reused: false,
            },
            source_sha256.clone(),
            crate::pom::PomKind::Layers,
            None,
            "node-a",
        )
        .unwrap();

        let mut tp_req = req(0, 31, 32, 1);
        tp_req.source_model_path = "/models/original.gguf".into();
        tp_req.base_name = "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(crate::pom::part_path(&scratch, &tp_sha256).unwrap(), b"tp").unwrap();
        seal_single_part(
            &root,
            &scratch,
            &tp_req,
            &plan::SliceOutcome {
                output_path: crate::pom::part_path(&scratch, &tp_sha256)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                bytes: 2,
                sha256: tp_sha256.clone(),
                reused: false,
            },
            source_sha256.clone(),
            crate::pom::PomKind::TensorParallel,
            Some(crate::pom::PomTp { rank: 0, size: 2 }),
            "node-b",
        )
        .unwrap();

        let dir = crate::pom::locate_source_dir(&root, &source_sha256).unwrap();
        assert_eq!(
            dir.file_name().unwrap().to_string_lossy(),
            "Llama-3.1-8B-Instruct-Q4_K_M-aaaaaaaa"
        );
        let manifestos = crate::pom::load_dir(&dir);
        assert_eq!(manifestos.len(), 1);
        let manifest = &manifestos[0];
        assert_eq!(manifest.pom_id, source_sha256);
        assert_eq!(manifest.parts.len(), 1);
        assert_eq!(manifest.tp_parts.len(), 1);
        assert_eq!(manifest.kind, crate::pom::PomKind::Combined);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn tensor_slice_plan_lists_one_full_model_range_per_rank() {
        let root = std::env::temp_dir().join(format!(
            "pom-webui-tp-plan-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("model.gguf");
        std::fs::write(&source, crate::splitter::test_gguf_bytes(4)).unwrap();

        let plan = tensor_slice_plan(&source, 2).unwrap();

        assert_eq!(plan.mode, SliceMode::TensorParallel);
        assert_eq!(plan.tp_size, 2);
        assert_eq!(plan.total_layers, 4);
        assert_eq!(plan.ranks.len(), 2);
        assert_eq!(
            plan.ranks
                .iter()
                .map(|rank| (rank.rank, rank.start_layer, rank.end_layer))
                .collect::<Vec<_>>(),
            vec![(0, 0, 3), (1, 0, 3)]
        );

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn source_hash_reports_bytes_read_before_slicing() {
        let root = std::env::temp_dir().join(format!(
            "pom-source-progress-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("model.gguf");
        let source_bytes = crate::splitter::test_gguf_bytes(4);
        std::fs::write(&source, &source_bytes).unwrap();

        let mut reports = Vec::new();
        let digest = hash_source_with_progress(&source, |done, total| {
            reports.push((done, total));
        })
        .unwrap();

        assert!(!reports.is_empty());
        assert_eq!(reports.first().unwrap().0, 0);
        assert_eq!(
            reports.last().unwrap(),
            &(source_bytes.len() as u64, source_bytes.len() as u64)
        );
        assert_eq!(
            digest,
            crate::multipart::GgufSource::open(&source)
                .unwrap()
                .hash_sha256()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn job_activity_exposes_phase_bytes_and_terminal_error() {
        let id = format!("job-activity-{}", uuid::Uuid::new_v4().simple());
        jobs()
            .write()
            .await
            .insert(id.clone(), new_job(&id, "slice", "modelo", "preparando"));

        set_job_phase(
            &id,
            "lendo modelo",
            "lendo a fonte",
            Some("/models/model.gguf".into()),
        )
        .await;
        set_job_byte_progress(&id, "lendo modelo", 64, 128, None).await;
        let running = jobs().read().await.get(&id).cloned().unwrap();
        assert_eq!(running.step, "lendo modelo");
        assert_eq!(running.bytes_processed, Some(64));
        assert_eq!(running.total_bytes, Some(128));
        assert_eq!(running.current_file.as_deref(), Some("/models/model.gguf"));

        set_job(&id, "failed", "erro", "falha depois da primeira fatia").await;
        let failed = jobs().read().await.get(&id).cloned().unwrap();
        assert_eq!(failed.state, "failed");
        assert_eq!(
            failed.error.as_deref(),
            Some("falha depois da primeira fatia")
        );
        assert!(failed.finished_at.is_some());

        jobs().write().await.remove(&id);
    }

    #[test]
    fn disk_space_status_distinguishes_insufficient_tight_and_safe() {
        let gib = 1u64 << 30;
        assert_eq!(classify_disk_space(90 * gib, 100 * gib), "insufficient");
        assert_eq!(classify_disk_space(105 * gib, 100 * gib), "tight");
        assert_eq!(classify_disk_space(120 * gib, 100 * gib), "ok");
        assert_eq!(classify_disk_space(0, 100 * gib), "unknown");
    }

    #[test]
    fn tensor_slice_request_does_not_require_pipeline_slices() {
        let request: SliceRequest = serde_json::from_str(
            r#"{"model_path":"/models/model.gguf","mode":"tensor_parallel","tp_size":2}"#,
        )
        .unwrap();

        assert!(request.slices.is_empty());
        assert_eq!(request.mode, SliceMode::TensorParallel);
        assert_eq!(request.tp_size, Some(2));
    }

    #[test]
    fn single_range_request_does_not_require_a_size_list() {
        let request: SliceRequest = serde_json::from_str(
            r#"{"model_path":"/models/model.gguf","range":{"start_layer":4,"end_layer":7}}"#,
        )
        .unwrap();

        assert!(request.slices.is_empty());
        assert_eq!(request.range.unwrap().start_layer, 4);
    }

    #[test]
    fn single_range_plan_is_exact_and_rejects_out_of_bounds_ranges() {
        let root = std::env::temp_dir().join(format!(
            "pom-single-range-plan-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("model.gguf");
        std::fs::write(&source, crate::splitter::test_gguf_bytes(4)).unwrap();
        let layout = crate::gguf::read_layout(&source).unwrap();

        let selected = single_range_plan(
            &layout,
            &SliceRange {
                start_layer: 1,
                end_layer: 2,
            },
        )
        .unwrap();
        assert_eq!(selected.ranges.len(), 1);
        assert_eq!(
            (selected.ranges[0].start_layer, selected.ranges[0].end_layer),
            (1, 2)
        );
        assert_eq!(selected.model_bytes, selected.ranges[0].bytes);
        assert!(!selected.complete);

        let error = single_range_plan(
            &layout,
            &SliceRange {
                start_layer: 1,
                end_layer: 99,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("fora do modelo"), "{error}");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_range_reuse_reads_manifest_without_touching_other_parts() {
        let root = std::env::temp_dir().join(format!(
            "pom-existing-range-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let source_sha256 = "a".repeat(64);
        let directory = crate::pom::source_run_dir(&root, "model", &source_sha256);
        std::fs::create_dir_all(&directory).unwrap();
        let old_bytes = vec![1; 2048];
        let other_bytes = vec![2; 2048];
        let old_sha256 = hex::encode(Sha256::digest(&old_bytes));
        let other_sha256 = hex::encode(Sha256::digest(&other_bytes));
        let old = crate::pom::PomPart {
            sha256: old_sha256,
            bytes: 2048,
            start_layer: 0,
            end_layer: 1,
            is_first: true,
            is_last: false,
            tp: None,
        };
        let other = crate::pom::PomPart {
            sha256: other_sha256,
            bytes: 2048,
            start_layer: 2,
            end_layer: 3,
            is_first: false,
            is_last: true,
            tp: None,
        };
        std::fs::write(
            crate::pom::part_path(&directory, &old.sha256).unwrap(),
            old_bytes,
        )
        .unwrap();
        std::fs::write(
            crate::pom::part_path(&directory, &other.sha256).unwrap(),
            other_bytes,
        )
        .unwrap();
        let manifest = crate::pom::PomManifest::new(
            crate::pom::PomModel {
                name: "model".into(),
                label: "model".into(),
                total_layers: 4,
                quant: None,
            },
            crate::pom::PomKind::Layers,
            "test",
            source_sha256.clone(),
            vec![old.clone(), other.clone()],
        );
        crate::pom::write_atomic(&directory, &manifest).unwrap();

        let reused = existing_layer_slice(&root, &source_sha256, 0, 1).unwrap();
        assert!(reused.reused);
        assert_eq!(reused.sha256, old.sha256);
        assert!(crate::pom::part_path(&directory, &other.sha256)
            .unwrap()
            .is_file());
        assert_eq!(crate::pom::load_dir(&directory)[0].parts.len(), 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tensor_slice_manifest_keeps_all_ranks_in_tp_parts() {
        let manifest = tensor_slice_manifest(
            "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into(),
            "Llama".into(),
            32,
            "node-a".into(),
            "a".repeat(64),
            None,
            vec![
                crate::pom::PomPart {
                    sha256: "b".repeat(64),
                    bytes: 10,
                    start_layer: 0,
                    end_layer: 31,
                    is_first: true,
                    is_last: true,
                    tp: Some(crate::pom::PomTp { rank: 0, size: 2 }),
                },
                crate::pom::PomPart {
                    sha256: "c".repeat(64),
                    bytes: 11,
                    start_layer: 0,
                    end_layer: 31,
                    is_first: true,
                    is_last: true,
                    tp: Some(crate::pom::PomTp { rank: 1, size: 2 }),
                },
            ],
        );

        assert_eq!(manifest.kind, crate::pom::PomKind::TensorParallel);
        assert!(manifest.parts.is_empty());
        assert_eq!(manifest.tp_parts.len(), 2);
        manifest.validate().unwrap();
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn test_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
        let end = pos.checked_add(4)?;
        let value = u32::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
        *pos = end;
        Some(value)
    }

    fn test_u64(bytes: &[u8], pos: &mut usize) -> Option<u64> {
        let end = pos.checked_add(8)?;
        let value = u64::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
        *pos = end;
        Some(value)
    }

    fn test_string(bytes: &[u8], pos: &mut usize) -> Option<String> {
        let len = usize::try_from(test_u64(bytes, pos)?).ok()?;
        let end = pos.checked_add(len)?;
        let value = std::str::from_utf8(bytes.get(*pos..end)?).ok()?.to_owned();
        *pos = end;
        Some(value)
    }

    fn skip_test_value(bytes: &[u8], pos: &mut usize, value_type: u32) -> Option<()> {
        let scalar_size = match value_type {
            0 | 1 | 7 => Some(1),
            2 | 3 => Some(2),
            4 | 5 | 6 => Some(4),
            10..=12 => Some(8),
            _ => None,
        };
        if let Some(size) = scalar_size {
            *pos = pos.checked_add(size)?;
            return bytes.get(..*pos).map(|_| ());
        }
        match value_type {
            8 => {
                let len = usize::try_from(test_u64(bytes, pos)?).ok()?;
                *pos = pos.checked_add(len)?;
                bytes.get(..*pos).map(|_| ())
            }
            9 => {
                let element_type = test_u32(bytes, pos)?;
                let count = usize::try_from(test_u64(bytes, pos)?).ok()?;
                for _ in 0..count {
                    skip_test_value(bytes, pos, element_type)?;
                }
                Some(())
            }
            _ => None,
        }
    }

    fn test_array(bytes: &[u8], wanted: &str) -> Option<(u32, Vec<u8>)> {
        let mut pos = 0;
        if test_u32(bytes, &mut pos)? != 0x4655_4747 {
            return None;
        }
        test_u32(bytes, &mut pos)?;
        test_u64(bytes, &mut pos)?;
        let kv_count = usize::try_from(test_u64(bytes, &mut pos)?).ok()?;
        for _ in 0..kv_count {
            let key = test_string(bytes, &mut pos)?;
            let value_type = test_u32(bytes, &mut pos)?;
            if key == wanted && value_type == 9 {
                let element_type = test_u32(bytes, &mut pos)?;
                let count = usize::try_from(test_u64(bytes, &mut pos)?).ok()?;
                let width = match element_type {
                    0 | 1 | 7 => 1,
                    2 | 3 => 2,
                    4 | 5 | 6 => 4,
                    10..=12 => 8,
                    _ => return None,
                };
                let end = pos.checked_add(count.checked_mul(width)?)?;
                return Some((element_type, bytes.get(pos..end)?.to_vec()));
            }
            skip_test_value(bytes, &mut pos, value_type)?;
        }
        None
    }

    fn test_u32_array(bytes: &[u8], wanted: &str) -> Option<Vec<u32>> {
        let (element_type, payload) = test_array(bytes, wanted)?;
        (element_type == 4 && payload.len() % 4 == 0).then(|| {
            payload
                .chunks_exact(4)
                .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
                .collect()
        })
    }

    fn test_bool_array(bytes: &[u8], wanted: &str) -> Option<Vec<bool>> {
        let (element_type, payload) = test_array(bytes, wanted)?;
        (element_type == 7).then(|| payload.into_iter().map(|value| value != 0).collect())
    }

    /// `merge_parts` with two real mini-GGUF slices: the execution path the
    /// planner's merged steps feed into, exercised with files instead of mocks.
    #[test]
    fn merge_parts_joins_two_slices_into_a_whole() {
        block_on(async {
            let root = std::env::temp_dir().join(format!("pom-webui-merge-{}", std::process::id()));
            let parts_dir = root.join("parts");
            std::fs::create_dir_all(&parts_dir).unwrap();
            let source_sha256 = "d".repeat(64);
            let source = root.join("source.gguf");
            std::fs::write(&source, crate::splitter::test_gguf_bytes(32)).unwrap();
            let write_part = |dir: &std::path::Path, start: u32, end: u32| {
                std::fs::create_dir_all(dir).unwrap();
                let tmp = dir.join("tmp");
                crate::splitter::split(&source, &tmp, start, end, start == 0, end == 31, |_| {})
                    .unwrap();
                let (sha256, bytes) = crate::pom::hash_file(&tmp).unwrap();
                let path = crate::pom::part_path(dir, &sha256).unwrap();
                std::fs::rename(&tmp, &path).unwrap();
                let manifest = crate::pom::PomManifest::new(
                    crate::pom::PomModel {
                        name: "m".into(),
                        label: "m".into(),
                        total_layers: 32,
                        quant: None,
                    },
                    crate::pom::PomKind::Layers,
                    "test",
                    source_sha256.clone(),
                    vec![crate::pom::PomPart {
                        sha256: sha256.clone(),
                        bytes,
                        start_layer: start,
                        end_layer: end,
                        is_first: start == 0,
                        is_last: end == 31,
                        tp: None,
                    }],
                );
                crate::pom::write_atomic(dir, &manifest).unwrap();
                path
            };
            let a = write_part(&parts_dir.join("a"), 0, 15);
            let b = write_part(&parts_dir.join("b"), 16, 31);

            let req = req(0, 31, 32, 1);
            let parts = vec![
                MergeSource {
                    source_model_path: a.to_string_lossy().into_owned(),
                    start_layer: 0,
                    end_layer: 15,
                },
                MergeSource {
                    source_model_path: b.to_string_lossy().into_owned(),
                    start_layer: 16,
                    end_layer: 31,
                },
            ];
            let out = merge_parts(root.clone(), &req, &parts, "job-merge")
                .await
                .unwrap();
            let out_path = std::path::Path::new(&out);
            // O nome é o conteúdo: quem junta não escolhe como chamar o
            // resultado, o hash é que o nomeia.
            let nome = out_path.file_stem().unwrap().to_string_lossy().into_owned();
            assert!(
                crate::pom::is_sha256_hex(&nome),
                "o modelo junto sai endereçado por conteúdo, não por convenção de nome: {nome}"
            );
            assert_eq!(
                crate::pom::hash_file(out_path).unwrap().0,
                nome,
                "e o hash do arquivo tem que ser o nome dele"
            );
            let layout = crate::gguf::read_layout(out_path).unwrap();
            assert_eq!(
                layout.layer_count(),
                32,
                "as duas fatias juntas cobrem o modelo"
            );
            let merged_bytes = std::fs::read(out_path).unwrap();
            assert_eq!(
                test_u32_array(&merged_bytes, "llama.attention.head_count_kv_by_layer"),
                Some((0..32).collect()),
                "a metadata de cabeças KV precisa cobrir as duas fatias"
            );
            assert_eq!(
                test_bool_array(&merged_bytes, "llama.attention.sliding_window_pattern"),
                Some((0..32).map(|layer| layer % 2 == 0).collect()),
                "a janela deslizante precisa cobrir as duas fatias"
            );
            assert!(out_path.metadata().unwrap().len() > a.metadata().unwrap().len());

            // A stale file under a content address must not be published as a
            // merged model. The second apply repairs it from the real slices.
            let marker = vec![0xAAu8; 2048];
            std::fs::write(out_path, &marker).unwrap();
            let again = merge_parts(root.clone(), &req, &parts, "job-merge-2")
                .await
                .unwrap();
            assert_eq!(again, out);
            assert_eq!(
                std::fs::read(out_path).unwrap() == marker,
                false,
                "um arquivo inválido no endereço de conteúdo não pode ser reaproveitado"
            );
            crate::gguf::read_layout(out_path).unwrap();

            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn merge_parts_refuses_one_part_or_a_gap() {
        block_on(async {
            let root =
                std::env::temp_dir().join(format!("pom-webui-merge-bad-{}", std::process::id()));
            let parts_dir = root.join("parts");
            std::fs::create_dir_all(&parts_dir).unwrap();
            let a = parts_dir.join("m_0_15.gguf");
            let b = parts_dir.join("m_17_31.gguf");
            std::fs::write(&a, crate::splitter::test_gguf_bytes(16)).unwrap();
            std::fs::write(&b, crate::splitter::test_gguf_bytes(15)).unwrap();

            let req = req(0, 31, 32, 1);
            let single = vec![MergeSource {
                source_model_path: a.to_string_lossy().into_owned(),
                start_layer: 0,
                end_layer: 15,
            }];
            let err = merge_parts(root.clone(), &req, &single, "job-1")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("menos de duas"), "{err}");

            let gap = vec![
                MergeSource {
                    source_model_path: a.to_string_lossy().into_owned(),
                    start_layer: 0,
                    end_layer: 15,
                },
                MergeSource {
                    source_model_path: b.to_string_lossy().into_owned(),
                    start_layer: 17,
                    end_layer: 31,
                },
            ];
            let err = merge_parts(root.clone(), &req, &gap, "job-2")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("vizinhas"), "{err}");

            std::fs::remove_dir_all(&root).ok();
        });
    }
}
