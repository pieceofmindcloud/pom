//! Executor CPU mínimo para experts GGML Q4_0.
//!
//! O payload contém duas matrizes packed, `gate_up` e `down`, na ordem de
//! blocos do GGML. O executor faz o gate-up, SiLU gated e down projection. A
//! seleção top-k e a soma ponderada continuam sendo responsabilidade do bloco
//! MoE que o chamar.

use super::cache::{CacheError, ExpertCache, ExpertKey};
use std::fmt;
use std::sync::Arc;

const BLOCK_ELEMENTS: usize = 32;
const BLOCK_BYTES: usize = 18;
const PAYLOAD_HEADER_BYTES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Q4Error {
    InvalidGeometry(&'static str),
    InvalidPayload(&'static str),
    InvalidPayloadLength { expected: usize, actual: usize },
}

impl fmt::Display for Q4Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGeometry(reason) => write!(formatter, "geometria Q4_0 inválida: {reason}"),
            Self::InvalidPayload(reason) => write!(formatter, "payload Q4_0 inválido: {reason}"),
            Self::InvalidPayloadLength { expected, actual } => write!(
                formatter,
                "payload Q4_0 tem {actual} bytes, mas a geometria exige {expected}"
            ),
        }
    }
}

impl std::error::Error for Q4Error {}

#[derive(Debug)]
pub enum Q4ExecuteError {
    Cache(CacheError),
    Format(Q4Error),
    HiddenWidth { expected: usize, actual: usize },
}

impl fmt::Display for Q4ExecuteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cache(error) => error.fmt(formatter),
            Self::Format(error) => error.fmt(formatter),
            Self::HiddenWidth { expected, actual } => {
                write!(
                    formatter,
                    "hidden Q4_0 tem {actual} elementos, esperado {expected}"
                )
            }
        }
    }
}

impl std::error::Error for Q4ExecuteError {}

impl From<CacheError> for Q4ExecuteError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<Q4Error> for Q4ExecuteError {
    fn from(error: Q4Error) -> Self {
        Self::Format(error)
    }
}

pub struct Q4_0Expert {
    hidden: usize,
    intermediate: usize,
    gate_up: Vec<u8>,
    down: Vec<u8>,
}

impl Q4_0Expert {
    pub fn packed_bytes(hidden: usize, intermediate: usize) -> Result<usize, Q4Error> {
        if hidden == 0 || intermediate == 0 {
            return Err(Q4Error::InvalidGeometry("dimensão zero"));
        }
        if hidden % BLOCK_ELEMENTS != 0 || intermediate % BLOCK_ELEMENTS != 0 {
            return Err(Q4Error::InvalidGeometry(
                "hidden e intermediate precisam fechar blocos de 32",
            ));
        }
        let gate_up = (2 * intermediate)
            .checked_mul(row_bytes(hidden)?)
            .ok_or(Q4Error::InvalidGeometry("overflow em gate_up"))?;
        let down = hidden
            .checked_mul(row_bytes(intermediate)?)
            .ok_or(Q4Error::InvalidGeometry("overflow em down"))?;
        PAYLOAD_HEADER_BYTES
            .checked_add(gate_up)
            .and_then(|bytes| bytes.checked_add(down))
            .ok_or(Q4Error::InvalidGeometry("overflow no payload"))
    }

    pub fn from_payload(
        hidden: usize,
        intermediate: usize,
        payload: &[u8],
    ) -> Result<Self, Q4Error> {
        let expected = Self::packed_bytes(hidden, intermediate)?;
        if payload.len() != expected {
            return Err(Q4Error::InvalidPayloadLength {
                expected,
                actual: payload.len(),
            });
        }
        let gate_up_len = u32::from_le_bytes(
            payload[..4]
                .try_into()
                .map_err(|_| Q4Error::InvalidPayload("cabeçalho gate_up ausente"))?,
        ) as usize;
        let expected_gate_up = (2 * intermediate) * row_bytes(hidden)?;
        if gate_up_len != expected_gate_up {
            return Err(Q4Error::InvalidPayload(
                "tamanho gate_up não corresponde à geometria",
            ));
        }
        let down_len = u32::from_le_bytes(
            payload[4..8]
                .try_into()
                .map_err(|_| Q4Error::InvalidPayload("cabeçalho down ausente"))?,
        ) as usize;
        let expected_down = hidden * row_bytes(intermediate)?;
        if down_len != expected_down {
            return Err(Q4Error::InvalidPayload(
                "tamanho down não corresponde à geometria",
            ));
        }
        let gate_up_start = PAYLOAD_HEADER_BYTES;
        let down_start = gate_up_start + gate_up_len;
        if down_start + down_len != payload.len() {
            return Err(Q4Error::InvalidPayload("seções não cobrem o payload"));
        }
        Ok(Self {
            hidden,
            intermediate,
            gate_up: payload[gate_up_start..down_start].to_vec(),
            down: payload[down_start..].to_vec(),
        })
    }

    pub fn run(&self, hidden: &[f32]) -> Result<Vec<f32>, Q4ExecuteError> {
        if hidden.len() != self.hidden {
            return Err(Q4ExecuteError::HiddenWidth {
                expected: self.hidden,
                actual: hidden.len(),
            });
        }
        let gate_up = mat_vec(&self.gate_up, 2 * self.intermediate, self.hidden, hidden);
        let gated: Vec<f32> = gate_up[..self.intermediate]
            .iter()
            .zip(&gate_up[self.intermediate..])
            .map(|(&gate, &up)| silu(gate) * up)
            .collect();
        Ok(mat_vec(&self.down, self.hidden, self.intermediate, &gated))
    }
}

/// Internal payload format used by the cache provider. It keeps the two bank
/// lengths explicit so a corrupt or mismatched expert cannot be interpreted as
/// a different geometry.
pub fn pack_payload(gate_up: &[u8], down: &[u8]) -> Result<Vec<u8>, Q4Error> {
    if gate_up.len() > u32::MAX as usize || down.len() > u32::MAX as usize {
        return Err(Q4Error::InvalidPayload("bank excede u32"));
    }
    let mut payload = Vec::with_capacity(PAYLOAD_HEADER_BYTES + gate_up.len() + down.len());
    payload.extend_from_slice(&(gate_up.len() as u32).to_le_bytes());
    payload.extend_from_slice(&(down.len() as u32).to_le_bytes());
    payload.extend_from_slice(gate_up);
    payload.extend_from_slice(down);
    Ok(payload)
}

/// Executor de um expert Q4_0 com cache compartilhado entre chamadas.
pub struct Q4_0Executor {
    cache: Arc<ExpertCache>,
    hidden: usize,
    intermediate: usize,
}

impl Q4_0Executor {
    pub fn new(
        cache: Arc<ExpertCache>,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, Q4Error> {
        Q4_0Expert::packed_bytes(hidden, intermediate)?;
        Ok(Self {
            cache,
            hidden,
            intermediate,
        })
    }

    /// Executa um expert, admitindo-o somente depois de o loader fornecer o
    /// payload packed. O loader é chamado fora da política de LRU, então não
    /// segura o mutex durante leitura de disco ou de GGUF.
    pub fn run<F>(
        &self,
        key: ExpertKey,
        load: F,
        hidden: &[f32],
    ) -> Result<Vec<f32>, Q4ExecuteError>
    where
        F: FnOnce() -> Result<Vec<u8>, CacheError>,
    {
        let lease = match self.cache.lookup(&key) {
            Some(lease) => lease,
            None => self.cache.admit(key, load()?)?,
        };
        let expert = Q4_0Expert::from_payload(self.hidden, self.intermediate, lease.bytes())?;
        expert.run(hidden)
    }
}

fn row_bytes(columns: usize) -> Result<usize, Q4Error> {
    if columns % BLOCK_ELEMENTS != 0 {
        return Err(Q4Error::InvalidGeometry("linha não fecha bloco de 32"));
    }
    columns
        .checked_div(BLOCK_ELEMENTS)
        .and_then(|blocks| blocks.checked_mul(BLOCK_BYTES))
        .ok_or(Q4Error::InvalidGeometry("overflow na linha"))
}

fn mat_vec(matrix: &[u8], rows: usize, columns: usize, input: &[f32]) -> Vec<f32> {
    let row_size = columns / BLOCK_ELEMENTS * BLOCK_BYTES;
    (0..rows)
        .map(|row| {
            let bytes = &matrix[row * row_size..(row + 1) * row_size];
            bytes
                .chunks_exact(BLOCK_BYTES)
                .enumerate()
                .map(|(block, packed)| {
                    let scale = f16_to_f32(u16::from_le_bytes([packed[0], packed[1]]));
                    (0..BLOCK_ELEMENTS)
                        .map(|offset| {
                            let q = if offset < 16 {
                                packed[2 + offset] & 0x0f
                            } else {
                                packed[2 + offset - 16] >> 4
                            };
                            scale * (q as f32 - 8.0) * input[block * BLOCK_ELEMENTS + offset]
                        })
                        .sum::<f32>()
                })
                .sum()
        })
        .collect()
}

fn silu(value: f32) -> f32 {
    value / (1.0 + (-value).exp())
}

fn f16_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let fraction = (bits & 0x03ff) as u32;
    match exponent {
        0 if fraction == 0 => sign * 0.0,
        0 => sign * (fraction as f32 / 1024.0) * 2.0f32.powi(-14),
        31 => {
            if fraction == 0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        exponent => sign * (1.0 + fraction as f32 / 1024.0) * 2.0f32.powi(exponent - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::super::admission::MemoryAccount;
    use super::super::cache::{ExpertCache, FreeMemoryProbe};
    use super::*;
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FixedProbe(AtomicU64);

    impl FixedProbe {
        fn new(bytes: u64) -> Arc<Self> {
            Arc::new(Self(AtomicU64::new(bytes)))
        }
    }

    impl FreeMemoryProbe for FixedProbe {
        fn free_bytes(&self) -> io::Result<u64> {
            Ok(self.0.load(Ordering::Relaxed))
        }
    }

    fn packed_rows(rows: usize, columns: usize, nibble: u8) -> Vec<u8> {
        let row_size = columns / BLOCK_ELEMENTS * BLOCK_BYTES;
        let mut out = vec![0u8; rows * row_size];
        for row in out.chunks_exact_mut(row_size) {
            for block in row.chunks_exact_mut(BLOCK_BYTES) {
                block[..2].copy_from_slice(&0x3c00u16.to_le_bytes());
                for byte in &mut block[2..] {
                    *byte = nibble | (nibble << 4);
                }
            }
        }
        out
    }

    #[test]
    fn q4_0_executa_gate_up_silu_e_down() {
        let hidden = 32;
        let intermediate = 32;
        let gate_up = packed_rows(2 * intermediate, hidden, 9);
        let down = packed_rows(hidden, intermediate, 9);
        let payload = pack_payload(&gate_up, &down).unwrap();
        let expert = Q4_0Expert::from_payload(hidden, intermediate, &payload).unwrap();
        let output = expert.run(&[1.0; 32]).unwrap();
        assert_eq!(output.len(), hidden);
        assert!(output
            .iter()
            .all(|value| value.is_finite() && *value > 30_000.0));
    }

    #[test]
    fn payload_rejeita_geometria_que_nao_fecha_bloco() {
        assert!(matches!(
            Q4_0Expert::packed_bytes(31, 32),
            Err(Q4Error::InvalidGeometry(_))
        ));
    }

    #[test]
    fn executor_reutiliza_o_expert_admitido_no_segundo_forward() {
        let hidden = 32;
        let intermediate = 32;
        let probe = FixedProbe::new(100_000);
        let cache = ExpertCache::new(
            MemoryAccount {
                baseline_free_bytes: 100_000,
                weights_bytes: 0,
                fixed_bytes: 0,
                reserve_bytes: 0,
            },
            probe,
        );
        let executor = Q4_0Executor::new(Arc::clone(&cache), hidden, intermediate).unwrap();
        let gate_up = packed_rows(2 * intermediate, hidden, 9);
        let down = packed_rows(hidden, intermediate, 9);
        let payload = pack_payload(&gate_up, &down).unwrap();
        let key = ExpertKey::new("fixture-moe", 0, 7, "q4_0");
        let first = executor
            .run(key.clone(), || Ok(payload.clone()), &[1.0; 32])
            .unwrap();
        let second = executor
            .run(
                key,
                || panic!("cache hit não deve chamar o loader"),
                &[1.0; 32],
            )
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(cache.stats().admissions, 1);
        assert_eq!(cache.stats().hits, 1);
    }
}
