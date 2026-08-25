//! Runtime local que entrega estados ocultos ao bloco MoE Q4_0.
//!
//! O runtime não cria um segundo protocolo: recebe o mesmo vetor f32 que
//! `decode_hidden` já aceitou do peer e devolve outro vetor da mesma forma.
//! Atenção e o restante das camadas continuam no contexto llama.cpp; aqui a
//! fronteira própria é o router GGUF mais o FFN esparso da camada local.

use super::block::{
    HeterogeneousMoeBlock, IqQuantMoeBlock, LegacyQuantMoeBlock, LowBitMoeBlock, MoeBlockError,
    Nvfp4MoeBlock, PlainMoeBlock, Q4_0MoeBlock,
};
use super::cache::{CacheError, ExpertCache};
use super::format_heterogeneous::{HeterogeneousMoeSource, TensorRole};
use super::format_iq_quants::{IqQuantFormat, IqQuantSource};
use super::format_k_quants::{KQuantFormat, KQuantSource};
use super::format_legacy_quants::{LegacyQuantFormat, LegacyQuantSource};
use super::format_low_bits::{LowBitFormat, LowBitSource};
use super::format_nvfp4::Nvfp4Source;
use super::format_plain::{PlainFormat, PlainSource};
use super::format_q4_0::{
    detect_expert_dtypes, detect_expert_format, detect_expert_layer_count, ggml_type_name,
    ExpertFormat,
};
use super::format_q4_0::{Q4Error, Q4LoadError, Q4_0Source};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoeExecutorKind {
    CpuReference,
    Cuda,
}

impl MoeExecutorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CpuReference => "cpu-reference",
            Self::Cuda => "cuda",
        }
    }
}

/// Evidência do executor que realmente processou os experts de um forward.
///
/// `dtypes` é uma lista, não um único valor: arquivos UD podem misturar IQ3_S,
/// IQ2_S e IQ4_XS no mesmo modelo. `reason` fica preenchido quando o caminho
/// CPUReference é escolhido por falta de ponte CUDA, para que a telemetria não
/// transforme o caminho lento em uma escolha invisível.
#[derive(Debug, Clone, PartialEq)]
pub struct MoeExecutorTelemetry {
    pub executor: MoeExecutorKind,
    pub dtypes: Vec<u32>,
    pub reason: Option<String>,
    pub diagnostics: Option<MoeExecutorDiagnostics>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MoeExecutorDiagnostics {
    pub profiled: bool,
    pub calls: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_admissions: u64,
    pub cache_evictions: u64,
    pub cache_rejections: u64,
    pub cache_entries: usize,
    pub cache_resident_bytes: usize,
    pub device_cache_hits: u64,
    pub device_cache_misses: u64,
    pub device_cache_admissions: u64,
    pub device_cache_evictions: u64,
    pub device_cache_rejections: u64,
    pub device_cache_uploads: u64,
    pub device_cache_upload_bytes: u64,
    pub device_cache_upload_ms: f64,
    pub device_cache_resident_bytes: usize,
    pub payload_validation_ms: f64,
    pub cuda_alloc_ms: f64,
    pub cuda_h2d_ms: f64,
    pub cuda_h2d_bytes: u64,
    pub cuda_kernel_ms: f64,
    pub cuda_launch_ms: f64,
    pub cuda_sync_ms: f64,
    pub cuda_d2h_ms: f64,
    pub cuda_d2h_bytes: u64,
    pub cuda_free_ms: f64,
    pub host_cache_lookup_ms: f64,
    pub host_expert_load_ms: f64,
    pub host_cache_admit_ms: f64,
    pub host_batch_prepare_ms: f64,
    pub host_result_copy_ms: f64,
    pub host_aggregation_ms: f64,
    pub cuda_call_wall_ms: f64,
    pub runtime_forward_ms: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct MoeRuntimeDiagnostics {
    runtime_forward_ms: f64,
}

impl From<super::format_nvfp4::Nvfp4BackendKind> for MoeExecutorKind {
    fn from(kind: super::format_nvfp4::Nvfp4BackendKind) -> Self {
        match kind {
            super::format_nvfp4::Nvfp4BackendKind::CpuReference => Self::CpuReference,
            super::format_nvfp4::Nvfp4BackendKind::Cuda => Self::Cuda,
        }
    }
}

#[derive(Debug)]
pub enum MoeRuntimeError {
    Source(Q4LoadError),
    Cache(CacheError),
    Format(Q4Error),
    Block(String),
    FormatMismatch,
    MissingLayer {
        layer: u32,
        layers: u32,
    },
    InvalidShape {
        values: usize,
        tokens: usize,
        hidden: usize,
    },
}

impl fmt::Display for MoeRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Cache(error) => error.fmt(formatter),
            Self::Format(error) => error.fmt(formatter),
            Self::Block(error) => formatter.write_str(error),
            Self::FormatMismatch => {
                formatter.write_str("fonte e bloco MoE usam formatos diferentes")
            }
            Self::MissingLayer { layer, layers } => {
                write!(formatter, "camada MoE local {layer} fora de [0, {layers})")
            }
            Self::InvalidShape {
                values,
                tokens,
                hidden,
            } => write!(
                formatter,
                "runtime MoE recebeu {values} floats, esperado {tokens} × {hidden}"
            ),
        }
    }
}

impl std::error::Error for MoeRuntimeError {}

impl From<Q4LoadError> for MoeRuntimeError {
    fn from(error: Q4LoadError) -> Self {
        Self::Source(error)
    }
}

impl From<CacheError> for MoeRuntimeError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<Q4Error> for MoeRuntimeError {
    fn from(error: Q4Error) -> Self {
        Self::Format(error)
    }
}

impl From<MoeBlockError> for MoeRuntimeError {
    fn from(error: MoeBlockError) -> Self {
        Self::Block(error.to_string())
    }
}

#[cfg(any(feature = "cuda", test))]
fn require_cuda_compute_bytes(compute_bytes: Option<u64>) -> Result<u64, MoeRuntimeError> {
    compute_bytes.filter(|bytes| *bytes > 0).ok_or_else(|| {
        MoeRuntimeError::Block(
            "executor CUDA recusado: llama.cpp não reportou um compute buffer CUDA medido".into(),
        )
    })
}

#[cfg(feature = "cuda")]
fn create_measured_device_cache(
    compute_bytes: Option<u64>,
) -> Result<Arc<super::cache::DeviceExpertCache>, MoeRuntimeError> {
    let compute_bytes = require_cuda_compute_bytes(compute_bytes)?;
    super::cuda::create_device_cache(compute_bytes)
        .map_err(|error| MoeRuntimeError::Block(error.to_string()))
}

enum MoeSource {
    Legacy(LegacyQuantSource),
    Plain(PlainSource),
    Q4(Q4_0Source),
    K(KQuantSource),
    Iq(IqQuantSource),
    Nvfp4(Nvfp4Source),
    LowBit(LowBitSource),
    Heterogeneous(HeterogeneousMoeSource),
}

impl MoeSource {
    fn hidden(&self) -> usize {
        match self {
            Self::Legacy(source) => source.hidden(),
            Self::Plain(source) => source.hidden(),
            Self::Q4(source) => source.hidden(),
            Self::K(source) => source.hidden(),
            Self::Iq(source) => source.hidden(),
            Self::Nvfp4(source) => source.hidden(),
            Self::LowBit(source) => source.hidden(),
            Self::Heterogeneous(source) => source.hidden(),
        }
    }

    fn experts(&self) -> u32 {
        match self {
            Self::Legacy(source) => source.experts(),
            Self::Plain(source) => source.experts(),
            Self::Q4(source) => source.experts(),
            Self::K(source) => source.experts(),
            Self::Iq(source) => source.experts(),
            Self::Nvfp4(source) => source.experts(),
            Self::LowBit(source) => source.experts(),
            Self::Heterogeneous(source) => source.experts(),
        }
    }

    fn dtype(&self) -> u32 {
        match self {
            Self::Legacy(source) => source.format().dtype(),
            Self::Plain(source) => source.format().dtype(),
            Self::Q4(_) => 2,
            Self::K(source) => source.format().dtype(),
            Self::Iq(source) => source.format().dtype(),
            Self::Nvfp4(_) => 40,
            Self::LowBit(source) => source.format().dtype(),
            Self::Heterogeneous(_) => 0,
        }
    }

    fn dtypes(&self) -> Vec<u32> {
        let mut dtypes = match self {
            Self::Legacy(source) => vec![source.format().dtype()],
            Self::Plain(source) => vec![source.format().dtype()],
            Self::Q4(_) => vec![2],
            Self::K(source) => vec![source.format().dtype()],
            Self::Iq(source) => vec![source.format().dtype()],
            Self::Nvfp4(_) => vec![40],
            Self::LowBit(source) => vec![source.format().dtype()],
            Self::Heterogeneous(source) => source.dtypes(),
        };
        dtypes.sort_unstable();
        dtypes.dedup();
        dtypes
    }

    fn activation(&self) -> Option<i32> {
        match self {
            Self::Nvfp4(_) => Some(2), // LLM_FFN_GELU, as implemented by NVFP4Expert
            Self::Heterogeneous(_) => None,
            _ => Some(1), // LLM_FFN_SILU, as implemented by the other decoders
        }
    }
}

enum MoeBlock {
    Legacy(LegacyQuantMoeBlock),
    Plain(PlainMoeBlock),
    Q4(Q4_0MoeBlock),
    K(super::block::KQuantMoeBlock),
    Iq(IqQuantMoeBlock),
    Nvfp4(Nvfp4MoeBlock),
    LowBit(LowBitMoeBlock),
    Heterogeneous(HeterogeneousMoeBlock),
}

pub struct MoeRuntime {
    source: MoeSource,
    blocks: Vec<MoeBlock>,
    cache: Arc<ExpertCache>,
    runtime_diagnostics: Mutex<MoeRuntimeDiagnostics>,
}

pub struct MoeRouteBatch<'a> {
    pub layer: u32,
    pub hidden: &'a [f32],
    pub selected_experts: &'a [i32],
    pub weights: &'a [f32],
    pub tokens: usize,
    pub expert_count: usize,
    pub expert_count_used: usize,
    pub activation: i32,
    pub weight_before_ffn: bool,
}

/// Ownership decision made before loading the llama.cpp model.
///
/// A `1` means that every expert tensor in that local layer has a device
/// executor and may be metadata-only in llama.cpp. A `0` is an explicit
/// native-ownership decision: llama.cpp keeps and executes that layer's
/// experts. Keeping this list data-driven prevents F5-E2 from turning an
/// unsupported dtype into a model with no executor at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoeExternalExecutorPlan {
    pub layers: Vec<u8>,
    pub dtypes: Vec<u32>,
    pub reason: Option<String>,
}

impl MoeExternalExecutorPlan {
    pub fn enabled(&self) -> bool {
        self.layers.iter().any(|value| *value != 0)
    }
}

fn make_external_executor_plan(
    detected: ExpertFormat,
    dtypes: Vec<u32>,
    layers: usize,
    cuda_compiled: bool,
    nvfp4_cpu_requested: bool,
) -> MoeExternalExecutorPlan {
    let dtype_list = dtypes
        .iter()
        .map(|dtype| format!("{dtype} ({})", ggml_type_name(*dtype)))
        .collect::<Vec<_>>()
        .join(", ");
    let single_dtype = dtypes.len() == 1;
    let bridge_supported = match detected {
        ExpertFormat::Nvfp4 => cuda_compiled && !nvfp4_cpu_requested,
        ExpertFormat::Q4K | ExpertFormat::Q6K => {
            cuda_compiled
                && single_dtype
                && KQuantFormat::from_dtype(dtypes[0]).is_ok_and(|format| format.cuda_supported())
        }
        ExpertFormat::Iq3S | ExpertFormat::Iq2S | ExpertFormat::Iq4XS => {
            cuda_compiled
                && single_dtype
                && IqQuantFormat::from_dtype(dtypes[0]).is_ok_and(|format| format.cuda_supported())
        }
        ExpertFormat::Heterogeneous => false,
        _ => false,
    };
    if bridge_supported {
        return MoeExternalExecutorPlan {
            layers: vec![1; layers],
            dtypes,
            reason: None,
        };
    }

    let reason = if detected == ExpertFormat::Heterogeneous {
        format!(
            "executor CUDA recusado para dtypes heterogêneos [{dtype_list}]: ponte por tensor ainda não disponível; experts permanecem no llama.cpp"
        )
    } else if !cuda_compiled {
        format!(
            "executor CUDA recusado para dtype [{dtype_list}]: binário sem feature cuda; experts permanecem no llama.cpp"
        )
    } else if detected == ExpertFormat::Nvfp4 && nvfp4_cpu_requested {
        format!(
            "executor CUDA recusado para dtype [{dtype_list}]: POM_MOE_NVFP4_BACKEND=cpu; experts permanecem no llama.cpp"
        )
    } else {
        format!(
            "executor CUDA recusado para dtype [{dtype_list}]: ponte device não suportada; experts permanecem no llama.cpp"
        )
    };
    MoeExternalExecutorPlan {
        layers: vec![0; layers],
        dtypes,
        reason: Some(reason),
    }
}

impl MoeRuntime {
    /// Decides ownership before `llama_model_load_from_file` so unsupported
    /// formats never have their experts removed from the native model.
    pub fn external_executor_plan(
        path: impl AsRef<Path>,
    ) -> Result<MoeExternalExecutorPlan, MoeRuntimeError> {
        let detected = detect_expert_format(&path)?;
        let dtypes = detect_expert_dtypes(&path)?;
        let layers = detect_expert_layer_count(&path)?;
        Ok(make_external_executor_plan(
            detected,
            dtypes,
            layers,
            cfg!(feature = "cuda"),
            std::env::var("POM_MOE_NVFP4_BACKEND")
                .is_ok_and(|value| value.eq_ignore_ascii_case("cpu")),
        ))
    }

    /// Carrega o primeiro formato próprio suportado e compartilha o cache
    /// entre todas as camadas locais desta fatia.
    pub fn open(
        path: impl AsRef<Path>,
        model: impl Into<String>,
        experts: u32,
        top_k: usize,
        weights_bytes: u64,
        cuda_compute_bytes: Option<u64>,
    ) -> Result<Self, MoeRuntimeError> {
        #[cfg(not(feature = "cuda"))]
        let _ = cuda_compute_bytes;
        let cache = ExpertCache::measured_system(weights_bytes, 0, 0)?;
        let model = model.into();
        let detected = detect_expert_format(&path)?;
        match detected {
            ExpertFormat::Q4_1 | ExpertFormat::Q5_0 | ExpertFormat::Q5_1 | ExpertFormat::Q8_0 => {
                let format = match detected {
                    ExpertFormat::Q4_1 => LegacyQuantFormat::Q4_1,
                    ExpertFormat::Q5_0 => LegacyQuantFormat::Q5_0,
                    ExpertFormat::Q5_1 => LegacyQuantFormat::Q5_1,
                    ExpertFormat::Q8_0 => LegacyQuantFormat::Q8_0,
                    _ => unreachable!("detect_expert_format mudou após seleção legacy"),
                };
                let source = LegacyQuantSource::open_infer(&path, experts)?;
                if source.format() != format {
                    return Err(MoeRuntimeError::FormatMismatch);
                }
                if !source.has_router() {
                    return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                        "blk.<layer>.ffn_gate_inp.weight".into(),
                    )));
                }
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::Legacy(
                        LegacyQuantMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(MoeRuntimeError::Block)?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::Legacy(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
            ExpertFormat::F32 | ExpertFormat::F16 | ExpertFormat::Bf16 => {
                let format = match detected {
                    ExpertFormat::F32 => PlainFormat::F32,
                    ExpertFormat::F16 => PlainFormat::F16,
                    ExpertFormat::Bf16 => PlainFormat::Bf16,
                    _ => unreachable!("detect_expert_format mudou após seleção escalar"),
                };
                let source = PlainSource::open_infer(&path, experts)?;
                if source.format() != format {
                    return Err(MoeRuntimeError::FormatMismatch);
                }
                if !source.has_router() {
                    return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                        "blk.<layer>.ffn_gate_inp.weight".into(),
                    )));
                }
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::Plain(
                        PlainMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(MoeRuntimeError::Block)?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::Plain(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
            ExpertFormat::Q4_0 => {
                let source = Q4_0Source::open_infer(&path, experts)?;
                if !source.has_router() {
                    return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                        "blk.<layer>.ffn_gate_inp.weight".into(),
                    )));
                }
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::Q4(Q4_0MoeBlock::new(
                        Arc::clone(&cache),
                        &model,
                        layer,
                        top_k,
                        source.hidden(),
                        source.intermediate(),
                    )?));
                }
                Ok(Self {
                    source: MoeSource::Q4(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
            ExpertFormat::Nvfp4 => {
                let source = Nvfp4Source::open_infer(&path, experts)?;
                if !source.has_router() {
                    return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                        "blk.<layer>.ffn_gate_inp.weight".into(),
                    )));
                }
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                #[cfg(feature = "cuda")]
                let device_cache = if std::env::var("POM_MOE_NVFP4_BACKEND")
                    .is_ok_and(|value| value.eq_ignore_ascii_case("cpu"))
                {
                    None
                } else {
                    Some(create_measured_device_cache(cuda_compute_bytes)?)
                };
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    #[cfg(feature = "cuda")]
                    let block = if let Some(device_cache) = device_cache.as_ref() {
                        Nvfp4MoeBlock::new_with_device_cache(
                            Arc::clone(&cache),
                            Arc::clone(device_cache),
                            &model,
                            layer,
                            top_k,
                            source.hidden(),
                            source.intermediate(),
                        )
                    } else {
                        Nvfp4MoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            source.hidden(),
                            source.intermediate(),
                        )
                    }
                    .map_err(MoeRuntimeError::Block)?;
                    #[cfg(not(feature = "cuda"))]
                    let block = Nvfp4MoeBlock::new(
                        Arc::clone(&cache),
                        &model,
                        layer,
                        top_k,
                        source.hidden(),
                        source.intermediate(),
                    )
                    .map_err(MoeRuntimeError::Block)?;
                    blocks.push(MoeBlock::Nvfp4(block));
                }
                Ok(Self {
                    source: MoeSource::Nvfp4(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
            ExpertFormat::Mxfp4
            | ExpertFormat::Tq1_0
            | ExpertFormat::Tq2_0
            | ExpertFormat::Q1_0
            | ExpertFormat::Q2_0 => {
                let format = match detected {
                    ExpertFormat::Mxfp4 => LowBitFormat::Mxfp4,
                    ExpertFormat::Tq1_0 => LowBitFormat::Tq1_0,
                    ExpertFormat::Tq2_0 => LowBitFormat::Tq2_0,
                    ExpertFormat::Q1_0 => LowBitFormat::Q1_0,
                    ExpertFormat::Q2_0 => LowBitFormat::Q2_0,
                    _ => unreachable!("detect_expert_format mudou após seleção low-bit"),
                };
                let source = LowBitSource::open_infer(&path, experts)?;
                if source.format() != format {
                    return Err(MoeRuntimeError::FormatMismatch);
                }
                if !source.has_router() {
                    return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                        "blk.<layer>.ffn_gate_inp.weight".into(),
                    )));
                }
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::LowBit(
                        LowBitMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(MoeRuntimeError::Block)?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::LowBit(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
            ExpertFormat::Q2K
            | ExpertFormat::Q3K
            | ExpertFormat::Q4K
            | ExpertFormat::Q5K
            | ExpertFormat::Q6K
            | ExpertFormat::Q8K => {
                let format = match detected {
                    ExpertFormat::Q2K => KQuantFormat::Q2K,
                    ExpertFormat::Q3K => KQuantFormat::Q3K,
                    ExpertFormat::Q4K => KQuantFormat::Q4K,
                    ExpertFormat::Q5K => KQuantFormat::Q5K,
                    ExpertFormat::Q6K => KQuantFormat::Q6K,
                    ExpertFormat::Q8K => KQuantFormat::Q8K,
                    _ => unreachable!("detect_expert_format mudou após a seleção K-quant"),
                };
                let source = KQuantSource::open_infer(&path, experts)?;
                if source.format() != format {
                    return Err(MoeRuntimeError::FormatMismatch);
                }
                if !source.has_router() {
                    return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                        "blk.<layer>.ffn_gate_inp.weight".into(),
                    )));
                }
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                #[cfg(feature = "cuda")]
                let device_cache = if format.cuda_supported() {
                    Some(create_measured_device_cache(cuda_compute_bytes)?)
                } else {
                    None
                };
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    #[cfg(feature = "cuda")]
                    let block = if let Some(device_cache) = device_cache.as_ref() {
                        super::block::KQuantMoeBlock::new_with_device_cache(
                            Arc::clone(&cache),
                            Arc::clone(device_cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                    } else {
                        super::block::KQuantMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                    };
                    #[cfg(not(feature = "cuda"))]
                    let block = super::block::KQuantMoeBlock::new(
                        Arc::clone(&cache),
                        &model,
                        layer,
                        top_k,
                        format,
                        source.hidden(),
                        source.intermediate(),
                    );
                    blocks.push(MoeBlock::K(
                        block.map_err(|error| MoeRuntimeError::Block(error.to_string()))?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::K(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
            ExpertFormat::Iq2XXS
            | ExpertFormat::Iq2XS
            | ExpertFormat::Iq3XXS
            | ExpertFormat::Iq1S
            | ExpertFormat::Iq4NL
            | ExpertFormat::Iq3S
            | ExpertFormat::Iq2S
            | ExpertFormat::Iq4XS
            | ExpertFormat::Iq1M => {
                let format = match detected {
                    ExpertFormat::Iq2XXS => IqQuantFormat::Iq2XXS,
                    ExpertFormat::Iq2XS => IqQuantFormat::Iq2XS,
                    ExpertFormat::Iq3XXS => IqQuantFormat::Iq3XXS,
                    ExpertFormat::Iq1S => IqQuantFormat::Iq1S,
                    ExpertFormat::Iq4NL => IqQuantFormat::Iq4NL,
                    ExpertFormat::Iq3S => IqQuantFormat::Iq3S,
                    ExpertFormat::Iq2S => IqQuantFormat::Iq2S,
                    ExpertFormat::Iq4XS => IqQuantFormat::Iq4XS,
                    ExpertFormat::Iq1M => IqQuantFormat::Iq1M,
                    _ => unreachable!("detect_expert_format mudou após a seleção IQ-quant"),
                };
                let source = IqQuantSource::open_infer(&path, experts)?;
                if source.format() != format {
                    return Err(MoeRuntimeError::FormatMismatch);
                }
                if !source.has_router() {
                    return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                        "blk.<layer>.ffn_gate_inp.weight".into(),
                    )));
                }
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                #[cfg(feature = "cuda")]
                let device_cache = if format.cuda_supported() {
                    Some(create_measured_device_cache(cuda_compute_bytes)?)
                } else {
                    None
                };
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    #[cfg(feature = "cuda")]
                    let block = if let Some(device_cache) = device_cache.as_ref() {
                        IqQuantMoeBlock::new_with_device_cache(
                            Arc::clone(&cache),
                            Arc::clone(device_cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                    } else {
                        IqQuantMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                    };
                    #[cfg(not(feature = "cuda"))]
                    let block = IqQuantMoeBlock::new(
                        Arc::clone(&cache),
                        &model,
                        layer,
                        top_k,
                        format,
                        source.hidden(),
                        source.intermediate(),
                    );
                    blocks.push(MoeBlock::Iq(
                        block.map_err(|error| MoeRuntimeError::Block(error.to_string()))?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::Iq(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
            ExpertFormat::Heterogeneous => {
                let source = HeterogeneousMoeSource::open_infer(&path, experts)?;
                let layers = source.layer_count();
                if layers == 0 {
                    return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                        "GGUF MoE sem camadas",
                    )));
                }
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::Heterogeneous(HeterogeneousMoeBlock::new(
                        Arc::clone(&cache),
                        &model,
                        layer,
                        top_k,
                    )));
                }
                Ok(Self {
                    source: MoeSource::Heterogeneous(source),
                    blocks,
                    cache: Arc::clone(&cache),
                    runtime_diagnostics: Mutex::new(MoeRuntimeDiagnostics::default()),
                })
            }
        }
    }

    pub fn layers(&self) -> u32 {
        self.blocks.len() as u32
    }

    pub fn executor_kind(&self) -> MoeExecutorKind {
        match self.blocks.first() {
            Some(MoeBlock::Nvfp4(block)) => block.executor_kind().into(),
            #[cfg(feature = "cuda")]
            Some(MoeBlock::K(block)) if block.uses_cuda() => MoeExecutorKind::Cuda,
            #[cfg(feature = "cuda")]
            Some(MoeBlock::Iq(block)) if block.uses_cuda() => MoeExecutorKind::Cuda,
            Some(_) | None => MoeExecutorKind::CpuReference,
        }
    }

    fn executor_diagnostics(&self) -> MoeExecutorDiagnostics {
        let cache = self.cache.stats();
        let diagnostics = MoeExecutorDiagnostics {
            cache_hits: cache.hits,
            cache_misses: cache.misses,
            cache_admissions: cache.admissions,
            cache_evictions: cache.evictions,
            cache_rejections: cache.rejections,
            cache_entries: cache.entries,
            cache_resident_bytes: cache.resident_bytes,
            runtime_forward_ms: self
                .runtime_diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .runtime_forward_ms,
            ..MoeExecutorDiagnostics::default()
        };

        #[cfg(feature = "cuda")]
        let mut diagnostics = diagnostics;

        #[cfg(feature = "cuda")]
        {
            for block in &self.blocks {
                let (cuda, device_cache) = match block {
                    MoeBlock::Nvfp4(block) => {
                        (block.cuda_diagnostics(), block.resident_cache_stats())
                    }
                    MoeBlock::K(block) if block.uses_cuda() => {
                        (block.cuda_diagnostics(), block.resident_cache_stats())
                    }
                    MoeBlock::Iq(block) if block.uses_cuda() => {
                        (block.cuda_diagnostics(), block.resident_cache_stats())
                    }
                    _ => continue,
                };
                diagnostics.profiled |= cuda.calls > 0;
                diagnostics.calls = diagnostics.calls.saturating_add(cuda.calls);
                diagnostics.payload_validation_ms += cuda.payload_validation_ms;
                diagnostics.cuda_alloc_ms += cuda.alloc_ms;
                diagnostics.cuda_h2d_ms += cuda.h2d_ms;
                diagnostics.cuda_h2d_bytes =
                    diagnostics.cuda_h2d_bytes.saturating_add(cuda.h2d_bytes);
                diagnostics.cuda_kernel_ms += cuda.kernel_ms;
                diagnostics.cuda_launch_ms += cuda.launch_ms;
                diagnostics.cuda_sync_ms += cuda.sync_ms;
                diagnostics.cuda_d2h_ms += cuda.d2h_ms;
                diagnostics.cuda_d2h_bytes =
                    diagnostics.cuda_d2h_bytes.saturating_add(cuda.d2h_bytes);
                diagnostics.cuda_free_ms += cuda.free_ms;
                diagnostics.cuda_call_wall_ms += cuda.cuda_call_wall_ms;
                diagnostics.host_batch_prepare_ms += cuda.host_batch_prepare_ms;
                diagnostics.host_result_copy_ms += cuda.host_result_copy_ms;
                diagnostics.device_cache_hits = diagnostics
                    .device_cache_hits
                    .saturating_add(device_cache.hits);
                diagnostics.device_cache_misses = diagnostics
                    .device_cache_misses
                    .saturating_add(device_cache.misses);
                diagnostics.device_cache_admissions = diagnostics
                    .device_cache_admissions
                    .saturating_add(device_cache.admissions);
                diagnostics.device_cache_evictions = diagnostics
                    .device_cache_evictions
                    .saturating_add(device_cache.evictions);
                diagnostics.device_cache_rejections = diagnostics
                    .device_cache_rejections
                    .saturating_add(device_cache.rejections);
                diagnostics.device_cache_uploads = diagnostics
                    .device_cache_uploads
                    .saturating_add(device_cache.uploads);
                diagnostics.device_cache_upload_bytes = diagnostics
                    .device_cache_upload_bytes
                    .saturating_add(device_cache.upload_bytes);
                diagnostics.device_cache_upload_ms += device_cache.upload_ms;
                diagnostics.device_cache_resident_bytes = diagnostics
                    .device_cache_resident_bytes
                    .saturating_add(device_cache.resident_bytes);
                if let MoeBlock::Nvfp4(block) = block {
                    let host = block.host_diagnostics();
                    diagnostics.host_cache_lookup_ms += host.cache_lookup_ms;
                    diagnostics.host_expert_load_ms += host.expert_load_ms;
                    diagnostics.host_cache_admit_ms += host.cache_admit_ms;
                    diagnostics.host_aggregation_ms += host.aggregation_ms;
                }
            }
        }

        diagnostics
    }

    pub fn executor_telemetry(&self) -> MoeExecutorTelemetry {
        let executor = self.executor_kind();
        let dtypes = self.source.dtypes();
        let reason = match executor {
            MoeExecutorKind::Cuda => None,
            MoeExecutorKind::CpuReference => {
                let names = dtypes
                    .iter()
                    .map(|dtype| {
                        format!("{} ({dtype})", super::format_q4_0::ggml_type_name(*dtype))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let explicit_cpu = matches!(self.source, MoeSource::Nvfp4(_))
                    && std::env::var("POM_MOE_NVFP4_BACKEND")
                        .is_ok_and(|value| value.eq_ignore_ascii_case("cpu"));
                if explicit_cpu {
                    Some(format!(
                        "dtype(s) {names}: CPUReference solicitado por POM_MOE_NVFP4_BACKEND=cpu"
                    ))
                } else {
                    Some(format!(
                        "dtype(s) {names} sem ponte CUDA; CPUReference selecionado"
                    ))
                }
            }
        };
        MoeExecutorTelemetry {
            executor,
            dtypes,
            reason,
            diagnostics: Some(self.executor_diagnostics()),
        }
    }

    pub fn validate_callback_metadata(
        &self,
        layer: u32,
        metadata: &[(TensorRole, &str, u32)],
    ) -> Result<(), MoeRuntimeError> {
        if let MoeSource::Heterogeneous(source) = &self.source {
            return source
                .validate_metadata(layer, metadata)
                .map_err(MoeRuntimeError::Source);
        }
        let expected = self.source.dtype();
        for (role, name, dtype) in metadata {
            if *dtype != expected {
                return Err(MoeRuntimeError::Block(format!(
                    "tensor {name} ({role}) usa dtype GGML {dtype}, mas o runtime homogêneo abriu dtype GGML {expected}"
                )));
            }
        }
        Ok(())
    }

    pub fn forward_batch_with_route(
        &self,
        route: MoeRouteBatch<'_>,
    ) -> Result<Vec<f32>, MoeRuntimeError> {
        let runtime_started = std::time::Instant::now();
        let MoeRouteBatch {
            layer,
            hidden,
            selected_experts,
            weights,
            tokens,
            expert_count,
            expert_count_used,
            activation,
            weight_before_ffn,
        } = route;
        if weight_before_ffn {
            return Err(MoeRuntimeError::Block(
                "hook MoE não suporta pesos aplicados antes do FFN".into(),
            ));
        }
        let expected_ids =
            tokens
                .checked_mul(expert_count_used)
                .ok_or(MoeRuntimeError::InvalidShape {
                    values: selected_experts.len(),
                    tokens,
                    hidden: expert_count_used,
                })?;
        if expert_count != self.source.experts() as usize
            || expert_count_used == 0
            || expert_count_used > expert_count
            || hidden.len() != tokens.saturating_mul(self.source.hidden())
            || selected_experts.len() != expected_ids
            || weights.len() != expected_ids
        {
            return Err(MoeRuntimeError::FormatMismatch);
        }
        if let Some(expected_activation) = self.source.activation() {
            if activation != expected_activation {
                return Err(MoeRuntimeError::Block(format!(
                    "ativação MoE não suportada para dtype {}: {} (esperado {})",
                    self.source.dtype(),
                    activation,
                    expected_activation
                )));
            }
        }

        let resolved_ids = selected_experts
            .iter()
            .map(|&expert| {
                u32::try_from(expert).map_err(|_| {
                    MoeRuntimeError::Block(format!(
                        "id de expert negativo no runtime MoE (dtype {})",
                        self.source.dtype()
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let block = self
            .blocks
            .get(layer as usize)
            .ok_or(MoeRuntimeError::MissingLayer {
                layer,
                layers: self.layers(),
            })?;

        #[cfg(feature = "cuda")]
        if let (MoeSource::Nvfp4(source), MoeBlock::Nvfp4(block)) = (&self.source, block) {
            if block.executor_kind() == super::format_nvfp4::Nvfp4BackendKind::Cuda {
                let token_ids = (0..tokens)
                    .flat_map(|token| std::iter::repeat(token as u32).take(expert_count_used))
                    .collect::<Vec<_>>();
                let token_offsets = (0..=tokens)
                    .map(|token| token.saturating_mul(expert_count_used))
                    .collect::<Vec<_>>();
                let output = block.forward_batch_from_source_with_route(
                    source,
                    hidden,
                    &resolved_ids,
                    &token_ids,
                    weights,
                    &token_offsets,
                    tokens,
                )?;
                let elapsed_ms = runtime_started.elapsed().as_secs_f64() * 1_000.0;
                self.runtime_diagnostics
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .runtime_forward_ms += elapsed_ms;
                return Ok(output);
            }
        }

        #[cfg(feature = "cuda")]
        if let (MoeSource::K(source), MoeBlock::K(block)) = (&self.source, block) {
            if block.uses_cuda() {
                let token_ids = (0..tokens)
                    .flat_map(|token| std::iter::repeat(token as u32).take(expert_count_used))
                    .collect::<Vec<_>>();
                let token_offsets = (0..=tokens)
                    .map(|token| token.saturating_mul(expert_count_used))
                    .collect::<Vec<_>>();
                let output = block.forward_batch_from_source_with_route(
                    source,
                    hidden,
                    &resolved_ids,
                    &token_ids,
                    weights,
                    &token_offsets,
                    tokens,
                )?;
                let elapsed_ms = runtime_started.elapsed().as_secs_f64() * 1_000.0;
                self.runtime_diagnostics
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .runtime_forward_ms += elapsed_ms;
                return Ok(output);
            }
        }

        #[cfg(feature = "cuda")]
        if let (MoeSource::Iq(source), MoeBlock::Iq(block)) = (&self.source, block) {
            if block.uses_cuda() {
                let token_ids = (0..tokens)
                    .flat_map(|token| std::iter::repeat(token as u32).take(expert_count_used))
                    .collect::<Vec<_>>();
                let token_offsets = (0..=tokens)
                    .map(|token| token.saturating_mul(expert_count_used))
                    .collect::<Vec<_>>();
                let output = block.forward_batch_from_source_with_route(
                    source,
                    hidden,
                    &resolved_ids,
                    &token_ids,
                    weights,
                    &token_offsets,
                    tokens,
                )?;
                let elapsed_ms = runtime_started.elapsed().as_secs_f64() * 1_000.0;
                self.runtime_diagnostics
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .runtime_forward_ms += elapsed_ms;
                return Ok(output);
            }
        }

        let hidden_width = self.source.hidden();
        let mut output = Vec::with_capacity(hidden.len());
        for token in 0..tokens {
            let token_hidden = &hidden[token * hidden_width..(token + 1) * hidden_width];
            let route_start = token * expert_count_used;
            let ids = &resolved_ids[route_start..route_start + expert_count_used];
            let route_weights = &weights[route_start..route_start + expert_count_used];
            let values = match (&self.source, block) {
                (MoeSource::Legacy(source), MoeBlock::Legacy(block)) => block
                    .forward_from_source_with_route(source, token_hidden, &ids, route_weights)?,
                (MoeSource::Plain(source), MoeBlock::Plain(block)) => block
                    .forward_from_source_with_route(source, token_hidden, &ids, route_weights)?,
                (MoeSource::Q4(source), MoeBlock::Q4(block)) => block
                    .forward_from_source_with_route(source, token_hidden, &ids, route_weights)?,
                (MoeSource::K(source), MoeBlock::K(block)) => block
                    .forward_from_source_with_route(source, token_hidden, &ids, route_weights)?,
                (MoeSource::Iq(source), MoeBlock::Iq(block)) => block
                    .forward_from_source_with_route(source, token_hidden, &ids, route_weights)?,
                (MoeSource::Nvfp4(source), MoeBlock::Nvfp4(block)) => block
                    .forward_from_source_with_route(source, token_hidden, &ids, route_weights)?,
                (MoeSource::LowBit(source), MoeBlock::LowBit(block)) => block
                    .forward_from_source_with_route(source, token_hidden, &ids, route_weights)?,
                (MoeSource::Heterogeneous(source), MoeBlock::Heterogeneous(block)) => block
                    .forward_from_source_with_route(
                        source,
                        token_hidden,
                        &ids,
                        route_weights,
                        activation,
                    )?,
                _ => return Err(MoeRuntimeError::FormatMismatch),
            };
            output.extend(values);
        }
        let elapsed_ms = runtime_started.elapsed().as_secs_f64() * 1_000.0;
        self.runtime_diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .runtime_forward_ms += elapsed_ms;
        Ok(output)
    }

    pub fn forward_batch(
        &self,
        layer: u32,
        hidden: &[f32],
        tokens: usize,
    ) -> Result<Vec<f32>, MoeRuntimeError> {
        let block = self
            .blocks
            .get(layer as usize)
            .ok_or(MoeRuntimeError::MissingLayer {
                layer,
                layers: self.layers(),
            })?;
        let hidden_width = self.source.hidden();
        let expected = tokens
            .checked_mul(hidden_width)
            .ok_or(MoeRuntimeError::InvalidShape {
                values: hidden.len(),
                tokens,
                hidden: hidden_width,
            })?;
        if tokens == 0 || hidden.len() != expected {
            return Err(MoeRuntimeError::InvalidShape {
                values: hidden.len(),
                tokens,
                hidden: hidden_width,
            });
        }
        let mut output = Vec::with_capacity(hidden.len());
        for token_hidden in hidden.chunks_exact(hidden_width) {
            let values = match (&self.source, block) {
                (MoeSource::Legacy(source), MoeBlock::Legacy(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Plain(source), MoeBlock::Plain(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Q4(source), MoeBlock::Q4(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::K(source), MoeBlock::K(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Iq(source), MoeBlock::Iq(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Nvfp4(source), MoeBlock::Nvfp4(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::LowBit(source), MoeBlock::LowBit(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Heterogeneous(_), MoeBlock::Heterogeneous(_)) => {
                    return Err(MoeRuntimeError::Block(
                        "runtime heterogêneo exige a rota resolvida do llama.cpp".into(),
                    ))
                }
                _ => return Err(MoeRuntimeError::FormatMismatch),
            };
            output.extend(values);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::super::format_iq_quants::{tests::write_iq_source, IqQuantFormat};
    use super::super::format_k_quants::tests::write_k_source;
    use super::super::format_legacy_quants::{tests::write_legacy_source, LegacyQuantFormat};
    use super::super::format_low_bits::{tests::write_low_bit_source, LowBitFormat};
    use super::super::format_plain::{tests::write_plain_source, PlainFormat};
    use super::super::format_q4_0::tests::write_q4_source;
    use super::*;

    #[test]
    fn executor_cuda_recusa_relatorio_sem_compute_medido() {
        let error = require_cuda_compute_bytes(None).unwrap_err();
        assert!(error.to_string().contains("compute buffer CUDA medido"));
    }

    #[test]
    fn dtype_sem_cache_device_permanece_com_experts_nativos() {
        let plan = make_external_executor_plan(ExpertFormat::Q4_0, vec![2], 4, true, false);
        assert!(!plan.enabled());
        assert_eq!(plan.layers, vec![0, 0, 0, 0]);
        assert!(plan.reason.unwrap().contains("Q4_0"));
    }

    #[test]
    fn dtypes_heterogeneos_nao_externalizam_parcialmente() {
        let plan = make_external_executor_plan(
            ExpertFormat::Heterogeneous,
            vec![21, 22, 23],
            3,
            true,
            false,
        );
        assert!(!plan.enabled());
        assert_eq!(plan.layers, vec![0, 0, 0]);
        let reason = plan.reason.unwrap();
        assert!(reason.contains("21 (IQ3_S)"));
        assert!(reason.contains("22 (IQ2_S)"));
        assert!(reason.contains("23 (IQ4_XS)"));
    }

    #[test]
    fn dtype_com_ponte_device_externaliza_todas_as_camadas_do_plano() {
        let plan = make_external_executor_plan(ExpertFormat::Q4K, vec![12], 3, true, false);
        assert!(plan.enabled());
        assert_eq!(plan.layers, vec![1, 1, 1]);
        assert!(plan.reason.is_none());
    }

    #[test]
    fn runtime_mantem_forma_do_batch_de_hidden_states() {
        let path =
            std::env::temp_dir().join(format!("pom-ftk-moe-runtime-{}.gguf", std::process::id()));
        let (hidden, _) = write_q4_source(&path);
        let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0, Some(1_000_000)).unwrap();
        let output = runtime.forward_batch(0, &[1.0; 64], 2).unwrap();
        assert_eq!(output.len(), hidden * 2);
        assert!(runtime.forward_batch(1, &[1.0; 32], 1).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn runtime_carrega_e_executa_k_quants() {
        for format in [
            KQuantFormat::Q2K,
            KQuantFormat::Q3K,
            KQuantFormat::Q4K,
            KQuantFormat::Q5K,
            KQuantFormat::Q6K,
            KQuantFormat::Q8K,
        ] {
            let dtype = format.dtype();
            let expected = format.name();
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-k-runtime-{dtype}-{}.gguf",
                std::process::id()
            ));
            write_k_source(&path, dtype, format);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0, Some(1_000_000)).unwrap();
            assert_eq!(runtime.layers(), 1, "formato {expected}");
            let output = runtime.forward_batch(0, &[1.0; 512], 2).unwrap();
            assert_eq!(output.len(), 512, "formato {expected}");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn runtime_carrega_e_executa_iq_quants() {
        for format in [
            IqQuantFormat::Iq2XXS,
            IqQuantFormat::Iq2XS,
            IqQuantFormat::Iq3XXS,
            IqQuantFormat::Iq1S,
            IqQuantFormat::Iq4NL,
            IqQuantFormat::Iq3S,
            IqQuantFormat::Iq2S,
            IqQuantFormat::Iq4XS,
            IqQuantFormat::Iq1M,
        ] {
            let dtype = format.dtype();
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-iq-runtime-{dtype}-{}.gguf",
                std::process::id()
            ));
            write_iq_source(&path, dtype, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0, Some(1_000_000)).unwrap();
            assert_eq!(runtime.layers(), 1, "formato {}", format.name());
            let output = runtime.forward_batch(0, &[1.0; 512], 2).unwrap();
            assert_eq!(output.len(), 512, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn runtime_carrega_e_executa_f32_f16_e_bf16() {
        for format in [PlainFormat::F32, PlainFormat::F16, PlainFormat::Bf16] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-plain-runtime-{}-{}.gguf",
                format.name(),
                std::process::id()
            ));
            write_plain_source(&path, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0, Some(1_000_000)).unwrap();
            let output = runtime.forward_batch(0, &[1.0; 4], 2).unwrap();
            assert_eq!(output.len(), 4, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn runtime_carrega_e_executa_q4_1_q5_0_q5_1_e_q8_0() {
        for format in [
            LegacyQuantFormat::Q4_1,
            LegacyQuantFormat::Q5_0,
            LegacyQuantFormat::Q5_1,
            LegacyQuantFormat::Q8_0,
        ] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-legacy-runtime-{}-{}.gguf",
                format.name(),
                std::process::id()
            ));
            write_legacy_source(&path, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0, Some(1_000_000)).unwrap();
            let output = runtime.forward_batch(0, &[1.0; 64], 2).unwrap();
            assert_eq!(output.len(), 64, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn runtime_carrega_e_executa_mxfp4_tq1_tq2_q1_e_q2() {
        for format in [
            LowBitFormat::Mxfp4,
            LowBitFormat::Tq1_0,
            LowBitFormat::Tq2_0,
            LowBitFormat::Q1_0,
            LowBitFormat::Q2_0,
        ] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-low-bit-runtime-{}-{}.gguf",
                format.name(),
                std::process::id()
            ));
            write_low_bit_source(&path, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0, Some(1_000_000)).unwrap();
            let output = runtime.forward_batch(0, &[1.0; 512], 2).unwrap();
            assert_eq!(output.len(), 512, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }
}
