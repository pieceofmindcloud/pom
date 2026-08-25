//! Ponte Rust -> CUDA para o executor NVFP4 dos experts.

#![cfg(feature = "cuda")]

use super::format_nvfp4::{Nvfp4Error, Nvfp4Expert};
use std::fmt;
use std::sync::Mutex;
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
    BackendUnavailable,
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
            Self::BackendUnavailable => formatter.write_str(
                "executor CUDA NVFP4 indisponível: o backend CUDA não está disponível para este binário"
            ),
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
            Nvfp4Error::BackendUnavailable => Self::BackendUnavailable,
        }
    }
}

pub struct Nvfp4CudaExecutor {
    hidden: usize,
    intermediate: usize,
    profile: bool,
    diagnostics: Mutex<Nvfp4CudaDiagnostics>,
}

impl Nvfp4CudaExecutor {
    pub fn new(hidden: usize, intermediate: usize) -> Self {
        debug_assert!(Nvfp4Expert::packed_bytes(hidden, intermediate).is_ok());
        Self {
            hidden,
            intermediate,
            profile: std::env::var("POM_MOE_CUDA_PROFILE")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
            diagnostics: Mutex::new(Nvfp4CudaDiagnostics::default()),
        }
    }

    pub fn run(&self, payload: &[u8], hidden: &[f32]) -> Result<Vec<f32>, CudaMoeError> {
        if hidden.len() != self.hidden {
            return Err(CudaMoeError::HiddenWidth {
                expected: self.hidden,
                actual: hidden.len(),
            });
        }
        let validation_started = Instant::now();
        Nvfp4Expert::from_payload(self.hidden, self.intermediate, payload)?;
        let payload_validation_ms = validation_started.elapsed().as_secs_f64() * 1_000.0;

        let mut output = vec![0.0f32; self.hidden];
        let mut timing = CudaMoeCallTiming {
            expert_calls: 1,
            payload_validation_ms,
            ..CudaMoeCallTiming::default()
        };
        let status = unsafe {
            pom_moe_nvfp4_forward(
                payload.as_ptr(),
                payload.len(),
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
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            self.diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .add_call(timing);
        }
        Ok(output)
    }

    pub fn run_batch(
        &self,
        payloads: &[&[u8]],
        hidden: &[f32],
    ) -> Result<Vec<Vec<f32>>, CudaMoeError> {
        if payloads.is_empty() {
            return Err(CudaMoeError::Payload("batch de experts vazio".into()));
        }
        if hidden.len() != self.hidden {
            return Err(CudaMoeError::HiddenWidth {
                expected: self.hidden,
                actual: hidden.len(),
            });
        }
        let validation_started = Instant::now();
        let mut payload_len = None;
        for payload in payloads {
            Nvfp4Expert::from_payload(self.hidden, self.intermediate, payload)?;
            if let Some(expected) = payload_len {
                if payload.len() != expected {
                    return Err(CudaMoeError::Payload(
                        "payloads NVFP4 do batch têm tamanhos diferentes".into(),
                    ));
                }
            } else {
                payload_len = Some(payload.len());
            }
        }
        let payload_len = payload_len.expect("batch não vazio foi validado");
        let payload_validation_ms = validation_started.elapsed().as_secs_f64() * 1_000.0;
        let mut flat_payloads = Vec::with_capacity(payload_len * payloads.len());
        for payload in payloads {
            flat_payloads.extend_from_slice(payload);
        }
        let mut flat_output = vec![0.0f32; self.hidden * payloads.len()];
        let mut timing = CudaMoeCallTiming {
            expert_calls: payloads.len() as u64,
            payload_validation_ms,
            ..CudaMoeCallTiming::default()
        };
        let status = unsafe {
            pom_moe_nvfp4_forward_batch(
                flat_payloads.as_ptr(),
                payload_len,
                payloads.len(),
                hidden.as_ptr(),
                hidden.len(),
                flat_output.as_mut_ptr(),
                self.hidden,
                self.intermediate,
                if self.profile {
                    &mut timing
                } else {
                    std::ptr::null_mut()
                },
            )
        };
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        if self.profile {
            self.diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .add_call(timing);
        }
        Ok(flat_output
            .chunks_exact(self.hidden)
            .map(ToOwned::to_owned)
            .collect())
    }

    pub fn diagnostics(&self) -> Nvfp4CudaDiagnostics {
        self.diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_owned()
    }
}

#[cfg(feature = "cuda")]
unsafe extern "C" {
    fn pom_moe_nvfp4_forward(
        payload: *const u8,
        payload_len: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        output_len: usize,
        hidden_width: usize,
        intermediate_width: usize,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;

    fn pom_moe_nvfp4_forward_batch(
        payloads: *const u8,
        payload_len: usize,
        payload_count: usize,
        hidden: *const f32,
        hidden_len: usize,
        output: *mut f32,
        hidden_width: usize,
        intermediate_width: usize,
        timing: *mut CudaMoeCallTiming,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let gpu = Nvfp4CudaExecutor::new(hidden, intermediate)
            .run(&payload, &hidden_values)
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
        let gpu = Nvfp4CudaExecutor::new(hidden, intermediate)
            .run(&payload, &hidden_values)
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
        let gpu = Nvfp4CudaExecutor::new(hidden, intermediate)
            .run_batch(&[first.as_slice(), second.as_slice()], &hidden_values)
            .unwrap();
        assert_eq!(gpu.len(), cpu.len());
        for (actual, expected) in gpu.iter().zip(cpu) {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert!((actual - expected).abs() <= 1e-4, "{actual} != {expected}");
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
