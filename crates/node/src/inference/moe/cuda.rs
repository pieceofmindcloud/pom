//! Ponte Rust -> CUDA para o executor NVFP4 dos experts.

#![cfg(feature = "cuda")]

use super::admission::MemoryAccount;
use super::cache::{DeviceAllocation, DeviceAllocator, DeviceExpertCache};
use super::format_nvfp4::{Nvfp4Error, Nvfp4Expert};
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
            Self::Geometry(reason) => write!(formatter, "geometria CUDA NVFP4 inválida: {reason}"),
            Self::Payload(reason) => write!(formatter, "payload CUDA NVFP4 inválido: {reason}"),
            Self::PayloadLength { expected, actual } => write!(
                formatter,
                "payload CUDA NVFP4 tem {actual} bytes, mas a geometria exige {expected}"
            ),
            Self::BackendUnavailable(reason) => {
                write!(formatter, "backend CUDA NVFP4 indisponível: {reason}")
            }
            Self::Cache(reason) => write!(formatter, "cache CUDA NVFP4 recusou o expert: {reason}"),
            Self::HiddenWidth { expected, actual } => {
                write!(
                    formatter,
                    "hidden CUDA NVFP4 tem {actual}, esperado {expected}"
                )
            }
            Self::Launch(code) => write!(formatter, "kernel CUDA NVFP4 falhou (código {code})"),
        }
    }
}

impl std::error::Error for CudaMoeError {}

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

impl Nvfp4CudaExecutor {
    pub fn new(hidden: usize, intermediate: usize) -> Result<Self, CudaMoeError> {
        debug_assert!(Nvfp4Expert::packed_bytes(hidden, intermediate).is_ok());
        let resident_cache = measured_device_cache()?;
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

fn measured_device_cache() -> Result<Arc<DeviceExpertCache>, CudaMoeError> {
    let allocator: Arc<dyn DeviceAllocator> = Arc::new(CudaDeviceAllocator);
    let baseline_free_bytes = allocator
        .free_bytes()
        .map_err(|error| CudaMoeError::BackendUnavailable(error.to_string()))?;
    Ok(DeviceExpertCache::new(
        MemoryAccount {
            baseline_free_bytes,
            weights_bytes: 0,
            fixed_bytes: 0,
            reserve_bytes: 0,
        },
        allocator,
    ))
}

pub fn create_device_cache() -> Result<Arc<DeviceExpertCache>, CudaMoeError> {
    measured_device_cache()
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

    fn pom_moe_device_free_bytes(free_bytes: *mut u64) -> i32;
    fn pom_moe_device_upload(payload: *const u8, payload_len: usize, out: *mut *mut c_void) -> i32;
    fn pom_moe_device_free(handle: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::super::format_nvfp4::{Nvfp4Expert, Nvfp4Source};
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
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate).unwrap();
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
    fn executor_cuda_nvfp4_preserva_quantizacao_q8_e_gelu_fp16() {
        let hidden = 64;
        let intermediate = 64;
        let payload = payload(hidden, intermediate);
        let hidden_values = vec![0.125f32; hidden];
        let cpu = Nvfp4Expert::from_payload(hidden, intermediate, &payload)
            .unwrap()
            .run(&hidden_values)
            .unwrap();
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate).unwrap();
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
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate).unwrap();
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
        let executor = Nvfp4CudaExecutor::new(hidden, intermediate).unwrap();
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
                &[0, 1, 1, 0],
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
        let executor = Nvfp4CudaExecutor::new(source.hidden(), source.intermediate()).unwrap();
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
