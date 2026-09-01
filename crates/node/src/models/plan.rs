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

use crate::config::KvCacheType;
use crate::gguf::{GgufInfo, GgufLayout};
use crate::kv_budget::kv_bytes_for_layers;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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

/// Parâmetros do cache usados para transformar a geometria GGUF em bytes.
#[derive(Debug, Clone, Copy)]
pub struct KvPlanParams {
    pub n_ctx: u32,
    pub max_sessions: u32,
    pub cache_type_k: KvCacheType,
    pub cache_type_v: KvCacheType,
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
    pub weight_bytes: u64,
    pub kv_bytes: u64,
    pub total_bytes: u64,
    pub is_first: bool,
    pub is_last: bool,
    pub has_model: bool,
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

/// Variante do planejador que recusa uma faixa quando peso + KV não cabem no
/// orçamento de memória do peer.
pub fn plan_split_with_kv(
    model_path: &str,
    layout: &GgufLayout,
    info: &GgufInfo,
    budgets: &[PeerBudget],
    reserve_pct: f64,
    kv: KvPlanParams,
) -> Result<SplitPlan> {
    plan_split_impl(
        model_path,
        layout,
        Some(info),
        budgets,
        reserve_pct,
        Some(kv),
    )
}

fn plan_split_impl(
    model_path: &str,
    layout: &GgufLayout,
    info: Option<&GgufInfo>,
    budgets: &[PeerBudget],
    reserve_pct: f64,
    kv: Option<KvPlanParams>,
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
        let usable_bytes = usable_memory_bytes(budget.budget_mb, reserve)?;
        let usable_mb = usable_bytes as f64 / MB;

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
            let (_, _, total_bytes) = range_cost(layout, info, kv, pos, end, is_last_peer)?;
            if total_bytes > usable_bytes {
                // `taken == 0` here means not even one layer fits; reported below.
                break;
            }
            taken = end - pos + 1;
            end += 1;
        }

        if taken == 0 {
            let (weight_bytes, kv_bytes, total_bytes) =
                range_cost(layout, info, kv, pos, pos, is_last_peer)?;
            warnings.push(format!(
                "{}: {:.0} MB não cobrem nem uma camada (peso {} B + KV {} B = {} B, já descontada a reserva de {:.0}%)",
                budget.label, budget.budget_mb, weight_bytes, kv_bytes, total_bytes, reserve_pct
            ));
            continue;
        }

        let start_layer = pos;
        let mut end_layer = pos + taken - 1;
        let is_first = shards.is_empty();

        // Snap the cut so the next slice starts on a valid layer kind. Hybrid
        // models fail with "missing tensor" when a slice starts mid-group.
        end_layer = layout.snap_end_layer(start_layer, end_layer, total_layers);
        let taken = end_layer - start_layer + 1;
        let is_last = is_last_peer || end_layer + 1 >= total_layers;
        let (weight_bytes, kv_bytes, total_bytes) =
            range_cost(layout, info, kv, start_layer, end_layer, is_last)?;

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
            weight_mb: weight_bytes as f64 / MB,
            weight_bytes,
            kv_bytes,
            total_bytes,
            is_first,
            is_last,
            has_model: budget.has_model,
        });

        pos = end_layer + 1;
    }

    let covered_layers = pos.min(total_layers);
    let complete = covered_layers >= total_layers;
    let (missing_range, missing_mb) = if complete {
        (None, 0.0)
    } else {
        let (_, _, missing) = range_cost(layout, info, kv, covered_layers, total_layers - 1, true)?;
        (
            Some((covered_layers, total_layers - 1)),
            missing as f64 / MB,
        )
    };

    if !complete {
        warnings.push(format!(
            "faltam as camadas {}-{}: some ~{:.0} MB de orçamento (ou some outro peer) para fechar o modelo",
            covered_layers,
            total_layers - 1,
            missing_mb / (1.0 - reserve)
        ));
    }

    let ranges = shards
        .iter()
        .map(|shard| (shard.start_layer, shard.end_layer))
        .collect::<Vec<_>>();
    layout.validate_slice_ranges(&ranges)?;

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

fn usable_memory_bytes(budget_mb: f64, reserve: f64) -> Result<u64> {
    if !budget_mb.is_finite() || budget_mb < 0.0 {
        bail!("orçamento de memória inválido: {budget_mb}");
    }
    let bytes = budget_mb * (1.0 - reserve) * MB;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        bail!("orçamento de memória excede a aritmética de bytes");
    }
    Ok(bytes.floor() as u64)
}

/// `pub(crate)`: também usado pela pré-visualização de capacidade do
/// planejador de implantação (`crate::deploy::capacity_preview_endpoint`),
/// que cobra peso e KV de uma faixa exatamente como o planejador de
/// fatiamento faz aqui.
pub(crate) fn range_cost(
    layout: &GgufLayout,
    info: Option<&GgufInfo>,
    kv: Option<KvPlanParams>,
    start_layer: u32,
    end_layer: u32,
    is_last: bool,
) -> Result<(u64, u64, u64)> {
    let weight_bytes = layout.slice_bytes(start_layer, end_layer, is_last);
    let kv_bytes = match (info, kv) {
        (Some(info), Some(kv)) => {
            let geometry = info.kv_geometry_for_range(start_layer, end_layer)?;
            kv_bytes_for_layers(
                kv.n_ctx as u64,
                kv.max_sessions as u64,
                &geometry,
                kv.cache_type_k,
                kv.cache_type_v,
            )?
        }
        (None, None) => 0,
        _ => bail!("configuração de orçamento KV incompleta"),
    };
    let total_bytes = weight_bytes
        .checked_add(kv_bytes)
        .ok_or_else(|| anyhow::anyhow!("overflow ao somar peso e KV da fatia"))?;
    Ok((weight_bytes, kv_bytes, total_bytes))
}

/// O nome do modelo tirado do caminho do arquivo, sem diretório nem `.gguf`.
///
/// Já teve uma segunda função - descascar um sufixo `_<inicio>_<fim>` para que
/// refatiar uma fatia não empilhasse faixas no nome. Isso morreu junto com a
/// convenção: a faixa de uma parte mora no `.pom`, e o arquivo se chama pelo
/// próprio hash.
pub fn base_name_of(model_path: &str) -> String {
    Path::new(model_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".to_string())
}

// ── Slicing ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SliceOutcome {
    pub output_path: String,
    pub bytes: u64,
    /// sha256 do arquivo — o nome dele em disco e a identidade dele no `.pom`.
    pub sha256: String,
    /// True when the slice already existed and was reused.
    pub reused: bool,
}

/// Grava a fatia num temporário e só a batiza depois de saber o hash.
///
/// O nome final é o conteúdo, e o conteúdo só se conhece no fim: por isso o
/// corte sai para `<uuid>.part` e vira `<sha256>.gguf` num `rename`, que é
/// atômico no mesmo sistema de arquivos. É também o que impede um corte
/// interrompido de ficar parecendo fatia boa - o defeito que o critério antigo
/// ("existe e tem mais de 1 KB") não conseguia enxergar.
async fn write_then_name<F>(output_dir: &Path, corte: F) -> Result<SliceOutcome>
where
    F: FnOnce(PathBuf) -> tokio::task::JoinHandle<Result<crate::splitter::WrittenSlice>>,
{
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("criar {}", output_dir.display()))?;

    let temp = output_dir.join(format!("{}.part", uuid::Uuid::new_v4()));
    let escrito = corte(temp.clone())
        .await
        .context("spawn_blocking para o fatiador")?
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })?;

    let destino = crate::pom::part_path(output_dir, &escrito.sha256)?;
    // Já existir é o caso bom: outra rodada produziu bytes idênticos, e o
    // endereço de conteúdo diz que um dos dois arquivos é redundante.
    if destino.exists() {
        let _ = std::fs::remove_file(&temp);
        info!(
            "parte idêntica já em disco, reaproveitando: {}",
            destino.display()
        );
    } else if let Err(e) = std::fs::rename(&temp, &destino) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("renomear para {}", destino.display()));
    }

    Ok(SliceOutcome {
        output_path: destino.to_string_lossy().into_owned(),
        bytes: escrito.bytes,
        sha256: escrito.sha256,
        reused: false,
    })
}

/// A parte já está aqui, com este hash exato?
///
/// Reaproveitar passa a exigir saber **qual** parte se quer, e não adivinhar um
/// nome: é o `.pom` que diz o hash. Em troca some a classe inteira de defeito
/// em que um arquivo com o nome certo e o conteúdo errado era carregado como se
/// estivesse bom.
fn ja_tenho(output_dir: &Path, sha256: &str) -> Option<SliceOutcome> {
    let caminho = crate::pom::part_path(output_dir, sha256).ok()?;
    let meta = std::fs::metadata(&caminho).ok()?;
    if meta.len() <= 1024 {
        return None;
    }
    info!("parte já existe, reaproveitando: {}", caminho.display());
    Some(SliceOutcome {
        output_path: caminho.to_string_lossy().into_owned(),
        bytes: meta.len(),
        sha256: sha256.to_string(),
        reused: true,
    })
}

/// Produce this peer's slice, unless it already exists.
///
/// Runs entirely on the local machine against the local copy of the source
/// model — no weights are transferred between peers.
///
/// `on_progress` is called with 0..=100 as the slicer writes bytes.
///
/// `expected_sha256` é o que o `.pom` diz que esta parte tem que ser. Com ele,
/// reaproveitar é uma checagem de endereço de conteúdo; sem ele (uma rodada
/// nova, que ainda não conhece o hash), corta-se e o hash sai do corte.
#[allow(clippy::too_many_arguments)]
pub async fn extract_shard(
    source_model: &str,
    output_dir: &Path,
    expected_sha256: Option<&str>,
    start_layer: u32,
    end_layer: u32,
    is_first: bool,
    is_last: bool,
    on_progress: impl Fn(f64) + Send + Sync + 'static,
) -> Result<SliceOutcome> {
    let src = std::path::PathBuf::from(source_model);
    // A manifest may point at a stale file already present in the content
    // addressed directory. Validate that candidate, or the source when there
    // is no candidate yet, before reusing or writing it. Otherwise an old
    // invalid DeepSeek4 slice would bypass the new contract forever.
    let existing_path = expected_sha256
        .and_then(|sha| crate::pom::part_path(output_dir, sha).ok())
        .filter(|path| path.is_file());
    let preflight_path = existing_path.or_else(|| src.is_file().then_some(src.clone()));
    if expected_sha256.is_some() {
        if let Some(preflight_path) = preflight_path {
            let preflight_src = preflight_path;
            tokio::task::spawn_blocking(move || {
                crate::splitter::validate_slice_source(
                    &preflight_src,
                    start_layer,
                    end_layer,
                    is_last,
                )
            })
            .await
            .context("preflight do GGUF antes de reaproveitar a fatia")??;
        }
    }

    // Reuse an existing slice: these files are gigabytes and rebuilding one is
    // minutes of I/O for an identical result.
    if let Some(pronta) = expected_sha256.and_then(|sha| ja_tenho(output_dir, sha)) {
        return Ok(pronta);
    }

    write_then_name(output_dir, move |dst| {
        tokio::task::spawn_blocking(move || {
            crate::splitter::split(
                &src,
                &dst,
                start_layer,
                end_layer,
                is_first,
                is_last,
                on_progress,
            )
        })
    })
    .await
}

/// Extrai o shard de tensor de um rank de tensor parallelism (caminho C).
///
/// Espelho de `extract_shard`: roda `splitter::shard_tensor_parallel` num
/// `spawn_blocking`, reaproveita arquivo já completo e apaga o parcial no
/// erro. O shard NÃO é carregável pelo llama.cpp de hoje; é a metade que o
/// fork do caminho C carrega junto com a coletiva all-reduce.
///
/// O carimbo `pom.tp_format` continua sendo gravado, mas deixou de ser o que
/// decide reaproveitar: um shard escrito por uma regra antiga tem **outros
/// bytes**, logo outro hash, logo outro nome. O defeito de 17/08 - a ksia
/// carregando o mesmo shard obsoleto durante três correções seguidas, com o log
/// anunciando "reaproveitando" a cada tentativa - não tem como se repetir
/// quando o nome do arquivo é o conteúdo dele.
pub async fn extract_tp_shard(
    source_model: &str,
    output_dir: &Path,
    expected_sha256: Option<&str>,
    tp_size: u32,
    tp_rank: u32,
    on_progress: impl Fn(f64) + Send + Sync + 'static,
) -> Result<SliceOutcome> {
    if let Some(pronta) = expected_sha256.and_then(|sha| ja_tenho(output_dir, sha)) {
        return Ok(pronta);
    }

    let src = std::path::PathBuf::from(source_model);
    write_then_name(output_dir, move |dst| {
        tokio::task::spawn_blocking(move || {
            crate::splitter::shard_tensor_parallel(&src, &dst, tp_size, tp_rank, on_progress)
        })
    })
    .await
}

// ── Fatias de tamanhos diferentes ────────────────────────────────────────────
//
// As máquinas do cluster não são parecidas, então dividir em partes iguais é a
// resposta errada quase sempre: quem manda no tamanho de uma fatia é a memória
// da máquina que vai recebê-la. Aqui o pedido é em bytes e a resposta é em
// camadas, porque camada é a unidade que `split_model.py` conhece.

/// One slice the operator asked for.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SizedSlice {
    /// Target size in MB. `None` means "whatever is left of the model".
    #[serde(default)]
    pub target_mb: Option<f64>,
}

impl SizedSlice {
    fn of_mb(mb: f64) -> Self {
        Self {
            target_mb: Some(mb),
        }
    }

    /// "O resto do modelo" — a conveniência que evita erro de arredondamento na
    /// última fatia.
    fn rest() -> Self {
        Self { target_mb: None }
    }
}

/// What a requested size turned into once rounded to whole layers.
#[derive(Debug, Clone, Serialize)]
pub struct SizedRange {
    pub index: u32,
    pub start_layer: u32,
    pub end_layer: u32,
    pub layers: u32,
    /// Real size of the slice, summed from the tensor table.
    pub bytes: u64,
    /// What was asked for, when it was not "the rest".
    pub target_mb: Option<f64>,
    pub is_rest: bool,
    pub is_last: bool,
    /// The target could not hold even the first layer of this range.
    pub too_small: bool,
    /// Nothing was left of the model for this slice.
    pub empty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SizePlan {
    pub total_layers: u32,
    /// Sum of the exact serialized files represented by `ranges`.
    pub model_bytes: u64,
    /// Size of the source GGUF payload before slice-only duplication.
    pub source_model_bytes: u64,
    /// Real head payload repeated by every non-final slice, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_head_bytes: Option<u64>,
    pub ranges: Vec<SizedRange>,
    pub covered_layers: u32,
    pub complete: bool,
    /// Layers no slice covers, and what they would weigh.
    pub missing_range: Option<(u32, u32)>,
    pub missing_bytes: u64,
    /// True when a slice is unusable: too small for a layer, or left with none.
    pub blocked: bool,
    pub warnings: Vec<String>,
    /// Filled by the web handler with the actual configured slice destination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_margin_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_status: Option<&'static str>,
}

/// Turn a list of target sizes into contiguous layer ranges.
///
/// Rounding is always up to a whole layer and always **visible**: `SizedRange`
/// carries both what was asked and what it became, because "5 GB" turning into
/// 4,8 GB of layers 0–11 is the thing the operator has to be able to check.
///
/// Sizes come from the real tensor table (`gguf::read_layout`), not from
/// `total / n_layers`: the embedding travels in every slice and the blocks
/// themselves are not all the same size.
pub fn plan_by_size(layout: &GgufLayout, slices: &[SizedSlice]) -> Result<SizePlan> {
    let total_layers = layout.layer_count();
    if total_layers == 0 {
        bail!("não foi possível ler as camadas do GGUF (tensor table vazia)");
    }
    if slices.is_empty() {
        bail!("nenhuma fatia na lista");
    }

    let mut ranges: Vec<SizedRange> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut pos: u32 = 0;

    for (i, slice) in slices.iter().enumerate() {
        let index = i as u32;
        let last_requested = i == slices.len() - 1;

        if pos >= total_layers {
            warnings.push(format!(
                "fatia {}: não sobrou camada nenhuma — as anteriores já cobrem o modelo inteiro",
                index + 1
            ));
            ranges.push(SizedRange {
                index,
                start_layer: total_layers,
                end_layer: total_layers,
                layers: 0,
                bytes: 0,
                target_mb: if last_requested {
                    None
                } else {
                    slice.target_mb
                },
                is_rest: slice.target_mb.is_none() || last_requested,
                is_last: false,
                too_small: false,
                empty: true,
            });
            continue;
        }

        // "O resto" takes everything left. The final entry is always the rest,
        // even when an older client still sends a numeric target for it.
        let take_rest = slice.target_mb.is_none() || last_requested;
        let (end, too_small) = if take_rest {
            (total_layers - 1, false)
        } else {
            let budget = (slice.target_mb.unwrap_or(0.0).max(0.0) * MB) as u64;
            let mut end = pos;
            let mut fits_one = false;
            while end < total_layers {
                let is_last = end + 1 >= total_layers;
                if layout.slice_bytes(pos, end, is_last) > budget {
                    break;
                }
                fits_one = true;
                end += 1;
            }
            if fits_one {
                (end - 1, false)
            } else {
                // Uma camada é o mínimo indivisível: entregamos uma e dizemos
                // que o alvo não cabia, em vez de inventar meia camada.
                (pos, true)
            }
        };

        let end = if take_rest {
            end
        } else {
            layout.snap_end_layer(pos, end, total_layers)
        };
        let end = end.min(total_layers - 1);
        let is_last = end + 1 >= total_layers;
        let bytes = layout.slice_bytes(pos, end, is_last);

        if too_small {
            warnings.push(format!(
                "fatia {}: {} não cobrem nem a camada {}, que sozinha já pesa {}",
                index + 1,
                fmt_gb(slice.target_mb.unwrap_or(0.0) * MB),
                pos,
                fmt_gb(bytes as f64)
            ));
        }

        ranges.push(SizedRange {
            index,
            start_layer: pos,
            end_layer: end,
            layers: end - pos + 1,
            bytes,
            target_mb: if take_rest { None } else { slice.target_mb },
            is_rest: take_rest,
            is_last,
            too_small,
            empty: false,
        });

        pos = end + 1;
        if take_rest && !last_requested {
            // "O resto" no meio da lista engole tudo; as seguintes ficam vazias
            // e o aviso acima explica por quê.
            continue;
        }
    }

    let covered_layers = pos.min(total_layers);
    let complete = covered_layers >= total_layers;
    let (missing_range, missing_bytes) = if complete {
        (None, 0)
    } else {
        (
            Some((covered_layers, total_layers - 1)),
            layout.slice_bytes(covered_layers, total_layers - 1, true),
        )
    };

    // A cobertura não vira aviso: ela já está aqui em número (`covered_layers`,
    // `missing_range`, `missing_bytes`), e quem mostra escreve uma frase só. Dois
    // textos dizendo a mesma falta é ruído.
    let blocked = ranges.iter().any(|r| r.empty || r.too_small);

    // NextN/MTP (Qwen3.5+): the predictor block lives only in the model's very
    // last layer. Only a slice that ends there carries it (`splitter` keeps
    // `nextn_predict_layers` only on those); every other slice runs as a plain
    // trunk. Slicing is the moment the operator is most likely to bump into
    // that: a mid-model cut loads fine, but a "complete" model it is not.
    if layout.nextn_predict_layers > 0 {
        let nonempty = ranges.iter().filter(|r| !r.empty).count();
        if complete {
            if nonempty > 1 {
                warnings.push(format!(
                    "modelo com predictor nextn (multi-token): o bloco de predição vive só na última camada ({}) e vai embutido na fatia final - as anteriores carregam como tronco puro",
                    total_layers - 1
                ));
            }
        } else {
            warnings.push(format!(
                "modelo com predictor nextn (multi-token): o bloco de predição fica na camada {}, que esta divisão não cobre - a fatia final carrega como tronco puro, sem predição multi-token",
                total_layers - 1
            ));
        }
    }

    let planned_bytes = ranges
        .iter()
        .filter(|range| !range.empty)
        .map(|range| range.bytes)
        .sum();

    let validation_ranges = ranges
        .iter()
        .filter(|range| !range.empty)
        .map(|range| (range.start_layer, range.end_layer))
        .collect::<Vec<_>>();
    layout.validate_slice_ranges(&validation_ranges)?;

    let required_head_bytes = ranges
        .iter()
        .any(|range| !range.empty && !range.is_last)
        .then_some(layout.required_head_bytes())
        .filter(|bytes| *bytes > 0);

    Ok(SizePlan {
        total_layers,
        model_bytes: planned_bytes,
        source_model_bytes: layout.source_file_bytes.max(layout.total_bytes),
        required_head_bytes,
        ranges,
        covered_layers,
        complete,
        missing_range,
        missing_bytes,
        blocked,
        warnings,
        destination_path: None,
        destination_free_bytes: None,
        space_margin_bytes: None,
        space_status: None,
    })
}

/// The "cut it in N" shortcut, as a list the operator can still edit.
///
/// Equal here means equal in *layers*, and the target of each entry is the real
/// size that range costs — so filling the list with 3 and then raising one of
/// them starts from an honest number rather than from `total / 3`.
pub fn plan_equal(layout: &GgufLayout, parts: u32) -> Result<SizePlan> {
    let ranges = even_split(layout.layer_count(), parts);
    if ranges.is_empty() {
        bail!("este arquivo não declara camadas — não há o que dividir");
    }
    let slices: Vec<SizedSlice> = ranges
        .iter()
        .enumerate()
        .map(|(i, (start, end))| {
            let is_last = i == ranges.len() - 1;
            if is_last {
                SizedSlice::rest()
            } else {
                SizedSlice::of_mb(layout.slice_bytes(*start, *end, false) as f64 / MB)
            }
        })
        .collect();
    plan_by_size(layout, &slices)
}

fn fmt_gb(bytes: f64) -> String {
    let gb = bytes / 1_073_741_824.0;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

/// Cut `total_layers` into `parts` contiguous ranges as evenly as they divide.
///
/// The remainder goes to the first slices, one layer each, so the difference
/// between the biggest and the smallest is never more than one layer. This is
/// the "split it in N" path the operator asks for by number — the asymmetric
/// planner above is the one that sizes ranges by memory.
pub fn even_split(total_layers: u32, parts: u32) -> Vec<(u32, u32)> {
    if total_layers == 0 || parts == 0 {
        return Vec::new();
    }
    let parts = parts.min(total_layers);
    let base = total_layers / parts;
    let extra = total_layers % parts;

    let mut ranges = Vec::with_capacity(parts as usize);
    let mut start = 0u32;
    for i in 0..parts {
        let len = base + if i < extra { 1 } else { 0 };
        ranges.push((start, start + len - 1));
        start += len;
    }
    ranges
}

/// Turn a catalog name such as `qwen3:8b` into the name of the model's folder.
///
/// Ollama keeps its weights as `sha256-<digest>` blobs, so naming a slicing run
/// after the path produces `sha256-a3de…686f` — a name nobody can read and
/// nothing can match across machines. The friendly name is right there in the
/// catalog; this makes it usable as a directory.
pub fn base_name_from_label(label: &str) -> Option<String> {
    let label = label.trim();
    let label = label
        .strip_suffix(".gguf")
        .or_else(|| label.strip_suffix(".GGUF"))
        .unwrap_or(label);
    if label.trim_matches(['-', '.', '_', '/']).is_empty() {
        return None;
    }
    Some(crate::pom::sanitize_name(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_split(
        model_path: &str,
        layout: &GgufLayout,
        budgets: &[PeerBudget],
        reserve_pct: f64,
    ) -> Result<SplitPlan> {
        plan_split_impl(model_path, layout, None, budgets, reserve_pct, None)
    }

    fn layout(n: u32, per_layer_mb: f64) -> GgufLayout {
        let per = (per_layer_mb * MB) as u64;
        GgufLayout {
            layer_bytes: vec![per; n as usize],
            embedding_bytes: (100.0 * MB) as u64,
            head_bytes: (100.0 * MB) as u64,
            other_bytes: 0,
            total_bytes: per * n as u64 + (200.0 * MB) as u64,
            source_file_bytes: 0,
            // Homogeneous stack: period 1, so cuts are unconstrained.
            layer_kinds: vec![vec!["attn.weight".to_string()]; n as usize],
            nextn_predict_layers: 0,
            has_nextn_tensors: false,
            serialized: None,
        }
    }

    /// A hybrid stack: every 4th layer is attention, the rest are recurrent.
    fn hybrid_layout(n: u32, per_layer_mb: f64) -> GgufLayout {
        let mut l = layout(n, per_layer_mb);
        l.layer_kinds = (0..n as usize)
            .map(|i| {
                if (i + 1) % 4 == 0 {
                    vec!["attn_q.weight".to_string()]
                } else {
                    vec!["ssm_conv1d.weight".to_string()]
                }
            })
            .collect();
        l
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
    fn hybrid_layout_reports_arch_boundaries() {
        // Homogeneous: no boundaries, snap_end_layer is a no-op.
        assert!(layout(32, 200.0).arch_boundaries().is_empty());
        assert_eq!(layout(32, 200.0).snap_end_layer(0, 11, 32), 11);
        // Every 4th layer is attention (3 recurrent + 1 attention repeating).
        // Kind changes at 3, 4, 7, 8, 11, 12, ...
        let b = hybrid_layout(32, 200.0).arch_boundaries();
        assert!(b.contains(&3));
        assert!(b.contains(&4));
        assert!(!b.contains(&0));
        // snap_end_layer walks back to the last valid cut.
        let h = hybrid_layout(32, 200.0);
        // Natural cut at layer 5 (next_start=6): last boundary ≤ 6 and > 0 is 4 → end=3.
        assert_eq!(h.snap_end_layer(0, 5, 32), 3);
        // Already on a boundary (next_start=4 ∈ boundaries): end stays at 3.
        assert_eq!(h.snap_end_layer(0, 3, 32), 3);
        // Last layer: no snap needed.
        assert_eq!(h.snap_end_layer(0, 31, 32), 31);
    }

    #[test]
    fn hybrid_cuts_land_on_arch_boundaries() {
        // Budgets that would naturally cut mid-group: the planner must extend to
        // the next architectural boundary so the following slice starts on the
        // correct layer kind.
        let l = hybrid_layout(32, 200.0);
        let plan = plan_split(
            "/m/hybrid.gguf",
            &l,
            &[budget("a", 1300.0), budget("b", 8000.0)],
            0.0,
        )
        .unwrap();
        // Every shard start must be on an arch boundary (or 0).
        let boundaries = l.arch_boundaries();
        for shard in &plan.shards {
            assert!(
                shard.start_layer == 0 || boundaries.contains(&shard.start_layer),
                "slice starting at {} is not on an arch boundary",
                shard.start_layer
            );
        }
        assert_eq!(plan.shards.last().unwrap().end_layer, 31);
    }

    #[test]
    fn asymmetric_budgets_give_asymmetric_ranges() {
        // 32 layers of 200 MB, plus 100 MB embedding on every slice.
        let l = layout(32, 200.0);
        // 0% reserve keeps the arithmetic checkable by hand.
        let plan = plan_split(
            "/m/qwen.gguf",
            &l,
            &[budget("a", 2100.0), budget("b", 5000.0)],
            0.0,
        )
        .unwrap();

        // a: 100 embd + 10*200 = 2100 → exactly 10 layers.
        assert_eq!(plan.shards[0].start_layer, 0);
        assert_eq!(plan.shards[0].layers, 10);
        // b picks up from 10 and is the last, so it also pays the 100 MB head.
        assert_eq!(plan.shards[1].start_layer, 10);
        assert!(plan.shards[1].layers > plan.shards[0].layers);
    }

    #[test]
    fn kv_bytes_entram_na_decisao_de_corte_e_nao_so_na_reserva() {
        let l = layout(4, 10.0);
        let info = GgufInfo {
            attention_head_count_kv: Some(vec![8]),
            attention_key_length: Some(vec![128]),
            attention_value_length: Some(vec![128]),
            ..GgufInfo::default()
        };
        let plan = plan_split_with_kv(
            "/m/qwen.gguf",
            &l,
            &info,
            &[budget("a", 120.0), budget("b", 500.0)],
            0.0,
            KvPlanParams {
                n_ctx: 1024,
                max_sessions: 2,
                cache_type_k: KvCacheType::F16,
                cache_type_v: KvCacheType::F16,
            },
        )
        .unwrap();

        assert_eq!(plan.shards[0].layers, 1);
        assert_eq!(plan.shards[0].kv_bytes, 8 * 1024 * 1024);
        assert_eq!(
            plan.shards[0].total_bytes,
            plan.shards[0].weight_bytes + plan.shards[0].kv_bytes
        );
    }

    #[test]
    fn kv_sem_metadado_completo_recusa_o_plano() {
        let l = layout(2, 10.0);
        let info = GgufInfo::default();
        let error = plan_split_with_kv(
            "/m/qwen.gguf",
            &l,
            &info,
            &[budget("a", 1000.0)],
            0.0,
            KvPlanParams {
                n_ctx: 1024,
                max_sessions: 1,
                cache_type_k: KvCacheType::F16,
                cache_type_v: KvCacheType::F16,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("attention.head_count_kv"));
    }

    #[test]
    fn reports_the_gap_when_budgets_fall_short() {
        let l = layout(32, 200.0);
        let plan = plan_split(
            "/m/qwen.gguf",
            &l,
            &[budget("a", 1100.0), budget("b", 1100.0)],
            0.0,
        )
        .unwrap();
        assert!(!plan.complete);
        // a (not last): 100 embd + 5*200 = 1100 → layers 0-4.
        // b (last) also carries the 100 MB head, so only 4 layers fit → 5-8.
        assert_eq!(plan.shards[0].layers, 5);
        assert_eq!(plan.shards[1].layers, 4);
        assert_eq!(plan.covered_layers, 9);
        assert_eq!(plan.missing_range, Some((9, 31)));
        assert!(plan.missing_mb > 0.0);
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("faltam as camadas")));
    }

    #[test]
    fn warns_when_a_budget_cannot_hold_one_layer() {
        let l = layout(32, 200.0);
        let plan = plan_split(
            "/m/qwen.gguf",
            &l,
            &[budget("tiny", 50.0), budget("b", 99000.0)],
            0.0,
        )
        .unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("nem uma camada")));
        // The remaining peer still covers everything.
        assert!(plan.complete);
    }

    #[test]
    fn the_last_size_entry_ignores_a_target_and_takes_the_rest() {
        let plan = plan_by_size(
            &layout(10, 100.0),
            &[SizedSlice::of_mb(300.0), SizedSlice::of_mb(1.0)],
        )
        .unwrap();

        let last = plan.ranges.last().unwrap();
        assert_eq!(last.target_mb, None);
        assert!(last.is_rest);
        assert_eq!(last.end_layer, 9);
        assert!(plan.complete);
    }

    #[test]
    fn equal_parts_leave_the_last_entry_as_rest() {
        let plan = plan_equal(&layout(10, 100.0), 3).unwrap();
        let last = plan.ranges.last().unwrap();

        assert!(last.is_rest);
        assert_eq!(last.target_mb, None);
        assert_eq!(last.end_layer, 9);
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
    fn base_name_is_just_the_file_stem() {
        assert_eq!(base_name_of("/models/qwen/qwen_0_15.gguf"), "qwen_0_15");
        assert_eq!(
            base_name_of("/x/Qwen3.6-35B-A3B-Q4_K_M.gguf"),
            "Qwen3.6-35B-A3B-Q4_K_M"
        );
        assert_eq!(base_name_of("/x/phi-4-Q6_K.gguf"), "phi-4-Q6_K");
    }

    #[tokio::test]
    async fn extract_tp_shard_writes_and_reuses_a_tensor_shard() {
        let dir = std::env::temp_dir().join(format!("pom-tp-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, crate::splitter::test_gguf_bytes(32)).unwrap();

        let outcome = extract_tp_shard(whole.to_str().unwrap(), &dir, None, 2, 0, |_| {})
            .await
            .unwrap();
        assert!(!outcome.reused);
        // O nome é o conteúdo: `<sha256>.gguf`, e nada mais.
        assert!(outcome
            .output_path
            .ends_with(&format!("{}.gguf", outcome.sha256)));
        assert!(std::path::Path::new(&outcome.output_path).exists());
        assert_eq!(
            crate::pom::hash_file(std::path::Path::new(&outcome.output_path))
                .unwrap()
                .0,
            outcome.sha256,
            "o hash do passe de escrita tem que bater com o do arquivo em disco"
        );

        // Sabendo o hash, reaproveita sem reescrever.
        let again = extract_tp_shard(
            whole.to_str().unwrap(),
            &dir,
            Some(&outcome.sha256),
            2,
            0,
            |_| {},
        )
        .await
        .unwrap();
        assert!(
            again.reused,
            "arquivo já completo é reaproveitado, não reescrito"
        );
        assert_eq!(again.output_path, outcome.output_path);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Shard escrito por uma regra antiga **não** é reaproveitado.
    ///
    /// Este é o teste que impede a repetição de 17/08: três correções seguidas
    /// no fatiamento por tensor, e a ksia carregando o mesmo shard obsoleto em
    /// todas, porque o critério de reuso era "existe e tem mais de 1 KB".
    /// Agora o nome do arquivo é o hash do conteúdo, então "regra antiga"
    /// e "regra atual" nem disputam o mesmo nome - o reuso só acerta quando os
    /// bytes são exatamente os pedidos.
    #[tokio::test]
    async fn a_shard_from_an_older_rule_is_rewritten_not_reused() {
        let dir = std::env::temp_dir().join(format!("pom-tp-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, crate::splitter::test_gguf_bytes(32)).unwrap();

        // Um "shard" grande que nunca passou pelo fatiador, gravado sob um hash
        // que não é o dele - exatamente o que uma regra antiga deixava para
        // trás.
        let hash_mentiroso = "b".repeat(64);
        let velho = crate::pom::part_path(&dir, &hash_mentiroso).unwrap();
        std::fs::write(&velho, vec![0u8; 4096]).unwrap();

        let outcome = extract_tp_shard(whole.to_str().unwrap(), &dir, None, 2, 0, |_| {})
            .await
            .unwrap();
        assert!(
            !outcome.reused,
            "sem hash esperado não há reuso: o fatiador corta e o conteúdo se nomeia"
        );
        assert_ne!(
            outcome.sha256, hash_mentiroso,
            "o shard novo não pode herdar o nome do lixo que estava na pasta"
        );

        // E o que saiu tem o carimbo da regra atual.
        let info = crate::gguf::read_info(std::path::Path::new(&outcome.output_path)).unwrap();
        let stamp = info
            .tp_stamp
            .expect("o shard novo carimba a regra que o escreveu");
        assert_eq!(stamp.format, crate::splitter::TP_SHARD_FORMAT);
        assert_eq!((stamp.size, stamp.rank), (2, 0));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── Fatias de tamanhos diferentes ────────────────────────────────────────
    //
    // Layout de referência: 32 camadas de 200 MB, embedding de 100 MB em toda
    // fatia, cabeça de 100 MB só na última. Uma fatia de N camadas que não é a
    // última custa 100 + 200N MB.

    #[test]
    fn sizes_become_layer_ranges_and_the_rounding_is_visible() {
        let l = layout(32, 200.0);
        let plan = plan_by_size(
            &l,
            &[
                SizedSlice::of_mb(1100.0),
                SizedSlice::of_mb(2100.0),
                SizedSlice::rest(),
            ],
        )
        .unwrap();

        // 100 de embedding + 5×200 = 1100 → camadas 0-4, exatamente no alvo.
        assert_eq!(
            (plan.ranges[0].start_layer, plan.ranges[0].end_layer),
            (0, 4)
        );
        assert_eq!(plan.ranges[0].bytes as f64 / MB, 1100.0);
        // 100 + 10×200 = 2100 → camadas 5-14.
        assert_eq!(
            (plan.ranges[1].start_layer, plan.ranges[1].end_layer),
            (5, 14)
        );
        // O resto leva 15-31 e paga a cabeça.
        assert!(plan.ranges[2].is_rest);
        assert_eq!(
            (plan.ranges[2].start_layer, plan.ranges[2].end_layer),
            (15, 31)
        );
        assert!(plan.ranges[2].is_last);
        assert!(plan.complete);
        assert!(!plan.blocked);
    }

    #[test]
    fn a_target_that_falls_between_layers_rounds_down_and_says_so() {
        let l = layout(32, 200.0);
        // 1250 MB dá para 5 camadas (1100) mas não para 6 (1300).
        let plan = plan_by_size(&l, &[SizedSlice::of_mb(1250.0), SizedSlice::rest()]).unwrap();
        assert_eq!(plan.ranges[0].layers, 5);
        assert_eq!(plan.ranges[0].target_mb, Some(1250.0));
        assert_eq!(
            plan.ranges[0].bytes as f64 / MB,
            1100.0,
            "o tamanho real fica abaixo do alvo, e é isso que a tela mostra"
        );
    }

    #[test]
    fn a_target_smaller_than_one_layer_is_refused_with_a_reason() {
        let l = layout(32, 200.0);
        let plan = plan_by_size(&l, &[SizedSlice::of_mb(50.0), SizedSlice::rest()]).unwrap();
        assert!(plan.ranges[0].too_small);
        assert_eq!(
            plan.ranges[0].layers, 1,
            "uma camada é o mínimo indivisível"
        );
        assert!(
            plan.blocked,
            "não se fatia com uma fatia impossível na lista"
        );
        assert!(plan.warnings.iter().any(|w| w.contains("nem a camada 0")));
    }

    #[test]
    fn the_last_numeric_target_is_promoted_to_the_rest() {
        let l = layout(32, 200.0);
        let plan = plan_by_size(&l, &[SizedSlice::of_mb(1100.0)]).unwrap();
        assert!(plan.complete);
        assert_eq!(plan.covered_layers, 32);
        assert_eq!(plan.missing_range, None);
        assert_eq!(plan.missing_bytes, 0);
        assert!(
            plan.warnings.is_empty(),
            "a última entrada é o resto e fecha a cobertura"
        );
        assert!(
            !plan.blocked,
            "faltar camada não impede fatiar o que já foi pedido"
        );
    }

    #[test]
    fn asking_for_more_than_the_model_has_leaves_the_extra_slices_empty() {
        let l = layout(8, 200.0);
        let plan = plan_by_size(
            &l,
            &[SizedSlice::of_mb(99_000.0), SizedSlice::of_mb(1000.0)],
        )
        .unwrap();
        assert_eq!(plan.ranges[0].layers, 8, "a primeira já cobre o modelo");
        assert!(plan.ranges[1].empty);
        assert!(plan.blocked);
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("não sobrou camada")));
        assert!(
            plan.complete,
            "cobertura fechada, o problema é a fatia vazia"
        );
    }

    #[test]
    fn the_equal_shortcut_fills_the_list_with_real_sizes() {
        let l = layout(32, 200.0);
        let plan = plan_equal(&l, 4).unwrap();
        assert_eq!(plan.ranges.len(), 4);
        assert!(plan.complete && !plan.blocked);
        for (i, r) in plan.ranges.iter().enumerate() {
            assert_eq!(r.layers, 8);
            // A última entrada é o resto, mesmo no atalho de partes iguais.
            assert_eq!(r.target_mb, (i < 3).then_some(r.bytes as f64 / MB));
        }
        assert!(plan.ranges[3].bytes > plan.ranges[0].bytes);
    }

    #[test]
    fn the_rest_alone_is_the_whole_model() {
        let l = layout(32, 200.0);
        let plan = plan_by_size(&l, &[SizedSlice::rest()]).unwrap();
        assert_eq!(plan.ranges.len(), 1);
        assert_eq!(
            (plan.ranges[0].start_layer, plan.ranges[0].end_layer),
            (0, 31)
        );
        assert!(plan.complete && !plan.blocked);
    }

    #[test]
    fn an_empty_list_is_not_a_plan() {
        let l = layout(32, 200.0);
        assert!(plan_by_size(&l, &[]).is_err());
    }

    #[test]
    fn nextn_complete_split_says_the_predictor_rides_the_last_slice() {
        let mut l = layout(32, 200.0);
        l.nextn_predict_layers = 1;
        let plan = plan_by_size(&l, &[SizedSlice::of_mb(1100.0), SizedSlice::rest()]).unwrap();
        assert!(plan.complete);
        // The note only makes sense when there is an actual split.
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("predictor nextn") && w.contains("última camada")));
    }

    #[test]
    fn nextn_numeric_last_entry_still_carries_the_predictor() {
        let mut l = layout(32, 200.0);
        l.nextn_predict_layers = 1;
        let plan = plan_by_size(&l, &[SizedSlice::of_mb(1100.0)]).unwrap();
        assert!(plan.complete);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn nextn_single_slice_whole_model_gets_no_note() {
        let mut l = layout(32, 200.0);
        l.nextn_predict_layers = 1;
        let plan = plan_by_size(&l, &[SizedSlice::rest()]).unwrap();
        assert!(plan.complete);
        assert!(
            plan.warnings.is_empty(),
            "fatiar o modelo inteiro em uma fatia não gera observação"
        );
    }

    #[test]
    fn a_catalog_name_becomes_a_readable_base() {
        // O caso que motiva isto: o peso do Ollama é um blob por digest.
        assert_eq!(
            base_name_from_label("qwen3:8b").as_deref(),
            Some("qwen3-8b")
        );
        assert_eq!(
            base_name_from_label("lmstudio-community/Qwen3.6-35B").as_deref(),
            Some("lmstudio-community-Qwen3.6-35B")
        );
        // Um rótulo com números no fim já não significa nada: a faixa mora no
        // `.pom`, não no nome.
        assert_eq!(
            base_name_from_label("modelo_0_15").as_deref(),
            Some("modelo_0_15")
        );
        assert_eq!(base_name_from_label("  ").as_deref(), None);
        assert_eq!(base_name_from_label("///").as_deref(), None);

        // Numa pasta comum o modelo se chama pelo arquivo, extensão e tudo, e
        // empilhar outra daria `modelo.gguf_0_15.gguf`.
        assert_eq!(
            base_name_from_label("Qwen3.5-9B-DeepSeek-V4-Flash-Q4_K_S.gguf").as_deref(),
            Some("Qwen3.5-9B-DeepSeek-V4-Flash-Q4_K_S")
        );
    }

    #[test]
    fn even_split_covers_every_layer_without_a_gap() {
        assert_eq!(even_split(32, 2), vec![(0, 15), (16, 31)]);
        assert_eq!(even_split(32, 1), vec![(0, 31)]);
        // 32 em 3: 11 + 11 + 10, e o resto vai para as primeiras.
        assert_eq!(even_split(32, 3), vec![(0, 10), (11, 21), (22, 31)]);

        for parts in 1..=9u32 {
            let ranges = even_split(37, parts);
            assert_eq!(ranges[0].0, 0);
            assert_eq!(ranges.last().unwrap().1, 36);
            for pair in ranges.windows(2) {
                assert_eq!(pair[1].0, pair[0].1 + 1, "faixas contíguas, sem buraco");
            }
        }
    }

    #[test]
    fn even_split_never_produces_an_empty_slice() {
        // Mais fatias do que camadas seria uma fatia vazia — que o servidor de
        // inferência não carrega.
        assert_eq!(even_split(3, 8).len(), 3);
        assert!(even_split(0, 4).is_empty());
        assert!(even_split(10, 0).is_empty());
    }
}
