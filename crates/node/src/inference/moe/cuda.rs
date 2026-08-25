//! Ponte Rust -> CUDA para o executor NVFP4 dos experts.

#![cfg(feature = "cuda")]

use super::admission::MemoryAccount;
use super::cache::{DeviceAllocation, DeviceAllocator, DeviceExpertCache};
use super::format_k_quants::KQuantFormat;
use super::format_nvfp4::{Nvfp4Error, Nvfp4Expert};
use super::format_plain::{PlainExpert, PlainFormat};
use std::fmt;
use std::io;
use std::os::raw::c_void;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CudaMoeCallTiming {
    pub expert_calls: u64,
    pub kernel_launches: u64,
    pub sync_count: u64,
    pub payload_validation_ms: f64,
    pub alloc_ms: f64,
    pub h2d_ms: f64,
    pub h2d_bytes: u64,
    pub kernel_ms: f64,
    pub launch_ms: f64,
    pub sync_ms: f64,
    pub d2h_ms: f64,
    pub d2h_bytes: u64,
    pub free_ms: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Nvfp4CudaDiagnostics {
    pub calls: u64,
    pub kernel_launches: u64,
    pub sync_count: u64,
    pub payload_validation_ms: f64,
    pub alloc_ms: f64,
    pub h2d_ms: f64,
    pub h2d_bytes: u64,
    pub kernel_ms: f64,
    pub launch_ms: f64,
    pub sync_ms: f64,
    pub d2h_ms: f64,
    pub d2h_bytes: u64,
    pub free_ms: f64,
    pub host_batch_prepare_ms: f64,
    pub host_result_copy_ms: f64,
    /// Parede externa da chamada FFI, sem somar submedições internas.
    pub cuda_call_wall_ms: f64,
}

impl Nvfp4CudaDiagnostics {
    pub fn add_call(&mut self, call: CudaMoeCallTiming) {
        self.calls = self.calls.saturating_add(call.expert_calls);
        self.kernel_launches = self.kernel_launches.saturating_add(call.kernel_launches);
        self.sync_count = self.sync_count.saturating_add(call.sync_count);
        self.payload_validation_ms += call.payload_validation_ms;
        self.alloc_ms += call.alloc_ms;
        self.h2d_ms += call.h2d_ms;
        self.h2d_bytes = self.h2d_bytes.saturating_add(call.h2d_bytes);
        self.kernel_ms += call.kernel_ms;
        self.launch_ms += call.launch_ms;
        self.sync_ms += call.sync_ms;
        self.d2h_ms += call.d2h_ms;
        self.d2h_bytes = self.d2h_bytes.saturating_add(call.d2h_bytes);
        self.free_ms += call.free_ms;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CudaMoeError {
    Geometry(String),
    Payload(String),
    PayloadLength { expected: usize, actual: usize },
    BackendUnavailable(String),
    Cache(String),
    HiddenWidth { expected: usize, actual: usize },
    Launch(i32),
}

impl fmt::Display for CudaMoeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(reason) => write!(formatter, "geometria CUDA MoE inválida: {reason}"),
            Self::Payload(reason) => write!(formatter, "payload CUDA MoE inválido: {reason}"),
            Self::PayloadLength { expected, actual } => write!(
                formatter,
                "payload CUDA MoE tem {actual} bytes, mas a geometria exige {expected}"
            ),
            Self::BackendUnavailable(reason) => {
                write!(formatter, "backend CUDA MoE indisponível: {reason}")
            }
            Self::Cache(reason) => write!(formatter, "cache CUDA MoE recusou o expert: {reason}"),
            Self::HiddenWidth { expected, actual } => {
                write!(
                    formatter,
                    "hidden CUDA MoE tem {actual}, esperado {expected}"
                )
            }
            Self::Launch(code) => write!(formatter, "kernel CUDA MoE falhou (código {code})"),
        }
    }
}

impl std::error::Error for CudaMoeError {}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CudaHeterogeneousTensor {
    pub data: *const u8,
    pub bytes: usize,
    pub dtype: u32,
}

impl Default for CudaHeterogeneousTensor {
    fn default() -> Self {
        Self {
            data: std::ptr::null(),
            bytes: 0,
            dtype: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct CudaHeterogeneousExpert {
    pub gate_up: CudaHeterogeneousTensor,
    pub gate: CudaHeterogeneousTensor,
    pub up: CudaHeterogeneousTensor,
    pub down: CudaHeterogeneousTensor,
}

impl From<Nvfp4Error> for CudaMoeError {
    fn from(error: Nvfp4Error) -> Self {
        match error {
            Nvfp4Error::Geometry(reason) => Self::Geometry(reason.to_string()),
            Nvfp4Error::Payload(reason) => Self::Payload(reason.to_string()),
            Nvfp4Error::PayloadLength { expected, actual } => {
                Self::PayloadLength { expected, actual }
            }
            Nvfp4Error::BackendUnavailable(reason) => Self::BackendUnavailable(reason),
        }
    }
}

pub struct Nvfp4CudaExecutor {
    hidden: usize,
    intermediate: usize,
    profile: bool,
    resident_cache: Arc<DeviceExpertCache>,
    diagnostics: Mutex<Nvfp4CudaDiagnostics>,
}

/// Executor CUDA para payloads K-quant residentes no device.
///
/// O dtype continua sendo dado na chamada FFI. A ponte não conhece modelo,
/// arquitetura nem convenção de nomes do GGUF. Nesta primeira entrega os
/// decodificadores numericamente provados são Q4_K (12) e Q6_K (14); os demais
/// K-quants permanecem no caminho CPUReference até terem uma ponte própria.
pub struct KQuantCudaExecutor {
    hidden: usize,
    intermediate: usize,
    format: KQuantFormat,
    profile: bool,
    resident_cache: Arc<DeviceExpertCache>,
    diagnostics: Mutex<Nvfp4CudaDiagnostics>,
}

/// Executor CUDA para os payloads escalares F32, F16 e BF16.
///
/// O payload continua no formato produzido por `PlainSource`, incluindo os
/// dois comprimentos de seção. O dtype é validado na fronteira e enviado ao
/// decoder CUDA como dado, para que F32, F16 e BF16 compartilhem o mesmo
/// caminho de cache e de roteamento.
pub struct PlainCudaExecutor {
    hidden: usize,
    intermediate: usize,
    format: PlainFormat,
    profile: bool,
    resident_cache: Arc<DeviceExpertCache>,
    diagnostics: Mutex<Nvfp4CudaDiagnostics>,
}

impl PlainCudaExecutor {
    pub fn new_with_cache(
        format: PlainFormat,
        hidden: usize,
        intermediate: usize,
        resident_cache: Arc<DeviceExpertCache>,
    ) -> Result<Self, CudaMoeError> {
        if !matches!(
            format,
            PlainFormat::F32 | PlainFormat::F16 | PlainFormat::Bf16
        ) {
            return Err(CudaMoeError::BackendUnavailable(format!(
                "dtype GGML {} não tem ponte CUDA escalar",
                format.dtype()
            )));
        }
        Ok(Self {
            hidden,
            intermediate,
            format,
            profile: std::env::var("POM_MOE_CUDA_PROFILE")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
            resident_cache,
            diagnostics: Mutex::new(Nvfp4CudaDiagnostics::default()),
        })
    }

    pub fn resident_lookup(
        &self,
        key: &super::cache::ExpertKey,
    ) -> Option<super::cache::DeviceExpertLease> {
        self.resident_cache.lookup(key)
    }

    pub fn resident_admit(
        &self,
        key: super::cache::ExpertKey,
        payload: &[u8],
    ) -> Result<super::cache::DeviceExpertLease, CudaMoeError> {
        PlainExpert::from_payload(self.format, self.hidden, self.intermediate, payload)
            .map_err(|error| CudaMoeError::Payload(error.to_string()))?;
        self.resident_cache
            .admit(key, payload)
            .map_err(|error| CudaMoeError::Cache(error.to_string()))
    }

    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.resident_cache.stats()
    }

    pub fn run_route_batch_resident(
        &self,
        payloads: &[super::cache::DeviceExpertLease],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        hidden: &[f32],
        tokens: usize,
    ) -> Result<Vec<f32>, CudaMoeError> {
        if payloads.is_empty()
            || payloads.len() != token_ids.len()
            || payloads.len() != weights.len()
            || tokens == 0
            || token_offsets.len() != tokens + 1
            || token_offsets.first() != Some(&0)
            || token_offsets.last() != Some(&payloads.len())
            || token_offsets.windows(2).any(|pair| pair[0] > pair[1])
            || token_ids.iter().any(|&token| token as usize >= tokens)
        {
            return Err(CudaMoeError::Payload(
                "batch CUDA escalar tem rotas inválidas".into(),
            ));
        }
        if hidden.len() != tokens.saturating_mul(self.hidden) {
            return Err(CudaMoeError::HiddenWidth {
                expected: tokens.saturating_mul(self.hidden),
                actual: hidden.len(),
            });
        }
        if weights.iter().any(|weight| !weight.is_finite()) {
            return Err(CudaMoeError::Payload(
                "batch CUDA escalar tem peso de rota não finito".into(),
            ));
        }

        let handles = payloads
            .iter()
            .map(|payload| payload.handle() as *const u8)
            .collect::<Vec<_>>();
        let mut output = vec![0.0f32; hidden.len()];
        let mut timing = CudaMoeCallTiming {
            expert_calls: payloads.len() as u64,
            ..CudaMoeCallTiming::default()
        };
        let cuda_started = Instant::now();
        let status = unsafe {
            pom_moe_plain_forward_route_batch_resident(
                self.format.dtype(),
                handles.as_ptr(),
                payloads.len(),
                token_ids.as_ptr(),
                weights.as_ptr(),
                token_offsets.as_ptr(),
                token_offsets.len(),
                tokens,
                hidden.as_ptr(),
                hidden.len(),
                output.as_mut_ptr(),
                output.len(),
                self.hidden,
                self.intermediate,
                if self.profile {
                    &mut timing
                } else {
                    std::ptr::null_mut()
                },
            )
        };
        let cuda_call_wall_ms = cuda_started.elapsed().as_secs_f64() * 1_000.0;
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            let mut diagnostics = self
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diagnostics.add_call(timing);
            diagnostics.cuda_call_wall_ms += cuda_call_wall_ms;
        }
        Ok(output)
    }

    pub fn diagnostics(&self) -> Nvfp4CudaDiagnostics {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_owned()
    }
}

impl KQuantCudaExecutor {
    pub fn new_with_cache(
        format: KQuantFormat,
        hidden: usize,
        intermediate: usize,
        resident_cache: Arc<DeviceExpertCache>,
    ) -> Result<Self, CudaMoeError> {
        if !format.cuda_supported() {
            return Err(CudaMoeError::BackendUnavailable(format!(
                "dtype GGML {} ({}) ainda não tem ponte CUDA K-quant",
                format.dtype(),
                format.name()
            )));
        }
        Ok(Self {
            hidden,
            intermediate,
            format,
            profile: std::env::var("POM_MOE_CUDA_PROFILE")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
            resident_cache,
            diagnostics: Mutex::new(Nvfp4CudaDiagnostics::default()),
        })
    }

    pub fn resident_lookup(
        &self,
        key: &super::cache::ExpertKey,
    ) -> Option<super::cache::DeviceExpertLease> {
        self.resident_cache.lookup(key)
    }

    pub fn resident_admit(
        &self,
        key: super::cache::ExpertKey,
        payload: &[u8],
    ) -> Result<super::cache::DeviceExpertLease, CudaMoeError> {
        self.resident_cache
            .admit(key, payload)
            .map_err(|error| CudaMoeError::Cache(error.to_string()))
    }

    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.resident_cache.stats()
    }

    pub fn run_route_batch_resident(
        &self,
        payloads: &[super::cache::DeviceExpertLease],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        hidden: &[f32],
        tokens: usize,
    ) -> Result<Vec<f32>, CudaMoeError> {
        if payloads.is_empty()
            || payloads.len() != token_ids.len()
            || payloads.len() != weights.len()
            || tokens == 0
            || token_offsets.len() != tokens + 1
            || token_offsets.first() != Some(&0)
            || token_offsets.last() != Some(&payloads.len())
            || token_offsets.windows(2).any(|pair| pair[0] > pair[1])
            || token_ids.iter().any(|&token| token as usize >= tokens)
        {
            return Err(CudaMoeError::Payload(
                "batch CUDA K-quant tem rotas inválidas".into(),
            ));
        }
        if hidden.len() != tokens.saturating_mul(self.hidden) {
            return Err(CudaMoeError::HiddenWidth {
                expected: tokens.saturating_mul(self.hidden),
                actual: hidden.len(),
            });
        }
        if weights.iter().any(|weight| !weight.is_finite()) {
            return Err(CudaMoeError::Payload(
                "batch CUDA K-quant tem peso de rota não finito".into(),
            ));
        }

        let handles = payloads
            .iter()
            .map(|payload| payload.handle() as *const u8)
            .collect::<Vec<_>>();
        let mut output = vec![0.0f32; hidden.len()];
        let mut timing = CudaMoeCallTiming {
            expert_calls: payloads.len() as u64,
            ..CudaMoeCallTiming::default()
        };
        let cuda_started = Instant::now();
        let status = unsafe {
            pom_moe_kquant_forward_route_batch_resident(
                self.format.dtype(),
                handles.as_ptr(),
                payloads.len(),
                token_ids.as_ptr(),
                weights.as_ptr(),
                token_offsets.as_ptr(),
                token_offsets.len(),
                tokens,
                hidden.as_ptr(),
                hidden.len(),
                output.as_mut_ptr(),
                output.len(),
                self.hidden,
                self.intermediate,
                if self.profile {
                    &mut timing
                } else {
                    std::ptr::null_mut()
                },
            )
        };
        let cuda_call_wall_ms = cuda_started.elapsed().as_secs_f64() * 1_000.0;
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            let mut diagnostics = self
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diagnostics.add_call(timing);
            diagnostics.cuda_call_wall_ms += cuda_call_wall_ms;
        }
        Ok(output)
    }

    pub fn diagnostics(&self) -> Nvfp4CudaDiagnostics {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_owned()
    }
}

/// Executor CUDA para os IQ usados nos modelos UD priorizados.
///
/// O formato permanece dado na chamada FFI. A ponte cobre IQ3_S (21), IQ2_S
/// (22) e IQ4_XS (23); um IQ diferente é recusado antes de chegar ao kernel.
pub struct IqQuantCudaExecutor {
    hidden: usize,
    intermediate: usize,
    format: super::format_iq_quants::IqQuantFormat,
    profile: bool,
    resident_cache: Arc<DeviceExpertCache>,
    diagnostics: Mutex<Nvfp4CudaDiagnostics>,
}

impl IqQuantCudaExecutor {
    pub fn new_with_cache(
        format: super::format_iq_quants::IqQuantFormat,
        hidden: usize,
        intermediate: usize,
        resident_cache: Arc<DeviceExpertCache>,
    ) -> Result<Self, CudaMoeError> {
        if !format.cuda_supported() {
            return Err(CudaMoeError::BackendUnavailable(format!(
                "dtype GGML {} ({}) ainda não tem ponte CUDA IQ-quant",
                format.dtype(),
                format.name()
            )));
        }
        Ok(Self::new_unchecked(
            format,
            hidden,
            intermediate,
            resident_cache,
        ))
    }

    #[cfg(test)]
    fn new_for_differential_test(
        format: super::format_iq_quants::IqQuantFormat,
        hidden: usize,
        intermediate: usize,
        resident_cache: Arc<DeviceExpertCache>,
    ) -> Self {
        Self::new_unchecked(format, hidden, intermediate, resident_cache)
    }

    fn new_unchecked(
        format: super::format_iq_quants::IqQuantFormat,
        hidden: usize,
        intermediate: usize,
        resident_cache: Arc<DeviceExpertCache>,
    ) -> Self {
        Self {
            hidden,
            intermediate,
            format,
            profile: std::env::var("POM_MOE_CUDA_PROFILE")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
            resident_cache,
            diagnostics: Mutex::new(Nvfp4CudaDiagnostics::default()),
        }
    }

    pub fn resident_lookup(
        &self,
        key: &super::cache::ExpertKey,
    ) -> Option<super::cache::DeviceExpertLease> {
        self.resident_cache.lookup(key)
    }

    pub fn resident_admit(
        &self,
        key: super::cache::ExpertKey,
        payload: &[u8],
    ) -> Result<super::cache::DeviceExpertLease, CudaMoeError> {
        self.resident_cache
            .admit(key, payload)
            .map_err(|error| CudaMoeError::Cache(error.to_string()))
    }

    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.resident_cache.stats()
    }

    pub fn run_route_batch_resident(
        &self,
        payloads: &[super::cache::DeviceExpertLease],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        hidden: &[f32],
        tokens: usize,
    ) -> Result<Vec<f32>, CudaMoeError> {
        if payloads.is_empty()
            || payloads.len() != token_ids.len()
            || payloads.len() != weights.len()
            || tokens == 0
            || token_offsets.len() != tokens + 1
            || token_offsets.first() != Some(&0)
            || token_offsets.last() != Some(&payloads.len())
            || token_offsets.windows(2).any(|pair| pair[0] > pair[1])
            || token_ids.iter().any(|&token| token as usize >= tokens)
        {
            return Err(CudaMoeError::Payload(
                "batch CUDA IQ-quant tem rotas inválidas".into(),
            ));
        }
        if hidden.len() != tokens.saturating_mul(self.hidden) {
            return Err(CudaMoeError::HiddenWidth {
                expected: tokens.saturating_mul(self.hidden),
                actual: hidden.len(),
            });
        }
        if weights.iter().any(|weight| !weight.is_finite()) {
            return Err(CudaMoeError::Payload(
                "batch CUDA IQ-quant tem peso de rota não finito".into(),
            ));
        }

        let handles = payloads
            .iter()
            .map(|payload| payload.handle() as *const u8)
            .collect::<Vec<_>>();
        let mut output = vec![0.0f32; hidden.len()];
        let mut timing = CudaMoeCallTiming {
            expert_calls: payloads.len() as u64,
            ..CudaMoeCallTiming::default()
        };
        let cuda_started = Instant::now();
        let status = unsafe {
            pom_moe_iquant_forward_route_batch_resident(
                self.format.dtype(),
                handles.as_ptr(),
                payloads.len(),
                token_ids.as_ptr(),
                weights.as_ptr(),
                token_offsets.as_ptr(),
                token_offsets.len(),
                tokens,
                hidden.as_ptr(),
                hidden.len(),
                output.as_mut_ptr(),
                output.len(),
                self.hidden,
                self.intermediate,
                if self.profile {
                    &mut timing
                } else {
                    std::ptr::null_mut()
                },
            )
        };
        let cuda_call_wall_ms = cuda_started.elapsed().as_secs_f64() * 1_000.0;
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            let mut diagnostics = self
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diagnostics.add_call(timing);
            diagnostics.cuda_call_wall_ms += cuda_call_wall_ms;
        }
        Ok(output)
    }

    pub fn diagnostics(&self) -> Nvfp4CudaDiagnostics {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_owned()
    }
}

/// Executor CUDA para uma rota cujos tensores podem usar IQ3_S, IQ2_S ou
/// IQ4_XS independentemente por papel. Os descritores carregam ponteiro,
/// tamanho e dtype de cada tensor; o cache residente mantém os bytes no
/// device entre tokens.
pub struct HeterogeneousIqCudaExecutor {
    hidden: usize,
    intermediate: usize,
    profile: bool,
    resident_cache: Arc<DeviceExpertCache>,
    diagnostics: Mutex<Nvfp4CudaDiagnostics>,
}

impl HeterogeneousIqCudaExecutor {
    pub fn new_with_cache(
        hidden: usize,
        intermediate: usize,
        resident_cache: Arc<DeviceExpertCache>,
    ) -> Self {
        Self {
            hidden,
            intermediate,
            profile: std::env::var("POM_MOE_CUDA_PROFILE")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
            resident_cache,
            diagnostics: Mutex::new(Nvfp4CudaDiagnostics::default()),
        }
    }

    pub fn resident_lookup(
        &self,
        key: &super::cache::ExpertKey,
    ) -> Option<super::cache::DeviceExpertLease> {
        self.resident_cache.lookup(key)
    }

    pub fn resident_admit(
        &self,
        key: super::cache::ExpertKey,
        payload: &[u8],
    ) -> Result<super::cache::DeviceExpertLease, CudaMoeError> {
        self.resident_cache
            .admit(key, payload)
            .map_err(|error| CudaMoeError::Cache(error.to_string()))
    }

    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.resident_cache.stats()
    }

    pub fn run_route_batch_resident(
        &self,
        experts: &[CudaHeterogeneousExpert],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        hidden: &[f32],
        tokens: usize,
        activation: i32,
    ) -> Result<Vec<f32>, CudaMoeError> {
        if experts.is_empty()
            || experts.len() != token_ids.len()
            || experts.len() != weights.len()
            || tokens == 0
            || token_offsets.len() != tokens + 1
            || token_offsets.first() != Some(&0)
            || token_offsets.last() != Some(&experts.len())
            || token_offsets.windows(2).any(|pair| pair[0] > pair[1])
            || token_ids.iter().any(|&token| token as usize >= tokens)
        {
            return Err(CudaMoeError::Payload(
                "batch CUDA heterogêneo tem rotas inválidas".into(),
            ));
        }
        if !matches!(activation, 1 | 2) {
            return Err(CudaMoeError::Payload(format!(
                "ativação heterogênea {activation} não suportada pelo kernel CUDA"
            )));
        }
        if hidden.len() != tokens.saturating_mul(self.hidden) {
            return Err(CudaMoeError::HiddenWidth {
                expected: tokens.saturating_mul(self.hidden),
                actual: hidden.len(),
            });
        }
        if weights.iter().any(|weight| !weight.is_finite()) {
            return Err(CudaMoeError::Payload(
                "batch CUDA heterogêneo tem peso de rota não finito".into(),
            ));
        }

        for (index, expert) in experts.iter().enumerate() {
            validate_heterogeneous_tensor(
                expert.down,
                self.hidden,
                self.intermediate,
                &format!("rota {index} down"),
            )?;
            if !expert.gate_up.data.is_null() {
                if !expert.gate.data.is_null() || !expert.up.data.is_null() {
                    return Err(CudaMoeError::Payload(format!(
                        "rota {index} mistura gate_up fundido com gate/up separados"
                    )));
                }
                validate_heterogeneous_tensor(
                    expert.gate_up,
                    2 * self.intermediate,
                    self.hidden,
                    &format!("rota {index} gate_up"),
                )?;
            } else {
                validate_heterogeneous_tensor(
                    expert.gate,
                    self.intermediate,
                    self.hidden,
                    &format!("rota {index} gate"),
                )?;
                validate_heterogeneous_tensor(
                    expert.up,
                    self.intermediate,
                    self.hidden,
                    &format!("rota {index} up"),
                )?;
            }
        }

        let mut output = vec![0.0f32; hidden.len()];
        let mut timing = CudaMoeCallTiming {
            expert_calls: experts.len() as u64,
            ..CudaMoeCallTiming::default()
        };
        let cuda_started = Instant::now();
        let status = unsafe {
            pom_moe_heterogeneous_iq_forward_route_batch_resident(
                experts.as_ptr(),
                experts.len(),
                token_ids.as_ptr(),
                weights.as_ptr(),
                token_offsets.as_ptr(),
                token_offsets.len(),
                tokens,
                hidden.as_ptr(),
                hidden.len(),
                output.as_mut_ptr(),
                output.len(),
                self.hidden,
                self.intermediate,
                activation,
                if self.profile {
                    &mut timing
                } else {
                    std::ptr::null_mut()
                },
            )
        };
        let cuda_call_wall_ms = cuda_started.elapsed().as_secs_f64() * 1_000.0;
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            let mut diagnostics = self
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diagnostics.add_call(timing);
            diagnostics.cuda_call_wall_ms += cuda_call_wall_ms;
        }
        Ok(output)
    }

    pub fn diagnostics(&self) -> Nvfp4CudaDiagnostics {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_owned()
    }
}

fn validate_heterogeneous_tensor(
    tensor: CudaHeterogeneousTensor,
    rows: usize,
    columns: usize,
    label: &str,
) -> Result<(), CudaMoeError> {
    let row_bytes = match tensor.dtype {
        21 => 110,
        22 => 82,
        23 => 136,
        dtype => {
            return Err(CudaMoeError::BackendUnavailable(format!(
                "dtype GGML {dtype} em {label} não tem ponte CUDA heterogênea"
            )))
        }
    };
    if tensor.data.is_null() || columns == 0 || !columns.is_multiple_of(256) {
        return Err(CudaMoeError::Geometry(format!(
            "{label} tem ponte nula ou colunas que não fecham blocos de 256"
        )));
    }
    let expected = rows
        .checked_mul(columns / 256)
        .and_then(|blocks| blocks.checked_mul(row_bytes))
        .ok_or_else(|| CudaMoeError::Geometry(format!("overflow na geometria de {label}")))?;
    if tensor.bytes != expected {
        return Err(CudaMoeError::PayloadLength {
            expected,
            actual: tensor.bytes,
        });
    }
    Ok(())
}

impl Nvfp4CudaExecutor {
    #[cfg(test)]
    pub fn new(
        hidden: usize,
        intermediate: usize,
        compute_bytes: u64,
    ) -> Result<Self, CudaMoeError> {
        debug_assert!(Nvfp4Expert::packed_bytes(hidden, intermediate).is_ok());
        let resident_cache = measured_device_cache(compute_bytes)?;
        Self::new_with_cache(hidden, intermediate, resident_cache)
    }

    pub fn new_with_cache(
        hidden: usize,
        intermediate: usize,
        resident_cache: Arc<DeviceExpertCache>,
    ) -> Result<Self, CudaMoeError> {
        debug_assert!(Nvfp4Expert::packed_bytes(hidden, intermediate).is_ok());
        Ok(Self {
            hidden,
            intermediate,
            profile: std::env::var("POM_MOE_CUDA_PROFILE")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
            resident_cache,
            diagnostics: Mutex::new(Nvfp4CudaDiagnostics::default()),
        })
    }

    pub fn resident_lookup(
        &self,
        key: &super::cache::ExpertKey,
    ) -> Option<super::cache::DeviceExpertLease> {
        self.resident_cache.lookup(key)
    }

    pub fn resident_admit(
        &self,
        key: super::cache::ExpertKey,
        payload: &[u8],
    ) -> Result<super::cache::DeviceExpertLease, CudaMoeError> {
        self.resident_cache
            .admit(key, payload)
            .map_err(|error| CudaMoeError::Cache(error.to_string()))
    }

    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.resident_cache.stats()
    }

    pub fn run_resident(
        &self,
        payload: &super::cache::DeviceExpertLease,
        hidden: &[f32],
    ) -> Result<Vec<f32>, CudaMoeError> {
        self.run_batch_resident(std::slice::from_ref(payload), hidden)
            .map(|mut outputs| outputs.remove(0))
    }

    pub fn run_batch_resident(
        &self,
        payloads: &[super::cache::DeviceExpertLease],
        hidden: &[f32],
    ) -> Result<Vec<Vec<f32>>, CudaMoeError> {
        if payloads.is_empty() {
            return Err(CudaMoeError::Payload(
                "batch residente de experts vazio".into(),
            ));
        }
        if hidden.len() != self.hidden {
            return Err(CudaMoeError::HiddenWidth {
                expected: self.hidden,
                actual: hidden.len(),
            });
        }
        let handles = payloads
            .iter()
            .map(|payload| payload.handle() as *const u8)
            .collect::<Vec<_>>();
        let mut flat_output = vec![0.0f32; self.hidden * payloads.len()];
        let mut timing = CudaMoeCallTiming {
            expert_calls: payloads.len() as u64,
            ..CudaMoeCallTiming::default()
        };
        let cuda_started = Instant::now();
        let status = unsafe {
            pom_moe_nvfp4_forward_batch_resident(
                handles.as_ptr(),
                payloads.len(),
                hidden.as_ptr(),
                hidden.len(),
                flat_output.as_mut_ptr(),
                flat_output.len(),
                self.hidden,
                self.intermediate,
                if self.profile {
                    &mut timing
                } else {
                    std::ptr::null_mut()
                },
            )
        };
        let cuda_call_wall_ms = cuda_started.elapsed().as_secs_f64() * 1_000.0;
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            let mut diagnostics = self
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diagnostics.add_call(timing);
            diagnostics.cuda_call_wall_ms += cuda_call_wall_ms;
        }
        Ok(flat_output
            .chunks_exact(self.hidden)
            .map(ToOwned::to_owned)
            .collect())
    }

    pub fn run_route_batch_resident(
        &self,
        payloads: &[super::cache::DeviceExpertLease],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        hidden: &[f32],
        tokens: usize,
    ) -> Result<Vec<f32>, CudaMoeError> {
        if payloads.is_empty() {
            return Err(CudaMoeError::Payload(
                "batch residente de rotas vazio".into(),
            ));
        }
        if payloads.len() != token_ids.len() || payloads.len() != weights.len() {
            return Err(CudaMoeError::Payload(
                "batch residente de rotas tem descritores com tamanhos diferentes".into(),
            ));
        }
        if tokens == 0 || token_offsets.len() != tokens + 1 {
            return Err(CudaMoeError::Payload(
                "offsets de rota não correspondem ao número de tokens".into(),
            ));
        }
        if token_offsets.first() != Some(&0)
            || token_offsets.last() != Some(&payloads.len())
            || token_offsets.windows(2).any(|pair| pair[0] > pair[1])
            || token_ids.iter().any(|&token| token as usize >= tokens)
        {
            return Err(CudaMoeError::Payload(
                "offsets ou ids de token da rota são inválidos".into(),
            ));
        }
        if hidden.len() != tokens.saturating_mul(self.hidden) {
            return Err(CudaMoeError::HiddenWidth {
                expected: tokens.saturating_mul(self.hidden),
                actual: hidden.len(),
            });
        }

        let handles = payloads
            .iter()
            .map(|payload| payload.handle() as *const u8)
            .collect::<Vec<_>>();
        let mut output = vec![0.0f32; hidden.len()];
        let mut timing = CudaMoeCallTiming {
            expert_calls: payloads.len() as u64,
            ..CudaMoeCallTiming::default()
        };
        let cuda_started = Instant::now();
        let status = unsafe {
            pom_moe_nvfp4_forward_route_batch_resident(
                handles.as_ptr(),
                payloads.len(),
                token_ids.as_ptr(),
                weights.as_ptr(),
                token_offsets.as_ptr(),
                token_offsets.len(),
                tokens,
                hidden.as_ptr(),
                hidden.len(),
                output.as_mut_ptr(),
                output.len(),
                self.hidden,
                self.intermediate,
                if self.profile {
                    &mut timing
                } else {
                    std::ptr::null_mut()
                },
            )
        };
        let cuda_call_wall_ms = cuda_started.elapsed().as_secs_f64() * 1_000.0;
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            let mut diagnostics = self
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diagnostics.add_call(timing);
            diagnostics.cuda_call_wall_ms += cuda_call_wall_ms;
        }
        Ok(output)
    }

    pub fn diagnostics(&self) -> Nvfp4CudaDiagnostics {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_owned()
    }
}

struct CudaDeviceAllocation {
    handle: usize,
    bytes: usize,
}

// The CUDA allocation is independent of the Rust thread that owns its lease;
// the CUDA runtime accepts freeing it from another host thread after the
// kernel call has completed.
unsafe impl Send for CudaDeviceAllocation {}
unsafe impl Sync for CudaDeviceAllocation {}

impl DeviceAllocation for CudaDeviceAllocation {
    fn bytes(&self) -> usize {
        self.bytes
    }

    fn handle(&self) -> usize {
        self.handle
    }
}

impl Drop for CudaDeviceAllocation {
    fn drop(&mut self) {
        unsafe { pom_moe_device_free(self.handle as *mut c_void) };
    }
}

struct CudaDeviceAllocator;

impl DeviceAllocator for CudaDeviceAllocator {
    fn device_name(&self) -> &str {
        "CUDA device 0"
    }

    fn free_bytes(&self) -> io::Result<u64> {
        let mut free = 0u64;
        let status = unsafe { pom_moe_device_free_bytes(&mut free) };
        if status == 0 {
            Ok(free)
        } else {
            Err(io::Error::other(format!(
                "cudaMemGetInfo retornou {status}"
            )))
        }
    }

    fn upload(&self, payload: &[u8]) -> Result<Box<dyn DeviceAllocation>, String> {
        let mut handle = std::ptr::null_mut();
        let status = unsafe { pom_moe_device_upload(payload.as_ptr(), payload.len(), &mut handle) };
        if status != 0 {
            return Err(format!("upload CUDA retornou {status}"));
        }
        if handle.is_null() {
            return Err("upload CUDA retornou handle nulo".into());
        }
        Ok(Box::new(CudaDeviceAllocation {
            handle: handle as usize,
            bytes: payload.len(),
        }))
    }
}

fn measured_device_cache(compute_bytes: u64) -> Result<Arc<DeviceExpertCache>, CudaMoeError> {
    let allocator: Arc<dyn DeviceAllocator> = Arc::new(CudaDeviceAllocator);
    let baseline_free_bytes = allocator
        .free_bytes()
        .map_err(|error| CudaMoeError::BackendUnavailable(error.to_string()))?;
    let account =
        MemoryAccount::for_device_cache(baseline_free_bytes, compute_bytes).ok_or_else(|| {
            CudaMoeError::BackendUnavailable(format!(
                "compute buffer CUDA medido inválido: {compute_bytes} bytes"
            ))
        })?;
    Ok(DeviceExpertCache::new(account, allocator))
}

pub fn create_device_cache(compute_bytes: u64) -> Result<Arc<DeviceExpertCache>, CudaMoeError> {
    measured_device_cache(compute_bytes)
}

#[cfg(feature = "cuda")]
unsafe extern "C" {
    fn pom_moe_nvfp4_forward_batch_resident(
        payloads: *const *const u8,
        payload_count: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;

    fn pom_moe_nvfp4_forward_route_batch_resident(
        payloads: *const *const u8,
        payload_count: usize,
        token_ids: *const u32,
        weights: *const f32,
        token_offsets: *const usize,
        token_offsets_len: usize,
        token_count: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;

    fn pom_moe_kquant_forward_route_batch_resident(
        dtype: u32,
        payloads: *const *const u8,
        payload_count: usize,
        token_ids: *const u32,
        weights: *const f32,
        token_offsets: *const usize,
        token_offsets_len: usize,
        token_count: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;

    fn pom_moe_plain_forward_route_batch_resident(
        dtype: u32,
        payloads: *const *const u8,
        payload_count: usize,
        token_ids: *const u32,
        weights: *const f32,
        token_offsets: *const usize,
        token_offsets_len: usize,
        token_count: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;

    fn pom_moe_iquant_forward_route_batch_resident(
        dtype: u32,
        payloads: *const *const u8,
        payload_count: usize,
        token_ids: *const u32,
        weights: *const f32,
        token_offsets: *const usize,
        token_offsets_len: usize,
        token_count: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;

    fn pom_moe_heterogeneous_iq_forward_route_batch_resident(
        experts: *const CudaHeterogeneousExpert,
        expert_count: usize,
        token_ids: *const u32,
        weights: *const f32,
        token_offsets: *const usize,
        token_offsets_len: usize,
        token_count: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
        activation: i32,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;

    fn pom_moe_device_free_bytes(free_bytes: *mut u64) -> i32;
    fn pom_moe_device_upload(payload: *const u8, payload_len: usize, out: *mut *mut c_void) -> i32;
    fn pom_moe_device_free(handle: *mut c_void);

    fn pom_moe_iquant_trace_single_resident(
        dtype: u32,
        payload: *const u8,
        hidden: *const f32,
        hidden_len: usize,
        gate_up: *mut f32,
        gate_up_len: usize,
        activated: *mut f32,
        activated_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::super::format_iq_quants::{dequantize_row, row_bytes, IqQuantExpert, IqQuantFormat};
    use super::super::format_k_quants::{KQuantExpert, KQuantFormat};
    use super::super::format_nvfp4::{Nvfp4Expert, Nvfp4Source};
    use super::super::format_plain::{PlainExpert, PlainFormat};
    use super::super::format_q4_0::parse_tensor_table;
    use super::*;
    use memmap2::Mmap;

    fn packed_rows(rows: usize, columns: usize, nibble: u8) -> Vec<u8> {
        let row_size = columns / 64 * 36;
        let mut output = vec![0u8; rows * row_size];
        for row in output.chunks_exact_mut(row_size) {
            for block in row.chunks_exact_mut(36) {
                block[..4].fill(0x38);
                block[4..].fill(nibble | (nibble << 4));
            }
        }
        output
    }

    fn payload(hidden: usize, intermediate: usize) -> Vec<u8> {
        payload_with_nibble(hidden, intermediate, 0x22)
    }

    fn payload_with_nibble(hidden: usize, intermediate: usize, nibble: u8) -> Vec<u8> {
        let gate_up = packed_rows(2 * intermediate, hidden, nibble);
        let down = packed_rows(hidden, intermediate, nibble);
        let mut payload = Vec::with_capacity(32 + gate_up.len() + down.len());
        payload.extend_from_slice(&(gate_up.len() as u32).to_le_bytes());
        payload.extend_from_slice(&(down.len() as u32).to_le_bytes());
        for _ in 0..6 {
            payload.extend_from_slice(&1.0f32.to_le_bytes());
        }
        payload.extend_from_slice(&gate_up);
        payload.extend_from_slice(&down);
        payload
    }

    fn k_payload(format: KQuantFormat, hidden: usize, intermediate: usize) -> Vec<u8> {
        let block_bytes = match format {
            KQuantFormat::Q4K => 144,
            KQuantFormat::Q6K => 210,
            _ => panic!("fixture CUDA K-quant não suportada"),
        };
        let row_bytes = |columns: usize| columns / 256 * block_bytes;
        let matrix = |rows: usize, columns: usize| {
            let mut bytes = vec![0u8; rows * row_bytes(columns)];
            for block in bytes.chunks_exact_mut(block_bytes) {
                block[..2].copy_from_slice(&0x3c00u16.to_le_bytes());
                if matches!(format, KQuantFormat::Q4K) {
                    block[4..16].fill(1);
                    block[16..].fill(0x22);
                } else {
                    block[194..210].fill(1);
                    block[2..194].fill(0);
                }
            }
            bytes
        };
        let gate_up = matrix(2 * intermediate, hidden);
        let down = matrix(hidden, intermediate);
        let mut payload = Vec::with_capacity(8 + gate_up.len() + down.len());
        payload.extend_from_slice(&(gate_up.len() as u32).to_le_bytes());
        payload.extend_from_slice(&(down.len() as u32).to_le_bytes());
        payload.extend_from_slice(&gate_up);
        payload.extend_from_slice(&down);
        payload
    }

    fn plain_payload(format: PlainFormat, hidden: usize, intermediate: usize) -> Vec<u8> {
        let element_bytes = match format {
            PlainFormat::F32 => 4,
            PlainFormat::F16 | PlainFormat::Bf16 => 2,
        };
        let encode = |value: f32, output: &mut Vec<u8>| match format {
            PlainFormat::F32 => output.extend_from_slice(&value.to_le_bytes()),
            PlainFormat::F16 => {
                output.extend_from_slice(&half::f16::from_f32(value).to_bits().to_le_bytes())
            }
            PlainFormat::Bf16 => {
                output.extend_from_slice(&((value.to_bits() >> 16) as u16).to_le_bytes())
            }
        };
        let matrix = |rows: usize, columns: usize| {
            let mut output = Vec::with_capacity(rows * columns * element_bytes);
            for row in 0..rows {
                for column in 0..columns {
                    let value = ((row.wrapping_mul(3) + column) % 9) as f32 * 0.0625 - 0.25;
                    encode(value, &mut output);
                }
            }
            output
        };
        let gate_up = matrix(2 * intermediate, hidden);
        let down = matrix(hidden, intermediate);
        let mut payload = Vec::with_capacity(8 + gate_up.len() + down.len());
        payload.extend_from_slice(&(gate_up.len() as u32).to_le_bytes());
        payload.extend_from_slice(&(down.len() as u32).to_le_bytes());
        payload.extend_from_slice(&gate_up);
        payload.extend_from_slice(&down);
        payload
    }

    fn iq_payload(format: IqQuantFormat, hidden: usize, intermediate: usize) -> Vec<u8> {
        let block_bytes = match format {
            IqQuantFormat::Iq3S => 110,
            IqQuantFormat::Iq2S => 82,
            IqQuantFormat::Iq4XS => 136,
            _ => panic!("fixture CUDA IQ não suportada"),
        };
        let row_bytes = |columns: usize| columns / 256 * block_bytes;
        let matrix = |rows: usize, columns: usize| {
            let mut bytes = vec![0u8; rows * row_bytes(columns)];
            for block in bytes.chunks_exact_mut(block_bytes) {
                block[..2].copy_from_slice(&0x3c00u16.to_le_bytes());
                match format {
                    IqQuantFormat::Iq3S => {
                        block[2..66].iter_mut().enumerate().for_each(|(i, value)| {
                            *value = (i as u8).wrapping_mul(17) & 0x7f;
                        });
                        block[66..74].fill(0x55);
                        block[74..106].fill(0x33);
                        block[106..110].fill(0x21);
                    }
                    IqQuantFormat::Iq2S => {
                        block[2..66].iter_mut().enumerate().for_each(|(i, value)| {
                            *value = (i as u8).wrapping_mul(29);
                        });
                        block[66..74].fill(0x55);
                        block[74..82].fill(0x21);
                    }
                    IqQuantFormat::Iq4XS => {
                        block[2..4].copy_from_slice(&0x02aau16.to_le_bytes());
                        block[4..8].fill(0x21);
                        block[8..136].iter_mut().enumerate().for_each(|(i, value)| {
                            *value = (i as u8).wrapping_mul(13);
                        });
                    }
                    _ => unreachable!(),
                }
            }
            bytes
        };
        let gate_up = matrix(2 * intermediate, hidden);
        let down = matrix(hidden, intermediate);
        let mut payload = Vec::with_capacity(8 + gate_up.len() + down.len());
        payload.extend_from_slice(&(gate_up.len() as u32).to_le_bytes());
        payload.extend_from_slice(&(down.len() as u32).to_le_bytes());
        payload.extend_from_slice(&gate_up);
        payload.extend_from_slice(&down);
        payload
    }

    fn split_iq_payload(
        format: IqQuantFormat,
        hidden: usize,
        intermediate: usize,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let payload = iq_payload(format, hidden, intermediate);
        let gate_up_len = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        let down_len = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
        let gate_up = &payload[8..8 + gate_up_len];
        let gate_bytes = gate_up.len() / 2;
        assert_eq!(gate_bytes * 2, gate_up.len());
        assert_eq!(down_len, payload.len() - 8 - gate_up_len);
        (
            gate_up[..gate_bytes].to_vec(),
            gate_up[gate_bytes..].to_vec(),
            payload[8 + gate_up_len..].to_vec(),
        )
    }

    fn iq_mat_vec(
        format: IqQuantFormat,
        matrix: &[u8],
        rows: usize,
        columns: usize,
        input: &[f32],
    ) -> Vec<f32> {
        let row_size = row_bytes(format, columns).unwrap();
        (0..rows)
            .map(|row| {
                dequantize_row(format, &matrix[row * row_size..(row + 1) * row_size])
                    .unwrap()
                    .iter()
                    .zip(input)
                    .map(|(weight, value)| weight * value)
                    .sum()
            })
            .collect()
    }

    fn run_resident(
        executor: &Nvfp4CudaExecutor,
        payloads: &[&[u8]],
        hidden: &[f32],
    ) -> Vec<Vec<f32>> {
        let leases = payloads
            .iter()
            .enumerate()
            .map(|(expert, payload)| {
                executor
                    .resident_admit(
                        super::super::cache::ExpertKey::new("cuda-test", 0, expert as u32, "nvfp4"),
                        payload,
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        executor.run_batch_resident(&leases, hidden).unwrap()
    }

    #[test]
    fn executor_cuda_nvfp4_preserva_o_forward_do_decoder_cpu() {
        let hidden = 64;
        let intermediate = 64;
        let payload = payload(hidden, intermediate);
        let hidden_values = vec![1.0f32; hidden];
        let cpu = Nvfp4Expert::from_payload(hidden, intermediate, &payload)
            .unwrap()
            .run(&hidden_values)
            .unwrap();
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate, 1_000_000).unwrap();
        let gpu = run_resident(&executor, &[payload.as_slice()], &hidden_values)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(gpu.len(), cpu.len());
        for (actual, expected) in gpu.iter().zip(cpu) {
            assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
        }
    }

    #[test]
    fn executor_cuda_k_quants_preserva_q4k_e_q6k_do_decoder_cpu() {
        let hidden = 256;
        let intermediate = 256;
        let hidden_values = (0..hidden)
            .map(|index| (index as f32 - 127.0) * 0.0078125)
            .collect::<Vec<_>>();
        for format in [KQuantFormat::Q4K, KQuantFormat::Q6K] {
            let payload = k_payload(format, hidden, intermediate);
            let expected = KQuantExpert::from_payload(format, hidden, intermediate, &payload)
                .unwrap()
                .run(&hidden_values)
                .unwrap();
            let executor = KQuantCudaExecutor::new_with_cache(
                format,
                hidden,
                intermediate,
                measured_device_cache(1_000_000).unwrap(),
            )
            .unwrap();
            let lease = executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-k-test", 0, 0, format.name()),
                    &payload,
                )
                .unwrap();
            let actual = executor
                .run_route_batch_resident(&[lease], &[0], &[1.0], &[0, 1], &hidden_values, 1)
                .unwrap();
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
            }
        }
    }

    #[test]
    fn executor_cuda_plain_preserva_f32_f16_e_bf16_do_decoder_cpu() {
        let hidden = 64;
        let intermediate = 64;
        let hidden_values = (0..hidden)
            .map(|index| (index as f32 - 31.0) * 0.03125)
            .collect::<Vec<_>>();
        for format in [PlainFormat::F32, PlainFormat::F16, PlainFormat::Bf16] {
            let payload = plain_payload(format, hidden, intermediate);
            let expected = PlainExpert::from_payload(format, hidden, intermediate, &payload)
                .unwrap()
                .run(&hidden_values)
                .unwrap();
            let executor = PlainCudaExecutor::new_with_cache(
                format,
                hidden,
                intermediate,
                measured_device_cache(1_000_000).unwrap(),
            )
            .unwrap();
            let lease = executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-plain-test", 0, 0, format.name()),
                    &payload,
                )
                .unwrap();
            let actual = executor
                .run_route_batch_resident(&[lease], &[0], &[1.0], &[0, 1], &hidden_values, 1)
                .unwrap();
            let tolerance = if matches!(format, PlainFormat::F32) {
                1e-5
            } else {
                2e-3
            };
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert!(
                    (actual - expected).abs() <= tolerance,
                    "{actual} != {expected}"
                );
            }
        }
    }

    #[test]
    fn executor_cuda_iq_preserva_iq3s_iq2s_e_iq4xs_do_decoder_cpu() {
        let hidden = 256;
        let intermediate = 256;
        let hidden_values = (0..hidden)
            .map(|index| (index as f32 - 127.0) * 0.0078125)
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for format in [
            IqQuantFormat::Iq3S,
            IqQuantFormat::Iq2S,
            IqQuantFormat::Iq4XS,
        ] {
            let payload = iq_payload(format, hidden, intermediate);
            let expected = IqQuantExpert::from_payload(format, hidden, intermediate, &payload)
                .unwrap()
                .run(&hidden_values)
                .unwrap();
            let executor = IqQuantCudaExecutor::new_for_differential_test(
                format,
                hidden,
                intermediate,
                measured_device_cache(1_000_000).unwrap(),
            );
            let lease = executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-iq-test", 0, 0, format.name()),
                    &payload,
                )
                .unwrap();
            let actual = executor
                .run_route_batch_resident(&[lease], &[0], &[1.0], &[0, 1], &hidden_values, 1)
                .unwrap();
            assert_eq!(actual.len(), expected.len());
            if let Some(difference) = first_numeric_difference(&actual, &expected, 1e-4) {
                eprintln!(
                    "IQ {}: primeiro erro índice={} actual={} esperado={} abs={} rel={}",
                    format.name(),
                    difference.index,
                    difference.actual,
                    difference.expected,
                    difference.absolute,
                    difference.relative
                );
                failures.push(format.name());
            }
        }
        assert!(failures.is_empty(), "IQ formats diverged: {failures:?}");
    }

    #[test]
    fn diagnostico_cuda_iq_divergencia_por_estagio() {
        let hidden = 256;
        let intermediate = 256;
        let hidden_values = (0..hidden)
            .map(|index| (index as f32 - 127.0) * 0.0078125)
            .collect::<Vec<_>>();
        for format in [
            IqQuantFormat::Iq3S,
            IqQuantFormat::Iq2S,
            IqQuantFormat::Iq4XS,
        ] {
            let payload = iq_payload(format, hidden, intermediate);
            let (gate, up, down) = split_iq_payload(format, hidden, intermediate);
            let gate_values = iq_mat_vec(format, &gate, intermediate, hidden, &hidden_values);
            let up_values = iq_mat_vec(format, &up, intermediate, hidden, &hidden_values);
            let expected_gate_up = gate_values
                .iter()
                .chain(&up_values)
                .copied()
                .collect::<Vec<_>>();
            let expected_activated = gate_values
                .iter()
                .zip(&up_values)
                .map(|(gate, up)| gate / (1.0 + (-gate).exp()) * up)
                .collect::<Vec<_>>();
            let expected_output =
                iq_mat_vec(format, &down, hidden, intermediate, &expected_activated);
            let executor = IqQuantCudaExecutor::new_for_differential_test(
                format,
                hidden,
                intermediate,
                measured_device_cache(1_000_000).unwrap(),
            );
            let lease = executor
                .resident_admit(
                    super::super::cache::ExpertKey::new(
                        "cuda-iq-stage-diagnostic",
                        0,
                        0,
                        format.name(),
                    ),
                    &payload,
                )
                .unwrap();
            let mut actual_gate_up = vec![0.0f32; 2 * intermediate];
            let mut actual_activated = vec![0.0f32; intermediate];
            let mut actual_output = vec![0.0f32; hidden];
            let status = unsafe {
                pom_moe_iquant_trace_single_resident(
                    format.dtype(),
                    lease.handle() as *const u8,
                    hidden_values.as_ptr(),
                    hidden_values.len(),
                    actual_gate_up.as_mut_ptr(),
                    actual_gate_up.len() * std::mem::size_of::<f32>(),
                    actual_activated.as_mut_ptr(),
                    actual_activated.len() * std::mem::size_of::<f32>(),
                    actual_output.as_mut_ptr(),
                    actual_output.len() * std::mem::size_of::<f32>(),
                    hidden,
                    intermediate,
                )
            };
            assert_eq!(status, 0, "trace CUDA falhou para {}", format.name());
            let gate_up_error = first_numeric_difference(&actual_gate_up, &expected_gate_up, 1e-4);
            let activation_error =
                first_numeric_difference(&actual_activated, &expected_activated, 1e-4);
            let output_error = first_numeric_difference(&actual_output, &expected_output, 1e-4);
            eprintln!(
                "IQ {} estágios: gate_up={gate_up_error:?}, activation={activation_error:?}, down={output_error:?}",
                format.name(),
            );
        }
    }

    #[derive(Debug)]
    struct NumericDifference {
        index: usize,
        actual: f32,
        expected: f32,
        absolute: f32,
        relative: f32,
    }

    fn first_numeric_difference(
        actual: &[f32],
        expected: &[f32],
        tolerance: f32,
    ) -> Option<NumericDifference> {
        actual
            .iter()
            .zip(expected)
            .enumerate()
            .find_map(|(index, (&actual, &expected))| {
                let absolute = (actual - expected).abs();
                let relative = absolute / expected.abs().max(f32::MIN_POSITIVE);
                if !actual.is_finite()
                    || !expected.is_finite()
                    || absolute > tolerance && relative > tolerance
                {
                    Some(NumericDifference {
                        index,
                        actual,
                        expected,
                        absolute,
                        relative,
                    })
                } else {
                    None
                }
            })
    }

    #[test]
    fn executor_cuda_iq_heterogeneo_preserva_gate_up_down_por_dtype() {
        let hidden = 256;
        let intermediate = 256;
        let hidden_values = (0..hidden)
            .map(|index| (index as f32 - 127.0) * 0.0078125)
            .collect::<Vec<_>>();
        let (gate, _, _) = split_iq_payload(IqQuantFormat::Iq3S, hidden, intermediate);
        let (_, up, _) = split_iq_payload(IqQuantFormat::Iq2S, hidden, intermediate);
        let (_, _, down) = split_iq_payload(IqQuantFormat::Iq4XS, hidden, intermediate);
        let gate_values = iq_mat_vec(
            IqQuantFormat::Iq3S,
            &gate,
            intermediate,
            hidden,
            &hidden_values,
        );
        let up_values = iq_mat_vec(
            IqQuantFormat::Iq2S,
            &up,
            intermediate,
            hidden,
            &hidden_values,
        );
        let activated = gate_values
            .iter()
            .zip(up_values)
            .map(|(gate, up)| gate / (1.0 + (-gate).exp()) * up)
            .collect::<Vec<_>>();
        let expected = iq_mat_vec(
            IqQuantFormat::Iq4XS,
            &down,
            hidden,
            intermediate,
            &activated,
        );

        let executor = HeterogeneousIqCudaExecutor::new_with_cache(
            hidden,
            intermediate,
            measured_device_cache(1_000_000).unwrap(),
        );
        let gate_lease = executor
            .resident_admit(
                super::super::cache::ExpertKey::new("cuda-heterogeneous-test", 0, 0, "gate-21"),
                &gate,
            )
            .unwrap();
        let up_lease = executor
            .resident_admit(
                super::super::cache::ExpertKey::new("cuda-heterogeneous-test", 0, 0, "up-22"),
                &up,
            )
            .unwrap();
        let down_lease = executor
            .resident_admit(
                super::super::cache::ExpertKey::new("cuda-heterogeneous-test", 0, 0, "down-23"),
                &down,
            )
            .unwrap();
        let descriptor = CudaHeterogeneousExpert {
            gate: CudaHeterogeneousTensor {
                data: gate_lease.handle() as *const u8,
                bytes: gate.len(),
                dtype: 21,
            },
            up: CudaHeterogeneousTensor {
                data: up_lease.handle() as *const u8,
                bytes: up.len(),
                dtype: 22,
            },
            down: CudaHeterogeneousTensor {
                data: down_lease.handle() as *const u8,
                bytes: down.len(),
                dtype: 23,
            },
            ..CudaHeterogeneousExpert::default()
        };
        let actual = executor
            .run_route_batch_resident(&[descriptor], &[0], &[1.0], &[0, 1], &hidden_values, 1, 1)
            .unwrap();
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
        }
    }

    #[test]
    fn executor_cuda_nvfp4_preserva_quantizacao_q8_e_gelu_fp16() {
        let hidden = 64;
        let intermediate = 64;
        let payload = payload(hidden, intermediate);
        let hidden_values = vec![0.125f32; hidden];
        let cpu = Nvfp4Expert::from_payload(hidden, intermediate, &payload)
            .unwrap()
            .run(&hidden_values)
            .unwrap();
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate, 1_000_000).unwrap();
        let gpu = run_resident(&executor, &[payload.as_slice()], &hidden_values)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(gpu.len(), cpu.len());
        for (actual, expected) in gpu.iter().zip(cpu) {
            assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
        }
    }

    #[test]
    fn executor_cuda_nvfp4_batch_preserva_cada_expert() {
        let hidden = 64;
        let intermediate = 64;
        let first = payload_with_nibble(hidden, intermediate, 0x22);
        let second = payload_with_nibble(hidden, intermediate, 0x33);
        let hidden_values = (0..hidden)
            .map(|index| (index as f32 - 31.0) * 0.03125)
            .collect::<Vec<_>>();
        let cpu = [&first, &second]
            .iter()
            .map(|payload| {
                Nvfp4Expert::from_payload(hidden, intermediate, payload)
                    .unwrap()
                    .run(&hidden_values)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate, 1_000_000).unwrap();
        let gpu = run_resident(
            &executor,
            &[first.as_slice(), second.as_slice()],
            &hidden_values,
        );
        assert_eq!(gpu.len(), cpu.len());
        for (actual, expected) in gpu.iter().zip(cpu) {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
            }
        }
    }

    #[test]
    fn executor_cuda_nvfp4_lote_de_rotas_agrega_tokens_no_device() {
        let hidden = 64;
        let intermediate = 64;
        let first = payload_with_nibble(hidden, intermediate, 0x22);
        let second = payload_with_nibble(hidden, intermediate, 0x33);
        let hidden_values = [
            (0..hidden)
                .map(|index| (index as f32 - 31.0) * 0.03125)
                .collect::<Vec<_>>(),
            (0..hidden)
                .map(|index| (15.0 - index as f32) * 0.0625)
                .collect::<Vec<_>>(),
        ];
        let cpu = [
            (&first, &hidden_values[0], [0.25f32, 0.75f32]),
            (&second, &hidden_values[0], [0.60f32, 0.40f32]),
        ];
        let expected = [
            [
                Nvfp4Expert::from_payload(hidden, intermediate, cpu[0].0)
                    .unwrap()
                    .run(cpu[0].1)
                    .unwrap(),
                Nvfp4Expert::from_payload(hidden, intermediate, cpu[1].0)
                    .unwrap()
                    .run(cpu[1].1)
                    .unwrap(),
            ],
            [
                Nvfp4Expert::from_payload(hidden, intermediate, &second)
                    .unwrap()
                    .run(&hidden_values[1])
                    .unwrap(),
                Nvfp4Expert::from_payload(hidden, intermediate, &first)
                    .unwrap()
                    .run(&hidden_values[1])
                    .unwrap(),
            ],
        ];
        let expected = [
            expected[0][0]
                .iter()
                .zip(&expected[0][1])
                .map(|(first, second)| 0.25 * first + 0.75 * second)
                .collect::<Vec<_>>(),
            expected[1][0]
                .iter()
                .zip(&expected[1][1])
                .map(|(first, second)| 0.60 * first + 0.40 * second)
                .collect::<Vec<_>>(),
        ];
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate, 1_000_000).unwrap();
        let leases = vec![
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-test", 0, 0, "nvfp4"),
                    &first,
                )
                .unwrap(),
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-test", 0, 1, "nvfp4"),
                    &second,
                )
                .unwrap(),
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-test", 0, 1, "nvfp4"),
                    &second,
                )
                .unwrap(),
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-test", 0, 0, "nvfp4"),
                    &first,
                )
                .unwrap(),
        ];
        let actual = executor
            .run_route_batch_resident(
                &leases,
                &[0, 0, 1, 1],
                &[0.25, 0.75, 0.60, 0.40],
                &[0, 2, 4],
                &hidden_values.concat(),
                2,
            )
            .unwrap();
        for (actual, expected) in actual.chunks_exact(hidden).zip(expected) {
            eprintln!(
                "NVFP4 route: actual[0..4]={:?} expected[0..4]={:?}",
                &actual[..4],
                &expected[..4]
            );
            for (actual, expected) in actual.iter().zip(expected) {
                assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
            }
        }
    }

    #[test]
    fn executor_cuda_nvfp4_rota_de_um_expert_por_token_isola_multitoken() {
        let hidden = 64;
        let intermediate = 64;
        let first = payload_with_nibble(hidden, intermediate, 0x22);
        let second = payload_with_nibble(hidden, intermediate, 0x33);
        let hidden_values = [
            (0..hidden)
                .map(|index| (index as f32 - 31.0) * 0.03125)
                .collect::<Vec<_>>(),
            (0..hidden)
                .map(|index| (15.0 - index as f32) * 0.0625)
                .collect::<Vec<_>>(),
        ];
        let expected = [
            Nvfp4Expert::from_payload(hidden, intermediate, &first)
                .unwrap()
                .run(&hidden_values[0])
                .unwrap(),
            Nvfp4Expert::from_payload(hidden, intermediate, &second)
                .unwrap()
                .run(&hidden_values[1])
                .unwrap(),
        ];
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate, 1_000_000).unwrap();
        let leases = vec![
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-one", 0, 0, "nvfp4"),
                    &first,
                )
                .unwrap(),
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-one", 0, 1, "nvfp4"),
                    &second,
                )
                .unwrap(),
        ];
        let actual = executor
            .run_route_batch_resident(
                &leases,
                &[0, 1],
                &[1.0, 1.0],
                &[0, 1, 2],
                &hidden_values.concat(),
                2,
            )
            .unwrap();
        for (token_index, (actual, expected)) in
            actual.chunks_exact(hidden).zip(expected).enumerate()
        {
            eprintln!(
                "NVFP4 one route token {token_index}: actual[0..4]={:?} expected[0..4]={:?}",
                &actual[..4],
                &expected[..4]
            );
            for (actual, expected) in actual.iter().zip(expected) {
                assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
            }
        }
    }

    #[test]
    fn executor_cuda_nvfp4_rota_multitoken_sem_chaves_repetidas() {
        let hidden = 64;
        let intermediate = 64;
        let first = payload_with_nibble(hidden, intermediate, 0x22);
        let second = payload_with_nibble(hidden, intermediate, 0x33);
        let hidden_values = [
            (0..hidden)
                .map(|index| (index as f32 - 31.0) * 0.03125)
                .collect::<Vec<_>>(),
            (0..hidden)
                .map(|index| (15.0 - index as f32) * 0.0625)
                .collect::<Vec<_>>(),
        ];
        let expected = [
            (
                [
                    Nvfp4Expert::from_payload(hidden, intermediate, &first)
                        .unwrap()
                        .run(&hidden_values[0])
                        .unwrap(),
                    Nvfp4Expert::from_payload(hidden, intermediate, &second)
                        .unwrap()
                        .run(&hidden_values[0])
                        .unwrap(),
                ],
                [0.25, 0.75],
            ),
            (
                [
                    Nvfp4Expert::from_payload(hidden, intermediate, &second)
                        .unwrap()
                        .run(&hidden_values[1])
                        .unwrap(),
                    Nvfp4Expert::from_payload(hidden, intermediate, &first)
                        .unwrap()
                        .run(&hidden_values[1])
                        .unwrap(),
                ],
                [0.60, 0.40],
            ),
        ]
        .map(|(pair, weights)| {
            pair[0]
                .iter()
                .zip(&pair[1])
                .map(|(first, second)| weights[0] * first + weights[1] * second)
                .collect::<Vec<_>>()
        });
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate, 1_000_000).unwrap();
        let leases = [(&first, 0), (&second, 1), (&second, 2), (&first, 3)]
            .into_iter()
            .map(|(payload, expert)| {
                executor
                    .resident_admit(
                        super::super::cache::ExpertKey::new(
                            "cuda-route-unique",
                            0,
                            expert,
                            "nvfp4",
                        ),
                        payload,
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let actual = executor
            .run_route_batch_resident(
                &leases,
                &[0, 0, 1, 1],
                &[0.25, 0.75, 0.60, 0.40],
                &[0, 2, 4],
                &hidden_values.concat(),
                2,
            )
            .unwrap();
        for (actual, expected) in actual.chunks_exact(hidden).zip(expected) {
            for (actual, expected) in actual.iter().zip(expected) {
                assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
            }
        }
    }

    #[test]
    fn executor_cuda_nvfp4_rota_de_dois_experts_por_token_isola_o_lote() {
        let hidden = 64;
        let intermediate = 64;
        let first = payload_with_nibble(hidden, intermediate, 0x22);
        let second = payload_with_nibble(hidden, intermediate, 0x33);
        let hidden_values = [
            (0..hidden)
                .map(|index| (index as f32 - 31.0) * 0.03125)
                .collect::<Vec<_>>(),
            (0..hidden)
                .map(|index| (15.0 - index as f32) * 0.0625)
                .collect::<Vec<_>>(),
        ];
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate, 1_000_000).unwrap();
        let leases = vec![
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-isolation", 0, 0, "nvfp4"),
                    &first,
                )
                .unwrap(),
            executor
                .resident_admit(
                    super::super::cache::ExpertKey::new("cuda-route-isolation", 0, 1, "nvfp4"),
                    &second,
                )
                .unwrap(),
        ];
        for (token_index, token) in hidden_values.iter().enumerate() {
            let expected = [
                Nvfp4Expert::from_payload(hidden, intermediate, &first)
                    .unwrap()
                    .run(token)
                    .unwrap(),
                Nvfp4Expert::from_payload(hidden, intermediate, &second)
                    .unwrap()
                    .run(token)
                    .unwrap(),
            ];
            let expected = expected[0]
                .iter()
                .zip(&expected[1])
                .map(|(first, second)| 0.25 * first + 0.75 * second)
                .collect::<Vec<_>>();
            let actual = executor
                .run_route_batch_resident(&leases, &[0, 0], &[0.25, 0.75], &[0, 2], token, 1)
                .unwrap();
            assert_eq!(actual.len(), expected.len(), "token {token_index}");
            for (actual, expected) in actual.iter().zip(expected) {
                assert!(
                    (actual - expected).abs() <= 1e-4,
                    "token {token_index}: {actual} != {expected}"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires a real NVFP4 GGUF and a CUDA device"]
    fn executor_cuda_nvfp4_lote_preserva_geometria_do_gguf_real() {
        let path = std::env::var_os("POM_MOE_NUMERIC_MODEL")
            .expect("POM_MOE_NUMERIC_MODEL must name a real MoE GGUF");
        let path = std::path::PathBuf::from(path);
        let file = std::fs::File::open(&path).unwrap();
        let mmap = unsafe { Mmap::map(&file) }.unwrap();
        let (_, tensors) = parse_tensor_table(&mmap).unwrap();
        let experts = tensors
            .iter()
            .find(|tensor| {
                tensor.name.ends_with("ffn_gate_up_exps.weight")
                    || tensor.name.ends_with("ffn_gate_exps.weight")
            })
            .and_then(|tensor| tensor.dims.get(2).copied())
            .and_then(|count| u32::try_from(count).ok())
            .expect("real GGUF must expose expert count in the gate tensor");
        drop(mmap);

        let source = Nvfp4Source::open_infer(&path, experts).unwrap();
        let hidden = (0..source.hidden())
            .map(|index| (index as f32 % 17.0 - 8.0) * 0.03125)
            .collect::<Vec<_>>();
        let payloads = [
            source.load_expert(0, 0).unwrap(),
            source.load_expert(0, 1).unwrap(),
        ];
        let cpu = payloads
            .iter()
            .map(|payload| {
                Nvfp4Expert::from_payload(source.hidden(), source.intermediate(), payload)
                    .unwrap()
                    .run(&hidden)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let executor =
            Nvfp4CudaExecutor::new(source.hidden(), source.intermediate(), 1_000_000).unwrap();
        let gpu = run_resident(
            &executor,
            &[payloads[0].as_slice(), payloads[1].as_slice()],
            &hidden,
        );

        assert_eq!(gpu.len(), cpu.len());
        for (actual, expected) in gpu.iter().zip(cpu) {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert!(
                    (actual - expected).abs() <= 2.0e-3,
                    "real GGUF CUDA batch diverged: {actual} != {expected}"
                );
            }
        }
    }

    #[test]
    fn diagnostico_cuda_acumula_fases_e_bytes_por_chamada() {
        let mut total = Nvfp4CudaDiagnostics::default();
        total.add_call(CudaMoeCallTiming {
            expert_calls: 1,
            kernel_launches: 2,
            sync_count: 2,
            payload_validation_ms: 1.0,
            alloc_ms: 2.0,
            h2d_ms: 3.0,
            h2d_bytes: 10,
            kernel_ms: 4.0,
            launch_ms: 5.0,
            sync_ms: 6.0,
            d2h_ms: 7.0,
            d2h_bytes: 8,
            free_ms: 9.0,
        });
        total.add_call(CudaMoeCallTiming {
            expert_calls: 1,
            kernel_launches: 2,
            sync_count: 2,
            payload_validation_ms: 10.0,
            alloc_ms: 20.0,
            h2d_ms: 30.0,
            h2d_bytes: 100,
            kernel_ms: 40.0,
            launch_ms: 50.0,
            sync_ms: 60.0,
            d2h_ms: 70.0,
            d2h_bytes: 80,
            free_ms: 90.0,
        });

        assert_eq!(total.calls, 2);
        assert_eq!(total.kernel_launches, 4);
        assert_eq!(total.sync_count, 4);
        assert_eq!(total.h2d_bytes, 110);
        assert_eq!(total.d2h_bytes, 88);
        assert_eq!(total.h2d_ms, 33.0);
        assert_eq!(total.kernel_ms, 44.0);
        assert_eq!(total.sync_ms, 66.0);
    }
}
