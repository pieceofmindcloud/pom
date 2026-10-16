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

fn plan_requesting(
    machines: &[Machine],
    selection: &[SelectedSlice],
    mmproj: Option<&str>,
) -> Plan {
    plan_with_rtt_options(
        "vision",
        machines,
        Some(selection),
        &RuntimeParams::default(),
        false,
        DeployKind::Shards,
        None,
        DeployStrategy::Simple,
        "vision",
        &[],
        &PairwiseRtt::default(),
        None,
        mmproj,
    )
}

/// The screen may send another machine's projector path (the admin listed
/// its own catalog). The head has the same copy in another folder: the plan
/// uses the head's path, by file name, instead of refusing.
#[test]
fn a_projector_from_another_machine_resolves_to_the_heads_copy_by_name() {
    let mut machines = build(vec![
        m("head", 100_000, &[(0, 15)]),
        m("tail", 100_000, &[(16, 31)]),
    ]);
    machines[0].held[0].mmproj_path = Some("/models/mmproj-vision-f16.gguf".into());
    machines[0].held[0].mmproj_candidates = vec![
        "/models/mmproj-vision-f16.gguf".into(),
        "/models/mmproj-vision-q8.gguf".into(),
    ];
    let selection = [pick(0, 15, "head"), pick(16, 31, "tail")];

    let planned = plan_requesting(
        &machines,
        &selection,
        Some("/data/models/mmproj-vision-q8.gguf"),
    );
    assert_eq!(planned.blocked, None);
    assert_eq!(
        planned.mmproj_path.as_deref(),
        Some("/models/mmproj-vision-q8.gguf")
    );

    let planned = plan_requesting(
        &machines,
        &selection,
        Some("/models/mmproj-vision-f16.gguf"),
    );
    assert_eq!(
        planned.mmproj_path.as_deref(),
        Some("/models/mmproj-vision-f16.gguf")
    );

    // The head does not have that projector: refuse, naming it.
    let planned = plan_requesting(
        &machines,
        &selection,
        Some("/data/models/mmproj-other.gguf"),
    );
    assert!(planned
        .blocked
        .as_deref()
        .is_some_and(|reason| reason.contains("mmproj-other.gguf")));
}

#[test]
fn resolve_head_projector_refuses_an_ambiguous_name() {
    let candidates = vec!["/a/mmproj.gguf".to_string(), "/b/mmproj.gguf".to_string()];
    assert_eq!(
        super::resolve_head_projector("/a/mmproj.gguf", &candidates).as_deref(),
        Some("/a/mmproj.gguf")
    );
    assert_eq!(
        super::resolve_head_projector("/c/mmproj.gguf", &candidates),
        None
    );
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
    assert_eq!(
        planned.mmproj_path.as_deref(),
        Some("/selected/mmproj.gguf")
    );

    let targets = super::launch_targets(&planned.steps, &machines, DeployKind::Shards, None, &[])
        .expect("targets should be launchable");
    assert_eq!(
        targets[0].mmproj_path.as_deref(),
        Some("/selected/mmproj.gguf")
    );
    assert_eq!(targets[1].mmproj_path, None);

    let selection = [pick(0, 15, "unselected-head"), pick(16, 31, "tail")];
    let planned = plan("vision", &machines, Some(&selection));
    assert_eq!(
        planned.mmproj_path.as_deref(),
        Some("/unselected/mmproj.gguf")
    );

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
        n_ubatch: llama_engine::DEFAULT_N_UBATCH,
        max_sessions: 2,
        max_new_tokens: 256,
        prefill_chunk_tokens: 4096,
        ..RuntimeParams::default()
    };
    let p = super::plan("m", &ms, None, &params, false, DeployKind::Shards, None);

    assert_eq!(p.params.n_ctx, 32768);
    // Each session receives 32768; 256 stay reserved for the answer.
    assert_eq!(p.prompt_tokens, 32512);
    // The chunk never exceeds n_batch, however high the request is.
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
        assert_eq!(effective, expected, "{kind:?} rewrote the deploy");
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
        Some("Tensor Parallelism does not support MoE in this version.")
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
    assert_eq!(p.steps[0].n_gpu_layers, None, "inherits the cluster value");
    assert_eq!(p.steps[1].n_gpu_layers, Some(0));
}

/// Where the KV cache lives is decided by the machine's VRAM, and the machines of the
/// cluster do not look alike: the choice made for one must not show up on the
/// other, nor erase the one that was not chosen.
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
    assert_eq!(p.steps[0].offload_kqv, None, "inherits the cluster choice");
    assert_eq!(p.steps[1].offload_kqv, Some(false));
    // One axis does not move the other: neither of the two asked for offload.
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
// died on `runtime::NodeConfig::validate` with "total_layers cannot be
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
        "0-15 and 16-31 reach layer 32"
    );
    assert!(p.total_layers_inferred, "came from the coverage, not from the file");
    assert!(p.blocked.is_none(), "{:?}", p.blocked);
    assert!(p.complete);
    assert!(
        p.warnings.iter().any(|w| w.contains("deduced")),
        "the screen must say the total was deduced: {:?}",
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
    let reason = p.blocked.expect("no total and no layer 0 is a refusal");
    assert!(
        reason.contains("m_16_31.gguf"),
        "the refusal must say which slice lacks the data: {reason}"
    );
    assert!(
        reason.contains("Slice screen"),
        "and what to do about it: {reason}"
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
    assert_eq!(p.missing, vec![[16, 31]], "L16-31 was left out");
    assert!(!p.complete, "half a network is not a complete network");
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
    .expect("the slice is valid, it just does not state the total");
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
    assert_eq!(
        inherited.mmproj_path.as_deref(),
        Some("/models/mmproj-vision.gguf")
    );
}

// ── A whole model ───────────────────────────────────────────────────────
//
// The degenerate case: one machine, all the layers. What these tests
// guard is not a second implementation - it is that there is **not** one. A whole
// model enters the same `HeldShard`, comes out as the same `Step` and is
// launched by the same `launch`; what changes is where the inventory is read from and
// which noun the refusal uses.

/// A machine with (or without) the whole 32-layer GGUF.
fn whole_machine(id: &'static str, mem_free_mb: u64, has: bool) -> Machine {
    Machine {
        node_id: id.to_string(),
        hostname: id.to_string(),
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

/// The plan of a whole model is one machine covering everything - and it is
/// `complete`, with no gap and no overlap, because the only range that
/// exists is the whole model.
#[test]
fn a_whole_model_is_one_machine_covering_every_layer() {
    let ms = vec![whole_machine("a", 900_000, true)];
    let p = whole("qwen3.5-9b", &ms, None);

    assert!(p.blocked.is_none(), "{:?}", p.blocked);
    assert!(p.complete, "a 0-31 range covers a model of 32 layers");
    assert!(p.missing.is_empty() && p.overlap.is_empty());
    assert_eq!(p.total_layers, Some(32));
    assert!(
        !p.total_layers_inferred,
        "the whole GGUF declares the total; nothing was deduced"
    );
    assert_eq!(p.kind, DeployKind::Whole);

    assert_eq!(p.steps.len(), 1);
    let s = &p.steps[0];
    assert_eq!((s.start_layer, s.end_layer), (0, 31));
    assert_eq!(s.position, 1);
    assert!(s.primary, "whoever has layer 0 receives the prompt");
    assert!(s.ready, "the model is already on the machine");
    assert!(!s.merged && s.parts.is_empty());
    assert_eq!(s.target_node_id, "a");
    assert_eq!(p.transfer_bytes, 0);
}

/// The operator chooses the machine, and the choice is validated like any
/// other: the range is that of the whole model.
#[test]
fn the_operator_picks_the_machine_for_a_whole_model() {
    let ms = vec![
        whole_machine("a", 900_000, true),
        whole_machine("b", 900_000, true),
    ];
    let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "b")]));

    assert!(p.blocked.is_none(), "{:?}", p.blocked);
    assert!(!p.auto, "it is the operator's choice, not the node's suggestion");
    assert_eq!(p.steps.len(), 1);
    assert_eq!(p.steps[0].target_node_id, "b");
    assert!(p.steps[0].ready);
}

/// Two copies of the whole model are two valid routes for the same
/// range. The error of a model chosen twice only applies when the same
/// target appears twice in the selection.
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
        "an exact replica must not block: {:?}",
        p.blocked
    );
    assert!(p.complete);
    assert_eq!(p.steps.len(), 2);
    assert_eq!(p.steps[0].target_node_id, "a");
    assert_eq!(p.steps[1].target_node_id, "b");
}

/// A machine that does not have the file receives a copy, exactly like a
/// slice - and the plan names who sends it.
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
    // The warning exists because `transfer::safe_model_path` pins the "pull"
    // direction to the sender's slice folder, and a whole model almost
    // always lives outside it.
    assert!(
        s.note
            .as_deref()
            .unwrap_or_default()
            .contains("Transfers"),
        "the copy of a whole model must say where it can come from: {:?}",
        s.note
    );
}

/// The refusal speaks of what is on the screen. "a fatia L0-31" names something that
/// the operator who chose a whole model does not see anywhere.
#[test]
fn a_whole_model_refusal_names_the_model_and_not_a_slice() {
    let ms = vec![whole_machine("a", 900_000, true)];
    let without_machine = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "")]));
    let reason = without_machine.blocked.expect("no machine is a refusal");
    assert!(
        reason.contains("the model qwen3.5-9b") && !reason.contains("slice"),
        "{reason}"
    );

    let nothing = whole("qwen3.5-9b", &ms, Some(&[]));
    let reason = nothing.blocked.expect("nothing chosen is a refusal");
    assert_eq!(reason, "no model selected");

    let nobody = whole("nao-existe", &[whole_machine("a", 900_000, false)], None);
    let reason = nobody.blocked.expect("nobody has the model");
    assert!(reason.contains("whole model nao-existe") || reason.contains("nao-existe"));
    assert!(!reason.contains("slices of"), "{reason}");
}

/// The target menu says "já tem o modelo", not "já tem a fatia".
#[test]
fn the_target_menu_speaks_of_a_model() {
    let ms = vec![
        whole_machine("a", 900_000, true),
        whole_machine("b", 900_000, false),
    ];
    let p = whole("qwen3.5-9b", &ms, None);
    let options = &p.steps[0].options;
    let a = options.iter().find(|o| o.node_id == "a").unwrap();
    assert!(a.has);
    assert_eq!(a.note.as_deref(), Some("already has the model"));
    let b = options.iter().find(|o| o.node_id == "b").unwrap();
    assert!(!b.has);
    assert!(b.note.as_deref().unwrap().contains("would receive"));
}

/// Layer 0 is the one that processes the whole prompt, and with a whole model
/// it is the chosen machine - which the suggestion sends to the roomiest one.
#[test]
fn the_suggestion_puts_a_whole_model_on_the_roomiest_machine() {
    let ms = vec![
        whole_machine("pequena", 10_000, true),
        whole_machine("grande", 900_000, true),
    ];
    let p = whole("qwen3.5-9b", &ms, None);
    assert_eq!(p.steps.len(), 1, "a whole model is one step, always");
    assert_eq!(p.steps[0].target_node_id, "grande");
}

/// A model whose GGUF header did not give the number of layers does not become a
/// deployment with `total_layers` zero: it simply does not enter the
/// inventory. `NodeConfig::validate` would refuse the zero at the target, after
/// `launch` had already stopped the inference there.
#[test]
fn a_model_with_no_layer_count_never_becomes_a_candidate() {
    let without_layers = json!({
        "name": "quebrado",
        "path": "/m/quebrado.gguf",
        "size_bytes": 1u64,
        "total_layers": serde_json::Value::Null,
        "shard_range": serde_json::Value::Null,
    });
    assert!(parse_whole(&without_layers, "quebrado").is_none());

    let zero = json!({
        "name": "quebrado",
        "path": "/m/quebrado.gguf",
        "size_bytes": 1u64,
        "total_layers": 0,
        "shard_range": serde_json::Value::Null,
    });
    assert!(parse_whole(&zero, "quebrado").is_none());
}

/// A slice is not a whole model, even with the requested name - the two
/// lists come out of the same survey and `shard_range` is what separates them.
#[test]
fn a_slice_is_never_read_as_a_whole_model() {
    let slice = json!({
        "name": "qwen",
        "path": "/m/qwen_0_15.gguf",
        "size_bytes": 1u64,
        "total_layers": 32,
        "shard_range": [0, 15],
    });
    assert!(parse_whole(&slice, "qwen").is_none());
}

/// The second half of the deployment: what the machine is told to load.
///
/// This is where "whole model" has to come out as an ordinary target - full
/// range, `skip_split`, without `merge` and without a forced position -, because it is
/// exactly that shape that makes the `apply` at the target recognize itself as
/// `single_peer` and come up without hidden-state extraction. A false
/// `skip_split` here would send the machine to slice a file that is already the whole model.
#[test]
fn a_whole_model_is_launched_as_one_plain_target() {
    let ms = vec![whole_machine("a", 900_000, true)];
    let p = whole("qwen3.5-9b", &ms, None);
    let targets = launch_targets(&p.steps, &ms, DeployKind::Whole, None, &[]).expect("one target");

    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert_eq!(target.node_id, "a");
    // The path is the target machine's, read from the survey - never a
    // guess about someone else's filesystem.
    assert_eq!(target.source_model_path, "/a/qwen3.5-9b.gguf");
    assert_eq!((target.start_layer, target.end_layer), (0, 31));
    assert!(target.skip_split, "the file is already the whole model");
    assert!(target.merge.is_none());
    assert!(
        target.peer_index.is_none() && target.peer_count.is_none(),
        "the whole deployment describes the pipeline, so the position comes from the list"
    );
}

/// A tensor parallelism group becomes one target per rank: the primary (rank 0)
/// and each worker, all pointing to the same rank0_url for the collective.
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
    let targets = launch_targets(&p.steps, &ms, DeployKind::TensorParallel, Some(&tp), &[])
        .expect("two targets");

    assert_eq!(targets.len(), 2);

    let primary = &targets[0];
    assert_eq!(primary.node_id, "a");
    assert_eq!(primary.tp.as_ref().unwrap().role, "primary");
    assert_eq!(primary.tp.as_ref().unwrap().rank, 0);
    // The collective's URL is derived from the inference port announced by the
    // primary - the operator only chooses the machine, does not type the URL.
    assert_eq!(
        primary.tp.as_ref().unwrap().rank0_url.as_deref(),
        Some("http://a:8181")
    );

    let worker = &targets[1];
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
        .expect("one target per rank");
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
    .expect("one target per pre-sliced rank");
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
    .expect("a missing rank must keep the selection blocked");
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

/// The static validations of the TP group refuse the plan before any
/// remote check: size, presence in the cluster, whole model and count
/// of workers.
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
        .contains("is not in the cluster"));

    let count = TpLaunch {
        size: 3,
        primary_node_id: "a".into(),
        worker_node_ids: vec!["b".into()],
    };
    assert!(tp_group_checks(&count, &ms, "qwen3.5-9b")
        .unwrap()
        .contains("needs 2 workers"));

    let self_worker = TpLaunch {
        size: 2,
        primary_node_id: "a".into(),
        worker_node_ids: vec!["a".into()],
    };
    assert!(tp_group_checks(&self_worker, &ms, "qwen3.5-9b")
        .unwrap()
        .contains("primary itself"));
}

/// The peer's refusal is repeated to the operator, instead of becoming a guess about
/// its binary version. The case that motivated it: `/api/ui/tp/check` is
/// authenticated, the call went without the key, and the `{"error":...}` that came back
/// was announced as "old binary" - with both machines on the same
/// commit.
#[test]
fn a_refusal_is_quoted_not_guessed_as_an_old_binary() {
    assert_eq!(tp_check_verdict(&json!({ "ok": true }), "ksia"), None);

    assert_eq!(
        tp_check_verdict(
            &json!({ "ok": false, "reason": "heads do not split" }),
            "ksia"
        ),
        Some("heads do not split".to_string())
    );

    let refusal =
        tp_check_verdict(&json!({ "error": "missing Authorization header" }), "ksia").unwrap();
    assert!(
        refusal.contains("missing Authorization header"),
        "the peer reason must show up: {refusal}"
    );
    assert!(
        !refusal.contains("old binary"),
        "a peer that refused is not an outdated peer: {refusal}"
    );

    // Only a body that explains nothing still counts as an old binary.
    assert!(tp_check_verdict(&json!({ "outra": 1 }), "ksia")
        .unwrap()
        .contains("old binary"));
}

/// The check has to arrive authenticated at the primary. Without it, it is 401 on
/// any machine, and no TP deployment passes.
#[tokio::test]
async fn the_shardability_check_carries_the_operator_key() {
    use crate::gateway::testkit::state_with;
    use axum::routing::post as axum_post;
    use std::sync::{Arc, Mutex};

    let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured = seen.clone();
    let app = Router::new().route(
        "/api/ui/tp/check",
        axum_post(move |headers: axum::http::HeaderMap| {
            let captured = captured.clone();
            async move {
                *captured.lock().unwrap() = headers
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

    let state = state_with(
        "eu",
        vec![crate::gateway::testkit::peer("primary", "ksia", port)],
    )
    .await;
    let machines = vec![Machine {
        node_id: "primary".into(),
        hostname: "ksia".into(),
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
            path: "/models/qwen3.5-9b.gguf".into(),
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
    let header = seen.lock().unwrap().clone();
    assert_eq!(
        header,
        Some("Bearer chave".to_string()),
        "the check arrived without the operator key: {header:?}"
    );
}

/// The members come out in rank order, and whoever does not announce an inference port
/// is refused **here**.
///
/// Rank 0 triggers each member's decode through that URL; a member without it
/// would hang the group on the first barrier, which is the worst place to discover
/// the problem.
#[test]
fn tp_members_are_rank_ordered_and_a_memberless_url_is_refused() {
    let mut a = whole_machine("a", 900_000, true);
    a.inference_url = Some("https://a:8181".into());
    let mut b = whole_machine("b", 900_000, true);
    b.inference_url = Some("https://b:8181".into());
    let mut silent = whole_machine("c", 900_000, true);
    silent.inference_url = None;

    let tp = TpLaunch {
        size: 2,
        primary_node_id: "a".into(),
        worker_node_ids: vec!["b".into()],
    };
    // The order in the cluster is not the order of the ranks: the test shuffles on
    // purpose, because whoever commands is `worker_node_ids`.
    let members = tp_members(&tp, &[b, a]).unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].node_id, "a", "rank 0 is the primary");
    assert_eq!(members[1].url, "https://b:8181");

    let with_silent = TpLaunch {
        size: 2,
        primary_node_id: "a".into(),
        worker_node_ids: vec!["c".into()],
    };
    let mut a2 = whole_machine("a", 900_000, true);
    a2.inference_url = Some("https://a:8181".into());
    let error = tp_members(&with_silent, &[a2, silent]).unwrap_err();
    assert!(error.contains("inference URL"), "{error}");
}

/// A machine that did not find the file after the copy is an error naming the
/// model, not a deployment that comes up pointing nowhere.
#[test]
fn a_missing_file_after_the_copy_is_an_error_that_names_the_model() {
    let ms = vec![
        whole_machine("tem", 900_000, true),
        whole_machine("nao-tem", 900_000, false),
    ];
    let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "nao-tem")]));
    let error = launch_targets(&p.steps, &ms, DeployKind::Whole, None, &[])
        .expect_err("the destination does not have the file");
    assert!(
        error.contains("nao-tem") && error.contains("the model"),
        "{error}"
    );
}

/// The path of the slices comes out exactly the same through the same function - the
/// extraction did not change what `launch` receives.
#[test]
fn the_slice_path_still_launches_the_way_it_did() {
    let ms = build(vec![
        m("a", 100_000, &[(0, 15)]),
        m("b", 200_000, &[(16, 31)]),
    ]);
    let p = plan("m", &ms, None);
    let targets = launch_targets(&p.steps, &ms, DeployKind::Shards, None, &[]).expect("two targets");
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].source_model_path, "/models/m_0_15.gguf");
    assert_eq!((targets[0].start_layer, targets[0].end_layer), (0, 15));
    assert_eq!((targets[1].start_layer, targets[1].end_layer), (16, 31));
    assert!(targets.iter().all(|a| a.skip_split && a.merge.is_none()));
}

/// The whole model becomes the range 0..total-1, which is what makes the whole rest
/// of the module work without knowing that it is whole.
#[test]
fn a_whole_model_reads_as_the_range_it_covers() {
    let whole = parse_whole(
        &json!({
            "name": "qwen3.5-9b",
            "path": "/m/qwen3.5-9b.gguf",
            "size_bytes": 5_368_709_120u64,
            "total_layers": 32,
            "shard_range": serde_json::Value::Null,
        }),
        "qwen3.5-9b",
    )
    .expect("a whole GGUF with 32 layers");
    assert_eq!((whole.start_layer, whole.end_layer), (0, 31));
    assert_eq!(whole.total_layers, Some(32));
    assert_eq!(whole.name, "qwen3.5-9b");

    // Another model with another name does not take the place of this one.
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

fn sidecar(
    kind: crate::catalog::SidecarKind,
    name: &str,
    size: u64,
    path: &str,
) -> crate::catalog::Sidecar {
    crate::catalog::Sidecar {
        kind,
        path: path.to_string(),
        file_name: name.to_string(),
        size_bytes: size,
        architecture: Some(
            if kind == crate::catalog::SidecarKind::Mtp {
                "llama"
            } else {
                "clip"
            }
            .to_string(),
        ),
        dim: Some(3072),
        general_name: None,
        quant: None,
    }
}

/// The projector belongs to the head and the MTP draft to the tail, and
/// neither has to be there already: the one that is missing is copied from
/// whoever holds it. The screen's choice wins over the saved link, and
/// the link fills in what the screen left open.
#[test]
fn vision_goes_to_the_head_and_mtp_to_the_tail_copied_when_missing() {
    use crate::catalog::{SidecarKind, SidecarLink, SidecarMode};
    let machines = build(vec![
        m("head", 20_000, &[(0, 15)]),
        m("tail", 20_000, &[(16, 31)]),
    ]);
    let mut params = RuntimeParams::default();
    params.mtp_enabled = true;
    let mut deploy_plan = super::plan(
        "qwen",
        &machines,
        Some(&[pick(0, 15, "head"), pick(16, 31, "tail")]),
        &params,
        false,
        DeployKind::Shards,
        None,
    );
    let blocked_before = deploy_plan.blocked.clone();
    let bytes_before = deploy_plan.transfer_bytes;
    let mut inventory = HashMap::new();
    inventory.insert(
        "head".to_string(),
        vec![sidecar(
            SidecarKind::Mtp,
            "draft.gguf",
            34,
            "/library/other-quant/draft.gguf",
        )],
    );
    inventory.insert(
        "tail".to_string(),
        vec![
            sidecar(
                SidecarKind::Mmproj,
                "mmproj.gguf",
                7,
                "/library/mmproj.gguf",
            ),
            // Wrong width: never offered.
            crate::catalog::Sidecar {
                dim: Some(1024),
                ..sidecar(SidecarKind::Mmproj, "other.gguf", 9, "/x/other.gguf")
            },
        ],
    );
    let shape = ModelShape {
        model_key: "qwen.gguf".into(),
        architecture: Some("llama".into()),
        embedding_length: Some(3072),
        embedded_mtp: false,
    };
    let links = vec![SidecarLink {
        model_key: "qwen.gguf".into(),
        kind: SidecarKind::Mtp,
        mode: SidecarMode::File,
        file_name: Some("draft.gguf".into()),
        size_bytes: Some(34),
        updated_at: String::new(),
        updated_by: String::new(),
    }];
    let vision = SidecarChoice {
        mode: Some(SidecarMode::File),
        file: Some(SidecarRef {
            file_name: "mmproj.gguf".into(),
            size_bytes: 7,
        }),
    };

    attach_sidecars(
        &mut deploy_plan,
        &machines,
        &inventory,
        Some(&shape),
        &links,
        false,
        true,
        Some(&vision),
        true,
        None,
    );

    assert_eq!(deploy_plan.blocked, blocked_before);
    assert_eq!(
        deploy_plan.sidecars.options.len(),
        2,
        "the 1024-wide projector is not offered"
    );
    let vision = deploy_plan.sidecars.vision.as_ref().unwrap();
    assert_eq!(vision.target_node_id, "head");
    assert!(!vision.from_link);
    assert!(vision.needs_copy());
    assert_eq!(vision.source_path.as_deref(), Some("/library/mmproj.gguf"));
    let mtp = deploy_plan.sidecars.mtp.as_ref().unwrap();
    assert_eq!(mtp.target_node_id, "tail");
    assert!(mtp.from_link);
    assert_eq!(mtp.source_node_id.as_deref(), Some("head"));
    assert_eq!(deploy_plan.transfer_bytes, bytes_before + 7 + 34);

    // A file nobody has refuses the plan and names it.
    let gone = SidecarChoice {
        mode: Some(SidecarMode::File),
        file: Some(SidecarRef {
            file_name: "missing.gguf".into(),
            size_bytes: 1,
        }),
    };
    let mut again = super::plan(
        "qwen",
        &machines,
        Some(&[pick(0, 15, "head"), pick(16, 31, "tail")]),
        &params,
        false,
        DeployKind::Shards,
        None,
    );
    attach_sidecars(
        &mut again,
        &machines,
        &inventory,
        Some(&shape),
        &[],
        false,
        false,
        None,
        true,
        Some(&gone),
    );
    assert!(again
        .blocked
        .as_deref()
        .is_some_and(|reason| reason.contains("missing.gguf")));
    assert!(
        again.sidecars.vision.is_none(),
        "vision off resolves no projector"
    );
}

#[test]
fn an_external_draft_makes_mtp_available_for_a_model_without_nextn() {
    let machines = build(vec![
        m("head", 20_000, &[(0, 15)]),
        m("tail", 20_000, &[(16, 31)]),
    ]);
    let mut deploy_plan = super::plan(
        "qwen",
        &machines,
        Some(&[pick(0, 15, "head"), pick(16, 31, "tail")]),
        &RuntimeParams::default(),
        false,
        DeployKind::Shards,
        None,
    );
    assert_eq!(
        deploy_plan.capabilities.mtp.state,
        crate::capabilities::CapabilityState::Unsupported
    );
    let mut inventory = HashMap::new();
    inventory.insert(
        "tail".to_string(),
        vec![sidecar(
            crate::catalog::SidecarKind::Mtp,
            "draft.gguf",
            34,
            "/library/draft.gguf",
        )],
    );
    let shape = ModelShape {
        model_key: "qwen.gguf".into(),
        architecture: Some("llama".into()),
        embedding_length: Some(3072),
        embedded_mtp: false,
    };
    attach_sidecars(
        &mut deploy_plan,
        &machines,
        &inventory,
        Some(&shape),
        &[],
        false,
        false,
        None,
        false,
        None,
    );
    assert_eq!(
        deploy_plan.capabilities.mtp.state,
        crate::capabilities::CapabilityState::Supported
    );
}
