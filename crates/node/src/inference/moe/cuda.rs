//! Ponte Rust -> CUDA para o executor NVFP4 dos experts.

#![cfg(feature = "cuda")]

use super::format_nvfp4::{Nvfp4Error, Nvfp4Expert};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CudaMoeError {
    Geometry(String),
    Payload(String),
    PayloadLength { expected: usize, actual: usize },
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
        }
    }
}

pub struct Nvfp4CudaExecutor {
    hidden: usize,
    intermediate: usize,
}

impl Nvfp4CudaExecutor {
    pub fn new(hidden: usize, intermediate: usize) -> Self {
        debug_assert!(Nvfp4Expert::packed_bytes(hidden, intermediate).is_ok());
        Self {
            hidden,
            intermediate,
        }
    }

    pub fn run(&self, payload: &[u8], hidden: &[f32]) -> Result<Vec<f32>, CudaMoeError> {
        if hidden.len() != self.hidden {
            return Err(CudaMoeError::HiddenWidth {
                expected: self.hidden,
                actual: hidden.len(),
            });
        }
        Nvfp4Expert::from_payload(self.hidden, self.intermediate, payload)?;

        let mut output = vec![0.0f32; self.hidden];
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
            )
        };
        if status != 0 {
            return Err(CudaMoeError::Launch(status));
        }
        Ok(output)
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
        let gate_up = packed_rows(2 * intermediate, hidden, 0x22);
        let down = packed_rows(hidden, intermediate, 0x22);
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
}
