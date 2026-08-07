//! Asymmetric layer planning and on-demand slicing.
//!
//! Peers are not equal — the MacBook can spare a few GB of unified memory while
//! the ksia has 16 GB of VRAM — so the model is *not* split into equal parts.
//! Each peer declares a memory budget, and the planner works out how many
//! layers of that specific GGUF actually fit in it, using the real quantised
//! size of every transformer block read from the tensor table (`gguf::read_layout`).
//!
//! Slicing itself stays where it already worked: `split_model.py`, which has
//! always taken an arbitrary `--start-layer`/`--end-layer`. Each peer produces
//! only its own slice from its own copy of the source model, so no weights ever
//! cross the network.

use crate::gguf::GgufLayout;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

const MB: f64 = 1_048_576.0;

/// Fraction of a peer's budget held back for the KV cache, activations and the
/// runtime itself, rather than model weights.
pub const DEFAULT_RESERVE_PCT: f64 = 15.0;

// ── Inputs ───────────────────────────────────────────────────────────────────

/// One peer's declared willingness to spend memory on this model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerBudget {
    pub node_id: String,
    /// Hostname or short id — what the UI shows.
    pub label: String,
    /// Memory this peer may use, in MB, as typed by the user.
    pub budget_mb: f64,
    /// Whether the source GGUF is present on that peer.
    #[serde(default)]
    pub has_model: bool,
}

// ── Outputs ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PlannedShard {
    pub node_id: String,
    pub label: String,
    pub budget_mb: f64,
    /// Budget minus the reserve — what weights may actually occupy.
    pub usable_mb: f64,
    pub start_layer: u32,
    pub end_layer: u32,
    pub layers: u32,
    /// Real size of the slice this peer would write to disk.
    pub weight_mb: f64,
    pub is_first: bool,
    pub is_last: bool,
    pub has_model: bool,
    /// Filename this peer's slice gets, e.g. `qwen_0_15.gguf`.
    pub shard_filename: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitPlan {
    pub model_path: String,
    pub base_name: String,
    pub total_layers: u32,
    pub model_weight_mb: f64,
    pub reserve_pct: f64,
    pub shards: Vec<PlannedShard>,
    pub covered_layers: u32,
    /// True when the shards cover every layer of the model.
    pub complete: bool,
    /// First uncovered layer range, when the budgets fall short.
    pub missing_range: Option<(u32, u32)>,
    /// Extra memory needed to close that gap.
    pub missing_mb: f64,
    /// Peers that lack the source model, budgets too small for a single layer, etc.
    pub warnings: Vec<String>,
}

// ── Planner ──────────────────────────────────────────────────────────────────

/// Turn per-peer memory budgets into per-peer layer ranges.
///
/// Peers are served in the order given: peer 0 takes layers from 0 upward until
/// its budget is spent, peer 1 continues from there, and so on. The last peer
/// also has to fit the output head, which `slice_bytes` accounts for.
pub fn plan_split(
    model_path: &str,
    layout: &GgufLayout,
    budgets: &[PeerBudget],
    reserve_pct: f64,
) -> Result<SplitPlan> {
    let total_layers = layout.layer_count();
    if total_layers == 0 {
        bail!("não foi possível ler as camadas do GGUF (tensor table vazia)");
    }
    if budgets.is_empty() {
        bail!("nenhum peer com orçamento informado");
    }

    let reserve = (reserve_pct / 100.0).clamp(0.0, 0.9);
    let base_name = base_name_of(model_path);
    let mut warnings: Vec<String> = Vec::new();
    let mut shards: Vec<PlannedShard> = Vec::new();
    let mut pos: u32 = 0;

    for (i, budget) in budgets.iter().enumerate() {
        let is_last_peer = i == budgets.len() - 1;
        let usable_mb = budget.budget_mb * (1.0 - reserve);

        if pos >= total_layers {
            warnings.push(format!(
                "{} ficou sem camadas: os peers anteriores já cobrem o modelo inteiro",
                budget.label
            ));
            continue;
        }

        // Grow the range while the slice still fits the budget. The last peer
        // additionally carries the output head, so it is priced differently.
        let mut end = pos;
        let mut taken = 0u32;
        while end < total_layers {
            let cost_mb = layout.slice_bytes(pos, end, is_last_peer) as f64 / MB;
            if cost_mb > usable_mb {
                // `taken == 0` here means not even one layer fits; reported below.
                break;
            }
            taken = end - pos + 1;
            end += 1;
        }

        if taken == 0 {
            let need = layout.slice_bytes(pos, pos, is_last_peer) as f64 / MB;
            warnings.push(format!(
                "{}: {:.0} MB não cobrem nem uma camada (mínimo ≈{:.0} MB, já descontada a reserva de {:.0}%)",
                budget.label, budget.budget_mb, need / (1.0 - reserve), reserve_pct
            ));
            continue;
        }

        let start_layer = pos;
        let end_layer = pos + taken - 1;
        let is_first = shards.is_empty();
        let is_last = is_last_peer || end_layer + 1 >= total_layers;

        if !budget.has_model {
            warnings.push(format!(
                "{} não tem o modelo escolhido instalado — precisa baixá-lo antes de fatiar",
                budget.label
            ));
        }

        shards.push(PlannedShard {
            node_id: budget.node_id.clone(),
            label: budget.label.clone(),
            budget_mb: budget.budget_mb,
            usable_mb,
            start_layer,
            end_layer,
            layers: taken,
            weight_mb: layout.slice_bytes(start_layer, end_layer, is_last) as f64 / MB,
            is_first,
            is_last,
            has_model: budget.has_model,
            shard_filename: shard_filename(&base_name, start_layer, end_layer),
        });

        pos = end_layer + 1;
    }

    let covered_layers = pos.min(total_layers);
    let complete = covered_layers >= total_layers;
    let (missing_range, missing_mb) = if complete {
        (None, 0.0)
    } else {
        let missing = layout.slice_bytes(covered_layers, total_layers - 1, true) as f64 / MB;
        (Some((covered_layers, total_layers - 1)), missing)
    };

    if !complete {
        warnings.push(format!(
            "faltam as camadas {}-{}: some ~{:.0} MB de orçamento (ou some outro peer) para fechar o modelo",
            covered_layers,
            total_layers - 1,
            missing_mb / (1.0 - reserve)
        ));
    }

    Ok(SplitPlan {
        model_path: model_path.to_string(),
        base_name,
        total_layers,
        model_weight_mb: layout.total_bytes as f64 / MB,
        reserve_pct,
        shards,
        covered_layers,
        complete,
        missing_range,
        missing_mb,
        warnings,
    })
}

/// Strip directory, `.gguf` and any existing `_<start>_<end>` suffix so
/// re-slicing an already-sliced name does not stack suffixes.
pub fn base_name_of(model_path: &str) -> String {
    let stem = Path::new(model_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".to_string());

    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() >= 3 {
        let last_two_numeric = parts[parts.len() - 1].parse::<u32>().is_ok()
            && parts[parts.len() - 2].parse::<u32>().is_ok();
        if last_two_numeric {
            return parts[..parts.len() - 2].join("_");
        }
    }
    stem
}

/// The filename convention `main.rs` and `catalog.rs` both parse.
pub fn shard_filename(base: &str, start: u32, end: u32) -> String {
    format!("{base}_{start}_{end}.gguf")
}

// ── Slicing ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SliceOutcome {
    pub output_path: String,
    pub bytes: u64,
    /// True when the slice already existed and was reused.
    pub reused: bool,
}

/// Produce this peer's slice with `split_model.py`, unless it is already there.
///
/// Runs entirely on the local machine against the local copy of the source
/// model — no weights are transferred between peers.
pub async fn extract_shard(
    cwd: &Path,
    python: &str,
    source_model: &str,
    output_dir: &Path,
    base_name: &str,
    start_layer: u32,
    end_layer: u32,
    is_first: bool,
    is_last: bool,
) -> Result<SliceOutcome> {
    let output = output_dir.join(shard_filename(base_name, start_layer, end_layer));

    // Reuse an existing slice: these files are gigabytes and rebuilding one is
    // minutes of I/O for an identical result.
    if let Ok(meta) = std::fs::metadata(&output) {
        if meta.len() > 1024 {
            info!("shard já existe, reaproveitando: {}", output.display());
            return Ok(SliceOutcome {
                output_path: output.to_string_lossy().into_owned(),
                bytes: meta.len(),
                reused: true,
            });
        }
    }

    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("criar {}", output_dir.display()))?;

    let script = cwd.join("split_model.py");
    if !script.exists() {
        bail!("split_model.py não encontrado em {}", cwd.display());
    }

    info!(
        "✂️  fatiando camadas {}-{} de {} → {}",
        start_layer,
        end_layer,
        source_model,
        output.display()
    );

    let mut cmd = Command::new(python);
    cmd.current_dir(cwd)
        .arg(&script)
        .arg("--model").arg(source_model)
        .arg("--start-layer").arg(start_layer.to_string())
        .arg("--end-layer").arg(end_layer.to_string())
        .arg("--output").arg(&output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if is_first {
        cmd.arg("--is-first");
    }
    if is_last {
        cmd.arg("--is-last");
    }

    let out = cmd.output().await.context("executar split_model.py")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.lines().rev().take(12).collect::<Vec<_>>().join("\n");
        // A partial file would be mistaken for a valid slice next time.
        let _ = std::fs::remove_file(&output);
        bail!("split_model.py falhou ({}): {tail}", out.status);
    }

    let bytes = std::fs::metadata(&output)
        .with_context(|| format!("split_model.py não gerou {}", output.display()))?
        .len();

    Ok(SliceOutcome {
        output_path: output.to_string_lossy().into_owned(),
        bytes,
        reused: false,
    })
}

/// Where slices go: `<cwd>/models/<base>/`.
pub fn shard_output_dir(cwd: &Path, base_name: &str) -> PathBuf {
    cwd.join("models").join(sanitize(base_name))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(n: u32, per_layer_mb: f64) -> GgufLayout {
        let per = (per_layer_mb * MB) as u64;
        GgufLayout {
            layer_bytes: vec![per; n as usize],
            embedding_bytes: (100.0 * MB) as u64,
            head_bytes: (100.0 * MB) as u64,
            other_bytes: 0,
            total_bytes: per * n as u64 + (200.0 * MB) as u64,
        }
    }

    fn budget(id: &str, mb: f64) -> PeerBudget {
        PeerBudget {
            node_id: id.into(),
            label: id.into(),
            budget_mb: mb,
            has_model: true,
        }
    }

    #[test]
    fn asymmetric_budgets_give_asymmetric_ranges() {
        // 32 layers of 200 MB, plus 100 MB embedding on every slice.
        let l = layout(32, 200.0);
        // 0% reserve keeps the arithmetic checkable by hand.
        let plan = plan_split("/m/qwen.gguf", &l, &[budget("a", 2100.0), budget("b", 5000.0)], 0.0)
            .unwrap();

        // a: 100 embd + 10*200 = 2100 → exactly 10 layers.
        assert_eq!(plan.shards[0].start_layer, 0);
        assert_eq!(plan.shards[0].layers, 10);
        // b picks up from 10 and is the last, so it also pays the 100 MB head.
        assert_eq!(plan.shards[1].start_layer, 10);
        assert!(plan.shards[1].layers > plan.shards[0].layers);
    }

    #[test]
    fn reports_the_gap_when_budgets_fall_short() {
        let l = layout(32, 200.0);
        let plan =
            plan_split("/m/qwen.gguf", &l, &[budget("a", 1100.0), budget("b", 1100.0)], 0.0)
                .unwrap();
        assert!(!plan.complete);
        // a (not last): 100 embd + 5*200 = 1100 → layers 0-4.
        // b (last) also carries the 100 MB head, so only 4 layers fit → 5-8.
        assert_eq!(plan.shards[0].layers, 5);
        assert_eq!(plan.shards[1].layers, 4);
        assert_eq!(plan.covered_layers, 9);
        assert_eq!(plan.missing_range, Some((9, 31)));
        assert!(plan.missing_mb > 0.0);
        assert!(plan.warnings.iter().any(|w| w.contains("faltam as camadas")));
    }

    #[test]
    fn warns_when_a_budget_cannot_hold_one_layer() {
        let l = layout(32, 200.0);
        let plan = plan_split("/m/qwen.gguf", &l, &[budget("tiny", 50.0), budget("b", 99000.0)], 0.0)
            .unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("nem uma camada")));
        // The remaining peer still covers everything.
        assert!(plan.complete);
    }

    #[test]
    fn warns_when_a_peer_lacks_the_model() {
        let l = layout(8, 100.0);
        let mut b = budget("b", 99000.0);
        b.has_model = false;
        let plan = plan_split("/m/qwen.gguf", &l, &[b], 0.0).unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("não tem o modelo")));
    }

    #[test]
    fn reserve_shrinks_the_usable_budget() {
        let l = layout(32, 200.0);
        let full = plan_split("/m/q.gguf", &l, &[budget("a", 4100.0)], 0.0).unwrap();
        let reserved = plan_split("/m/q.gguf", &l, &[budget("a", 4100.0)], 50.0).unwrap();
        assert!(reserved.shards[0].layers < full.shards[0].layers);
    }

    #[test]
    fn base_name_drops_an_existing_shard_suffix() {
        assert_eq!(base_name_of("/models/qwen/qwen_0_15.gguf"), "qwen");
        assert_eq!(
            base_name_of("/x/Qwen3.6-35B-A3B-Q4_K_M_20_39.gguf"),
            "Qwen3.6-35B-A3B-Q4_K_M"
        );
        assert_eq!(base_name_of("/x/phi-4-Q6_K.gguf"), "phi-4-Q6_K");
    }

    #[test]
    fn shard_filename_matches_the_parsed_convention() {
        assert_eq!(shard_filename("qwen", 0, 15), "qwen_0_15.gguf");
    }
}
