//! The slice this node was serving, kept so it survives a restart.
//!
//! The model assignment lives only in memory (a design decision: the deployment
//! plan is in charge, not a file forgotten on disk). A crash, however, is not
//! a change of intent - it is the same intent with a new process. So the
//! slice is recorded on disk and stays there until it is confirmed.
//!
//! **The annotation counts as resumable if and only if the slice was seen
//! loaded and serving.** That is why it carries a [`SliceState`]: the one that
//! writes `Pending` is `apply`, the one that promotes to `Serving` is the
//! inference server's `/health` answering `model_loaded`, and the one that
//! writes `Idle` is the idle monitor after releasing the weights.
//!
//! Confusing the two is an infinite loop, not a detail: a slice that does not
//! fit in memory kills the process before loading, Docker restarts the
//! container, and an optimistic annotation makes the node try the same slice
//! forever. A counter kept only in memory does not protect against that - it
//! dies with the process.
//! Reproduced in `data/pom-reinicio-infinito-carga/`.

use crate::runtime::NodeConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// At which point of the slice's life this annotation was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SliceState {
    /// Applied, not yet seen loaded even once.
    Pending,
    /// The watcher confirmed `model_loaded` on consecutive `/health` probes,
    /// after the minimum stability window.
    ///
    /// It is the default because a file written before 2026-08-15 does not
    /// have the field, and in that format only the restart route wrote it -
    /// on a node that was serving.
    #[default]
    Serving,
    /// The deployment is still active, but the weights were released by idle.
    Idle,
}

/// What is recorded on disk: the slice and how much is known about it.
#[derive(Serialize, Deserialize)]
struct Annotation {
    #[serde(default)]
    state: SliceState,
    /// Attempt of the deployment that has not yet reached `/health`.
    /// Old annotations without this field count as the first attempt.
    #[serde(default)]
    attempts: u32,
    #[serde(flatten)]
    slice: NodeConfig,
}

/// Persistent limit shared by the journal and the in-memory supervisor.
/// Ten is already the limit the supervisor uses for failures of one generation.
pub const MAX_AUTOMATIC_ATTEMPTS: u32 = 10;

/// What startup found recorded.
///
/// It is an enum, and not `Option<NodeConfig>`, because the defect it fixes
/// was precisely a slice being applied back without anyone asking whether it
/// ever served: here the config cannot be taken without deciding the case.
pub enum LastSlice {
    /// Nothing recorded.
    Nothing,
    /// Seen serving before the crash - the node comes back serving the same.
    Serving(NodeConfig),
    /// The node comes back announced and idle, without reloading before the first call.
    Idle(NodeConfig),
    /// Recorded before the deployment and not yet confirmed by `/health`.
    Pending { cfg: NodeConfig, attempts: u32 },
}

/// Where this node records its slice.
///
/// It derives from the config path because two nodes on the same machine run
/// with different `POM_CONFIG`; a fixed name would make one overwrite the other.
pub fn path() -> PathBuf {
    path_for(&crate::config::Config::config_path())
}

fn path_for(config_path: &Path) -> PathBuf {
    let mut s = config_path.as_os_str().to_os_string();
    s.push(".last-slice.json");
    PathBuf::from(s)
}

/// Records the slice, and at which point of its life.
pub fn save(cfg: &NodeConfig, state: SliceState) -> Result<()> {
    save_at(&path(), cfg, state)
}

pub(crate) fn save_at(p: &Path, cfg: &NodeConfig, state: SliceState) -> Result<()> {
    let annotation = Annotation {
        state,
        attempts: (state == SliceState::Pending).then_some(1).unwrap_or(0),
        slice: cfg.clone(),
    };
    write_annotation(p, &annotation)
}

fn write_annotation(p: &Path, annotation: &Annotation) -> Result<()> {
    let raw = serde_json::to_string_pretty(annotation)?;
    let tmp = p.with_file_name(format!(
        ".{}.tmp-{}",
        p.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("last-slice"),
        std::process::id()
    ));
    std::fs::write(&tmp, raw).with_context(|| format!("could not write {}", tmp.display()))?;
    // Windows does not replace an existing destination with rename. The normal
    // attempt is atomic; we only remove the destination if the platform requires it.
    if let Err(error) = std::fs::rename(&tmp, p) {
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(p);
            std::fs::rename(&tmp, p)
                .with_context(|| format!("could not replace {} after {error}", p.display()))?;
        }
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("could not replace {}", p.display()));
        }
    }
    Ok(())
}

/// Forgets the recorded slice.
///
/// Called when the operator removes the model on purpose (releasing the slice,
/// leaving the network, stopping to reconfigure). Without this, a node emptied
/// on request would come back serving on the next startup, undoing the
/// decision of whoever emptied it.
pub fn clear() {
    clear_at(&path());
}

fn clear_at(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(disable_path(p));
}

fn disable_path(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.disabled", p.display()))
}

/// Prevents any automatic resume until a new explicit deployment.
/// Also used when the main process is being relaunched from outside.
pub fn force_disable() -> Result<()> {
    force_disable_at(&path())
}

fn force_disable_at(p: &Path) -> Result<()> {
    let _ = std::fs::remove_file(p);
    std::fs::write(disable_path(p), "manual\n")
        .with_context(|| format!("could not create the lock {}", disable_path(p).display()))
}

pub fn auto_resume_disabled() -> bool {
    disable_path(&path()).exists()
}

/// Starts an explicit deployment and records the first attempt before any
/// call to the supervisor or the native server.
pub fn begin_new_attempt(cfg: &NodeConfig) -> Result<u32> {
    begin_new_attempt_at(&path(), cfg)
}

fn begin_new_attempt_at(p: &Path, cfg: &NodeConfig) -> Result<u32> {
    let _ = std::fs::remove_file(disable_path(p));
    let annotation = Annotation {
        state: SliceState::Pending,
        attempts: 1,
        slice: cfg.clone(),
    };
    write_annotation(p, &annotation)?;
    Ok(annotation.attempts)
}

/// Records the resume of a deployment that survived a restart.
pub fn resume_attempt(cfg: &NodeConfig) -> Result<u32> {
    resume_attempt_at(&path(), cfg)
}

fn resume_attempt_at(p: &Path, cfg: &NodeConfig) -> Result<u32> {
    let attempts = match read_annotation(p) {
        Some(annotation) if annotation.state == SliceState::Pending => {
            annotation.attempts.max(1).saturating_add(1)
        }
        _ => 1,
    };
    let annotation = Annotation {
        state: SliceState::Pending,
        attempts,
        slice: cfg.clone(),
    };
    write_annotation(p, &annotation)?;
    Ok(attempts)
}

/// Tells whether the attempt number is still within the automatic budget.
pub const fn should_resume(attempts: u32) -> bool {
    attempts <= MAX_AUTOMATIC_ATTEMPTS
}

/// Reads the recorded slice without consuming the journal.
pub fn inspect() -> LastSlice {
    inspect_at(&path())
}

fn inspect_at(p: &Path) -> LastSlice {
    let Some(annotation) = read_annotation(p) else {
        return LastSlice::Nothing;
    };
    match annotation.state {
        SliceState::Serving => LastSlice::Serving(annotation.slice),
        SliceState::Idle => LastSlice::Idle(annotation.slice),
        SliceState::Pending => LastSlice::Pending {
            cfg: annotation.slice,
            attempts: annotation.attempts.max(1),
        },
    }
}

fn read_annotation(p: &Path) -> Option<Annotation> {
    let Ok(raw) = std::fs::read_to_string(p) else {
        return None;
    };
    serde_json::from_str::<Annotation>(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two nodes on the same machine run with different POM_CONFIG. If the
    /// slice file had a fixed name, one would overwrite the other's and the
    /// machine would come back with the two slices swapped.
    #[test]
    fn the_path_derives_from_each_node_config() {
        assert_eq!(
            path_for(std::path::Path::new("/repo/pom.json")),
            std::path::PathBuf::from("/repo/pom.json.last-slice.json")
        );
        assert_eq!(
            path_for(std::path::Path::new("/repo/pom-cpu.json")),
            std::path::PathBuf::from("/repo/pom-cpu.json.last-slice.json")
        );
        assert_ne!(
            path_for(std::path::Path::new("/repo/pom.json")),
            path_for(std::path::Path::new("/repo/pom-cpu.json"))
        );
    }

    /// The read does not consume the journal: a crash between the read and the
    /// start of the attempt must remain countable on the next startup.
    #[test]
    fn inspect_preserves_the_journal() {
        let dir = tmp("inspect-preserva");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        assert!(
            matches!(inspect_at(&f), LastSlice::Serving(_)),
            "the read returns the slice"
        );
        assert!(f.exists(), "the read cannot erase the journal");
        assert!(
            matches!(inspect_at(&f), LastSlice::Serving(_)),
            "the next read still returns the same slice"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The guarantee of `c7b70d8`: a slice that was serving healthy and
    /// crashed comes back serving on its own.
    ///
    /// A segfault runs no code at all: on 2026-08-13 ksia-gpu3 crashed with
    /// SIGSEGV inside the CUDA kernel of gated_delta_net, Docker restarted
    /// the container through the `unless-stopped` policy, and the node came
    /// back without a slice - silently vanishing from the pipeline until
    /// someone noticed a "Gap at layer 32" in a request, minutes later. That
    /// is why `/health` is the one that records it, and not a graceful
    /// shutdown path that a crash like this skips.
    #[test]
    fn a_slice_seen_serving_comes_back_after_a_crash() {
        let dir = tmp("servindo-volta");
        let f = dir.join("cfg.json.last-slice.json");

        save_at(&f, &sample(), SliceState::Pending).unwrap(); // apply
        save_at(&f, &sample(), SliceState::Serving).unwrap(); // /health: model_loaded

        match inspect_at(&f) {
            LastSlice::Serving(cfg) => assert_eq!(cfg.model_label, "qwen35"),
            _ => panic!("a slice seen serving must come back serving"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_idle_slice_comes_back_announced_without_loading() {
        let dir = tmp("ociosa-volta");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Idle).unwrap();

        assert!(matches!(inspect_at(&f), LastSlice::Idle(cfg) if cfg.model_label == "qwen35"));
        assert!(f.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stopping is the operator's decision, and the next startup does not undo it.
    #[test]
    fn stopping_on_purpose_forgets_the_slice() {
        let dir = tmp("parar-esquece");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        clear_at(&f);

        assert!(!f.exists(), "stopping on purpose forgets");
        assert!(
            matches!(inspect_at(&f), LastSlice::Nothing),
            "and the next startup does not resurrect it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression of the infinite restart loop (2026-08-15).
    ///
    /// A slice that does not fit in memory kills the process before loading;
    /// Docker restarts the container; if the annotation written at `apply`
    /// counted as "was serving", the node would try the same slice forever.
    /// The `MAX_RESTARTS` counter does not hold that back: it lives in memory
    /// and dies together with the process.
    #[test]
    fn a_slice_that_never_loaded_is_not_resurrected() {
        let dir = tmp("nunca-carregou");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Pending).unwrap();

        match inspect_at(&f) {
            LastSlice::Pending { cfg, attempts } => {
                assert_eq!(cfg.model_label, "qwen35");
                assert_eq!(attempts, 1);
            }
            LastSlice::Serving(_) => panic!("a slice that never loaded cannot be resumed"),
            LastSlice::Idle(_) => panic!("a pending slice cannot be idle"),
            LastSlice::Nothing => {
                panic!("the reason of the failure was lost: the operator is left not knowing")
            }
        }
        assert!(f.exists(), "the journal must survive the next startup");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An `apply` overrides the annotation of a previous slice that was
    /// serving: nothing is known yet about the new one, and it is the one the
    /// node serves now.
    #[test]
    fn applying_another_slice_drops_the_serving_state_of_the_previous_one() {
        let dir = tmp("apply-sobrepoe");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        let mut new = sample();
        new.start_layer = 8;
        new.end_layer = 15;
        save_at(&f, &new, SliceState::Pending).unwrap();

        match inspect_at(&f) {
            LastSlice::Pending { cfg, attempts } => {
                assert_eq!(cfg.start_layer, 8);
                assert_eq!(attempts, 1);
            }
            _ => panic!("the new slice never loaded; the old one is no longer served"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An annotation written by a binary older than this change does not have
    /// the `state` field. Only the restart route wrote it, and on a node that
    /// was serving - so it counts as serving, otherwise "Update and
    /// restart" itself would lose the slice when crossing versions.
    #[test]
    fn an_old_annotation_without_state_counts_as_serving() {
        let dir = tmp("formato-antigo");
        let f = dir.join("cfg.json.last-slice.json");
        std::fs::write(&f, serde_json::to_string(&sample()).unwrap()).unwrap();

        assert!(
            matches!(inspect_at(&f), LastSlice::Serving(_)),
            "the old format is a slice that was up"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restarts_of_a_pending_slice_are_counted_and_blocked_at_the_limit() {
        let dir = tmp("tentativas-persistentes");
        let f = dir.join("cfg.json.last-slice.json");
        let cfg = sample();

        assert_eq!(begin_new_attempt_at(&f, &cfg).unwrap(), 1);
        for expected in 2..=MAX_AUTOMATIC_ATTEMPTS {
            assert_eq!(resume_attempt_at(&f, &cfg).unwrap(), expected);
        }

        assert!(should_resume(MAX_AUTOMATIC_ATTEMPTS));
        assert!(!should_resume(MAX_AUTOMATIC_ATTEMPTS + 1));
        assert!(
            matches!(inspect_at(&f), LastSlice::Pending { attempts, .. } if attempts == MAX_AUTOMATIC_ATTEMPTS)
        );
        assert!(f.exists(), "the block must survive the next startup");

        clear_at(&f);
        assert!(matches!(inspect_at(&f), LastSlice::Nothing));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_health_success_resets_the_count_and_allows_a_new_resume() {
        let dir = tmp("tentativa-sucesso");
        let f = dir.join("cfg.json.last-slice.json");
        let cfg = sample();

        begin_new_attempt_at(&f, &cfg).unwrap();
        resume_attempt_at(&f, &cfg).unwrap();
        save_at(&f, &cfg, SliceState::Serving).unwrap();

        assert!(matches!(inspect_at(&f), LastSlice::Serving(_)));
        assert_eq!(begin_new_attempt_at(&f, &cfg).unwrap(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_manual_block_prevents_resume_until_a_new_deployment() {
        let dir = tmp("bloqueio-manual");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Pending).unwrap();

        force_disable_at(&f).unwrap();
        assert!(!f.exists());
        assert!(disable_path(&f).exists());

        begin_new_attempt_at(&f, &sample()).unwrap();
        assert!(f.exists());
        assert!(!disable_path(&f).exists());

        clear_at(&f);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pom-lastslice-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> NodeConfig {
        crate::runtime::NodeConfig {
            deploy_id: String::new(),
            model_path: "/models/m_0_7.gguf".into(),
            mmproj_path: None,
            mtp_draft_path: None,
            model_label: "qwen35".into(),
            deployment_name: "qwen35".into(),
            start_layer: 0,
            end_layer: 7,
            total_layers: 40,
            peer_count: 5,
            peer_index: 0,
            primary: false,
            inference_mode: Default::default(),
            inference_service: None,
            engine_settings: Default::default(),
            inference_role: Default::default(),
            prefill_min_tokens: None,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            slot_save_path: None,
            cache_ram_mib: None,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_ubatch: llama_engine::DEFAULT_N_UBATCH,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: 512,
            prefill_max_inflight_chunks: 2,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            min_p: 0.0,
            seed: llama_engine::llm::DEFAULT_SAMPLER_SEED,
            dry_multiplier: 0.0,
            dry_base: 1.75,
            dry_allowed_length: 2,
            dry_penalty_last_n: 1024,
            reasoning_budget: -1,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            speculative_k: 4,
            speculative_lookup_window: 3,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            mtp_cpu_fallback_enabled: true,
            mtp_placement: Default::default(),
            moe_experts_in_ram: false,
            moe_cache_mib: 0,
            adaptive_prefill_chunks: true,
            unload_if_idle: false,
            idle_minutes: 60,
            tp: None,
        }
    }
}
