//! Inventário e execução de experts com dtype por tensor.
//!
//! Um GGUF UD pode usar, por exemplo, IQ3_S no gate, IQ2_S no up e IQ4_XS no
//! down. Este módulo não escolhe um formato para o arquivo: cada tensor entra
//! no inventário com o seu dtype, recebe o decoder correspondente e ganha uma
//! entrada própria no cache. A convenção de nomes só identifica o papel GGUF
//! do tensor; arquitetura e quantização não são codificadas neste caminho.

use super::cache::{CacheError, ExpertCache, ExpertKey};
use super::format_iq_quants::{row_bytes as iq_row_bytes, IqQuantFormat};
use super::format_k_quants::{row_bytes as k_row_bytes, KQuantFormat};
use super::format_legacy_quants::{decode_row as legacy_decode_row, LegacyQuantFormat};
use super::format_low_bits::{decode_row as low_bit_decode_row, LowBitFormat};
use super::format_plain::PlainFormat;
use super::format_q4_0::{
    decode_row as q4_decode_row, parse_tensor_table, Q4LoadError, TensorSpec,
};
use memmap2::Mmap;
use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TensorRole {
    GateUp,
    Gate,
    Up,
    Down,
}

impl TensorRole {
    fn suffix(self) -> &'static str {
        match self {
            Self::GateUp => "ffn_gate_up_exps.weight",
            Self::Gate => "ffn_gate_exps.weight",
            Self::Up => "ffn_up_exps.weight",
            Self::Down => "ffn_down_exps.weight",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::GateUp => "gate_up",
            Self::Gate => "gate",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

impl fmt::Display for TensorRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TensorDecoder {
    Plain(PlainFormat),
    Q4_0,
    Legacy(LegacyQuantFormat),
    K(KQuantFormat),
    Iq(IqQuantFormat),
    LowBit(LowBitFormat),
}

impl TensorDecoder {
    fn from_tensor(tensor: &TensorSpec) -> Result<Self, Q4LoadError> {
        let decoder = match tensor.dtype {
            0 => Self::Plain(PlainFormat::F32),
            1 => Self::Plain(PlainFormat::F16),
            2 => Self::Q4_0,
            3 => Self::Legacy(LegacyQuantFormat::Q4_1),
            6 => Self::Legacy(LegacyQuantFormat::Q5_0),
            7 => Self::Legacy(LegacyQuantFormat::Q5_1),
            8 => Self::Legacy(LegacyQuantFormat::Q8_0),
            10 => Self::K(KQuantFormat::Q2K),
            11 => Self::K(KQuantFormat::Q3K),
            12 => Self::K(KQuantFormat::Q4K),
            13 => Self::K(KQuantFormat::Q5K),
            14 => Self::K(KQuantFormat::Q6K),
            15 => Self::K(KQuantFormat::Q8K),
            16 => Self::Iq(IqQuantFormat::Iq2XXS),
            17 => Self::Iq(IqQuantFormat::Iq2XS),
            18 => Self::Iq(IqQuantFormat::Iq3XXS),
            19 => Self::Iq(IqQuantFormat::Iq1S),
            20 => Self::Iq(IqQuantFormat::Iq4NL),
            21 => Self::Iq(IqQuantFormat::Iq3S),
            22 => Self::Iq(IqQuantFormat::Iq2S),
            23 => Self::Iq(IqQuantFormat::Iq4XS),
            29 => Self::Iq(IqQuantFormat::Iq1M),
            30 => Self::Plain(PlainFormat::Bf16),
            34 => Self::LowBit(LowBitFormat::Tq1_0),
            35 => Self::LowBit(LowBitFormat::Tq2_0),
            39 => Self::LowBit(LowBitFormat::Mxfp4),
            41 => Self::LowBit(LowBitFormat::Q1_0),
            42 => Self::LowBit(LowBitFormat::Q2_0),
            dtype => {
                return Err(Q4LoadError::UnsupportedTensor {
                    name: tensor.name.clone(),
                    dtype,
                })
            }
        };
        Ok(decoder)
    }

    fn row_bytes(self, columns: usize) -> Result<usize, Q4LoadError> {
        match self {
            Self::Plain(format) => format
                .row_bytes(columns)
                .map_err(|_| Q4LoadError::InvalidTensor("geometria escalar inválida")),
            Self::Q4_0 => super::format_q4_0::row_bytes(columns)
                .map_err(|_| Q4LoadError::InvalidTensor("geometria Q4_0 inválida")),
            Self::Legacy(format) => format
                .row_bytes(columns)
                .map_err(|_| Q4LoadError::InvalidTensor("geometria legacy inválida")),
            Self::K(format) => k_row_bytes(format, columns)
                .map_err(|_| Q4LoadError::InvalidTensor("geometria K-quant inválida")),
            Self::Iq(format) => iq_row_bytes(format, columns)
                .map_err(|_| Q4LoadError::InvalidTensor("geometria IQ-quant inválida")),
            Self::LowBit(format) => format
                .row_bytes(columns)
                .map_err(|_| Q4LoadError::InvalidTensor("geometria low-bit inválida")),
        }
    }

    fn decode_row(self, row: &[u8], columns: usize) -> Result<Vec<f32>, Q4LoadError> {
        match self {
            Self::Plain(format) => format
                .decode_row(row, columns)
                .map_err(|_| Q4LoadError::InvalidTensor("payload escalar inválido")),
            Self::Q4_0 => q4_decode_row(row, columns)
                .map_err(|_| Q4LoadError::InvalidTensor("payload Q4_0 inválido")),
            Self::Legacy(format) => legacy_decode_row(format, row, columns)
                .map_err(|_| Q4LoadError::InvalidTensor("payload legacy inválido")),
            Self::K(format) => super::format_k_quants::dequantize_row(format, row)
                .map_err(|_| Q4LoadError::InvalidTensor("payload K-quant inválido")),
            Self::Iq(format) => super::format_iq_quants::dequantize_row(format, row)
                .map_err(|_| Q4LoadError::InvalidTensor("payload IQ-quant inválido")),
            Self::LowBit(format) => low_bit_decode_row(format, row, columns)
                .map_err(|_| Q4LoadError::InvalidTensor("payload low-bit inválido")),
        }
    }

    fn dtype(self) -> u32 {
        match self {
            Self::Plain(format) => format.dtype(),
            Self::Q4_0 => 2,
            Self::Legacy(format) => format.dtype(),
            Self::K(format) => format.dtype(),
            Self::Iq(format) => format.dtype(),
            Self::LowBit(format) => format.dtype(),
        }
    }
}

#[derive(Clone, Copy)]
struct ExpertTensor {
    spec_index: usize,
    decoder: TensorDecoder,
    role: TensorRole,
}

struct LayerInventory {
    gate_up: Option<ExpertTensor>,
    gate: Option<ExpertTensor>,
    up: Option<ExpertTensor>,
    down: Option<ExpertTensor>,
    hidden: usize,
    intermediate: usize,
}

pub struct HeterogeneousMoeSource {
    mmap: Mmap,
    data_start: usize,
    experts: u32,
    tensors: Vec<TensorSpec>,
    layers: Vec<LayerInventory>,
}

impl HeterogeneousMoeSource {
    pub fn open_infer(path: impl AsRef<Path>, experts: u32) -> Result<Self, Q4LoadError> {
        if experts == 0 {
            return Err(Q4LoadError::InvalidTensor("contagem de experts zero"));
        }
        let file = File::open(path.as_ref()).map_err(|error| Q4LoadError::Io(error.to_string()))?;
        // SAFETY: the file descriptor remains valid while mmap is created.
        let mmap =
            unsafe { Mmap::map(&file) }.map_err(|error| Q4LoadError::Io(error.to_string()))?;
        let (data_start, tensors) = parse_tensor_table(&mmap)?;
        let mut max_layer = None;
        let mut expert_entries = Vec::new();
        for (index, tensor) in tensors.iter().enumerate() {
            let Some(role) = role_for_name(&tensor.name) else {
                continue;
            };
            let layer = layer_for_name(&tensor.name)
                .ok_or(Q4LoadError::InvalidTensor("tensor de expert sem camada"))?;
            let decoder = TensorDecoder::from_tensor(tensor)?;
            max_layer = Some(max_layer.map_or(layer, |last: u32| last.max(layer)));
            expert_entries.push((
                layer,
                ExpertTensor {
                    spec_index: index,
                    decoder,
                    role,
                },
            ));
        }
        let layer_count = max_layer
            .map(|last| last.saturating_add(1) as usize)
            .ok_or_else(|| Q4LoadError::MissingTensor("blk.<layer>.ffn_*_exps.weight".into()))?;
        let mut layers: Vec<Option<LayerInventory>> = (0..layer_count).map(|_| None).collect();
        for layer in 0..layer_count as u32 {
            let find = |role: TensorRole| {
                expert_entries
                    .iter()
                    .find(|(entry_layer, entry)| *entry_layer == layer && entry.role == role)
                    .map(|(_, entry)| *entry)
            };
            let gate_up = find(TensorRole::GateUp);
            let gate = find(TensorRole::Gate);
            let up = find(TensorRole::Up);
            let down = find(TensorRole::Down);
            if gate_up.is_some() && (gate.is_some() || up.is_some()) {
                return Err(Q4LoadError::InvalidTensor(
                    "convenções fundida e separada de experts misturadas",
                ));
            }
            let (hidden, intermediate) = if let Some(entry) = gate_up {
                let tensor = &tensors[entry.spec_index];
                if tensor.dims.len() != 3
                    || tensor.dims[2] != u64::from(experts)
                    || tensor.dims[1] % 2 != 0
                {
                    return Err(Q4LoadError::InvalidTensor(
                        "geometria gate_up heterogênea inválida",
                    ));
                }
                (dimension(tensor, 0)?, dimension(tensor, 1)? / 2)
            } else {
                let gate = gate.ok_or_else(|| {
                    Q4LoadError::MissingTensor(format!("blk.{layer}.ffn_gate_exps.weight"))
                })?;
                let up = up.ok_or_else(|| {
                    Q4LoadError::MissingTensor(format!("blk.{layer}.ffn_up_exps.weight"))
                })?;
                let gate_tensor = &tensors[gate.spec_index];
                let up_tensor = &tensors[up.spec_index];
                if gate_tensor.dims != up_tensor.dims {
                    return Err(Q4LoadError::InvalidTensor(
                        "gate e up têm geometrias diferentes",
                    ));
                }
                (dimension(gate_tensor, 1)?, dimension(gate_tensor, 0)?)
            };
            let down = down.ok_or_else(|| {
                Q4LoadError::MissingTensor(format!("blk.{layer}.ffn_down_exps.weight"))
            })?;
            let down_tensor = &tensors[down.spec_index];
            validate_matrix(down_tensor, experts, hidden, intermediate, down.decoder)?;
            if let Some(entry) = gate_up {
                validate_matrix(
                    &tensors[entry.spec_index],
                    experts,
                    2 * intermediate,
                    hidden,
                    entry.decoder,
                )?;
            } else {
                let gate = gate.ok_or(Q4LoadError::InvalidTensor("gate ausente"))?;
                let up = up.ok_or(Q4LoadError::InvalidTensor("up ausente"))?;
                validate_matrix(
                    &tensors[gate.spec_index],
                    experts,
                    intermediate,
                    hidden,
                    gate.decoder,
                )?;
                validate_matrix(
                    &tensors[up.spec_index],
                    experts,
                    intermediate,
                    hidden,
                    up.decoder,
                )?;
            }
            layers[layer as usize] = Some(LayerInventory {
                gate_up,
                gate,
                up,
                down: Some(down),
                hidden,
                intermediate,
            });
        }
        let layers = layers
            .into_iter()
            .enumerate()
            .map(|(layer, inventory)| {
                inventory.ok_or_else(|| {
                    Q4LoadError::MissingTensor(format!("camada MoE {layer} sem experts"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            mmap,
            data_start,
            experts,
            tensors,
            layers,
        })
    }

    pub fn hidden(&self) -> usize {
        self.layers[0].hidden
    }

    pub fn experts(&self) -> u32 {
        self.experts
    }

    pub fn layer_count(&self) -> u32 {
        self.layers.len() as u32
    }

    pub fn layer_geometry(&self, layer: u32) -> Result<(usize, usize), Q4LoadError> {
        let inventory = self
            .layers
            .get(layer as usize)
            .ok_or(Q4LoadError::InvalidTensor(
                "camada MoE fora do inventário heterogêneo",
            ))?;
        Ok((inventory.hidden, inventory.intermediate))
    }

    pub fn validate_metadata(
        &self,
        layer: u32,
        metadata: &[(TensorRole, &str, u32)],
    ) -> Result<(), Q4LoadError> {
        let inventory = self
            .layers
            .get(layer as usize)
            .ok_or(Q4LoadError::InvalidTensor(
                "camada MoE fora do inventário heterogêneo",
            ))?;
        for (role, name, dtype) in metadata {
            let expected = self.entry_for_role(inventory, *role).ok_or_else(|| {
                Q4LoadError::MissingTensor(format!("tensor {role} ausente no inventário"))
            })?;
            let tensor = &self.tensors[expected.spec_index];
            if tensor.name != *name || expected.decoder.dtype() != *dtype {
                return Err(Q4LoadError::TensorMetadataMismatch {
                    name: (*name).to_string(),
                    dtype: *dtype,
                    expected_name: tensor.name.clone(),
                    expected_dtype: expected.decoder.dtype(),
                });
            }
        }
        Ok(())
    }

    fn entry_for_role(&self, inventory: &LayerInventory, role: TensorRole) -> Option<ExpertTensor> {
        match role {
            TensorRole::GateUp => inventory.gate_up,
            TensorRole::Gate => inventory.gate,
            TensorRole::Up => inventory.up,
            TensorRole::Down => inventory.down,
        }
    }

    fn tensor_slice(
        &self,
        entry: ExpertTensor,
        expert: u32,
        rows: usize,
        columns: usize,
    ) -> Result<Vec<u8>, Q4LoadError> {
        if expert >= self.experts {
            return Err(Q4LoadError::ExpertOutOfRange {
                expert,
                count: self.experts,
            });
        }
        let tensor = &self.tensors[entry.spec_index];
        let row_bytes = entry.decoder.row_bytes(columns)?;
        let bytes_per_expert = rows
            .checked_mul(row_bytes)
            .ok_or(Q4LoadError::InvalidTensor("overflow no tensor heterogêneo"))?;
        let total = bytes_per_expert
            .checked_mul(self.experts as usize)
            .ok_or(Q4LoadError::InvalidTensor("overflow no banco de experts"))?;
        let start = usize::try_from(tensor.offset)
            .ok()
            .and_then(|offset| self.data_start.checked_add(offset))
            .and_then(|offset| offset.checked_add(bytes_per_expert * expert as usize))
            .ok_or(Q4LoadError::InvalidTensor("offset GGUF inválido"))?;
        let end = start
            .checked_add(bytes_per_expert)
            .ok_or(Q4LoadError::InvalidTensor("fim do tensor excede usize"))?;
        let tensor_end = start
            .checked_sub(bytes_per_expert * expert as usize)
            .and_then(|begin| begin.checked_add(total))
            .ok_or(Q4LoadError::InvalidTensor("tamanho do tensor excede usize"))?;
        if tensor_end > self.mmap.len() || end > tensor_end {
            return Err(Q4LoadError::InvalidTensor(
                "tensor de expert excede o arquivo",
            ));
        }
        Ok(self.mmap[start..end].to_vec())
    }

    fn matrix(
        &self,
        cache: &Arc<ExpertCache>,
        model: &str,
        layer: u32,
        expert: u32,
        entry: ExpertTensor,
        rows: usize,
        columns: usize,
    ) -> Result<Vec<f32>, String> {
        let tensor = &self.tensors[entry.spec_index];
        let key = ExpertKey::new(
            model,
            layer,
            expert,
            format!("role={};dtype={}", entry.role, entry.decoder.dtype()),
        );
        let lease = match cache.lookup(&key) {
            Some(lease) => lease,
            None => cache
                .admit(
                    key,
                    self.tensor_slice(entry, expert, rows, columns)
                        .map_err(|e| format!("tensor {}: {e}", tensor.name))?,
                )
                .map_err(|error: CacheError| error.to_string())?,
        };
        let row_bytes = entry
            .decoder
            .row_bytes(columns)
            .map_err(|error| error.to_string())?;
        let values = lease.bytes();
        (0..rows)
            .map(|row| {
                let start = row * row_bytes;
                let end = start + row_bytes;
                entry
                    .decoder
                    .decode_row(&values[start..end], columns)
                    .map_err(|error| format!("tensor {}: {error}", tensor.name))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|rows| rows.into_iter().flatten().collect())
    }

    pub fn run_expert(
        &self,
        cache: &Arc<ExpertCache>,
        model: &str,
        layer: u32,
        expert: u32,
        hidden: &[f32],
        activation: i32,
    ) -> Result<Vec<f32>, String> {
        let inventory = self
            .layers
            .get(layer as usize)
            .ok_or_else(|| "camada MoE fora do inventário heterogêneo".to_string())?;
        let (hidden_width, intermediate) = (inventory.hidden, inventory.intermediate);
        if hidden.len() != hidden_width {
            return Err(format!(
                "hidden heterogêneo tem {}, esperado {}",
                hidden.len(),
                hidden_width
            ));
        }
        let (gate, up) = if let Some(entry) = inventory.gate_up {
            let values = self.matrix(
                cache,
                model,
                layer,
                expert,
                entry,
                2 * intermediate,
                hidden_width,
            )?;
            (
                values[..intermediate].to_vec(),
                values[intermediate..].to_vec(),
            )
        } else {
            let gate_entry = inventory.gate.ok_or("gate ausente")?;
            let up_entry = inventory.up.ok_or("up ausente")?;
            (
                mat_vec(
                    &self.matrix(
                        cache,
                        model,
                        layer,
                        expert,
                        gate_entry,
                        intermediate,
                        hidden_width,
                    )?,
                    intermediate,
                    hidden_width,
                    hidden,
                ),
                mat_vec(
                    &self.matrix(
                        cache,
                        model,
                        layer,
                        expert,
                        up_entry,
                        intermediate,
                        hidden_width,
                    )?,
                    intermediate,
                    hidden_width,
                    hidden,
                ),
            )
        };
        let gated = gate
            .into_iter()
            .zip(up)
            .map(|(gate, up)| {
                Ok(match activation {
                    1 => silu(gate) * up,
                    2 => gelu(gate) * up,
                    other => return Err(format!("ativação MoE não suportada: {other}")),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let down = inventory.down.ok_or("down ausente")?;
        let down_matrix = self.matrix(
            cache,
            model,
            layer,
            expert,
            down,
            hidden_width,
            intermediate,
        )?;
        Ok(mat_vec(&down_matrix, hidden_width, intermediate, &gated))
    }

    pub fn run_from_source_with_route(
        &self,
        cache: &Arc<ExpertCache>,
        model: &str,
        layer: u32,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
        activation: i32,
    ) -> Result<Vec<f32>, String> {
        if ids.is_empty() || ids.len() != weights.len() {
            return Err("rota heterogênea sem ids/pesos correspondentes".into());
        }
        let mut output = vec![0.0; self.layer_geometry(layer).map_err(|e| e.to_string())?.0];
        for (&expert, &weight) in ids.iter().zip(weights) {
            if !weight.is_finite() {
                return Err("peso de rota heterogênea não finito".into());
            }
            let values = self.run_expert(cache, model, layer, expert, hidden, activation)?;
            for (out, value) in output.iter_mut().zip(values) {
                *out += weight * value;
            }
        }
        Ok(output)
    }
}

fn role_for_name(name: &str) -> Option<TensorRole> {
    [
        TensorRole::GateUp,
        TensorRole::Gate,
        TensorRole::Up,
        TensorRole::Down,
    ]
    .into_iter()
    .find(|role| name.starts_with("blk.") && name.ends_with(role.suffix()))
}

fn layer_for_name(name: &str) -> Option<u32> {
    name.strip_prefix("blk.")?.split('.').next()?.parse().ok()
}

fn dimension(tensor: &TensorSpec, index: usize) -> Result<usize, Q4LoadError> {
    usize::try_from(
        *tensor
            .dims
            .get(index)
            .ok_or(Q4LoadError::InvalidTensor("dimensão ausente"))?,
    )
    .map_err(|_| Q4LoadError::InvalidTensor("dimensão excede usize"))
}

fn validate_matrix(
    tensor: &TensorSpec,
    experts: u32,
    rows: usize,
    columns: usize,
    decoder: TensorDecoder,
) -> Result<(), Q4LoadError> {
    if tensor.dims.as_slice() != [columns as u64, rows as u64, experts as u64] {
        return Err(Q4LoadError::InvalidTensor(
            "dimensões não correspondem ao tensor de experts",
        ));
    }
    decoder.row_bytes(columns)?;
    Ok(())
}

fn mat_vec(matrix: &[f32], rows: usize, columns: usize, input: &[f32]) -> Vec<f32> {
    (0..rows)
        .map(|row| {
            matrix[row * columns..(row + 1) * columns]
                .iter()
                .zip(input)
                .map(|(weight, value)| weight * value)
                .sum()
        })
        .collect()
}

fn silu(value: f32) -> f32 {
    value / (1.0 + (-value).exp())
}

fn gelu(value: f32) -> f32 {
    0.5 * value * (1.0 + (0.797_884_6 * (value + 0.044_715 * value.powi(3))).tanh())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    #[ignore = "requer o GGUF real Qwen3.6-35B-A3B-UD-IQ3_S"]
    fn qwen_ud_iq3_s_real_inventaria_dtypes_por_tensor() {
        let path = std::env::var_os("POM_REAL_MOE_GGUF")
            .expect("POM_REAL_MOE_GGUF deve apontar para o GGUF real");
        let source = HeterogeneousMoeSource::open_infer(path, 128)
            .expect("o GGUF real UD-IQ3_S deve abrir com inventário heterogêneo");
        assert_eq!(source.experts(), 128);
        assert_eq!(source.layer_count(), 40);
        let dtypes = source
            .tensors
            .iter()
            .filter(|tensor| role_for_name(&tensor.name).is_some())
            .map(|tensor| tensor.dtype)
            .collect::<BTreeSet<_>>();
        assert_eq!(dtypes, BTreeSet::from([21, 22, 23]));
        for layer in 0..source.layer_count() {
            let (hidden, intermediate) = source
                .layer_geometry(layer)
                .expect("cada camada deve ter geometria heterogênea");
            assert!(hidden > 0);
            assert!(intermediate > 0);
        }
    }
}
