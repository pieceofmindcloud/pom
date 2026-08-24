//! Executor CPU de experts NVFP4.
//!
//! O layout é o `block_nvfp4` do ggml: quatro escalas UE4M3 por bloco de 64
//! valores, uma para cada subbloco de 16, seguidas por 32 bytes de nibbles
//! E2M1. As escalas `.scale` e `.input_scale` por expert são preservadas no
//! payload do cache, em vez de serem confundidas com os bytes quantizados.

use super::cache::{CacheError, ExpertCache, ExpertKey};
use super::format_q4_0::{parse_tensor_table, Q4LoadError, TensorSpec};
use memmap2::Mmap;
use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

const BLOCK_ELEMENTS: usize = 64;
const SUBBLOCK_ELEMENTS: usize = 16;
const BLOCK_BYTES: usize = 36;
const PAYLOAD_HEADER_BYTES: usize = 32;
const SCALE_COUNT: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nvfp4Error {
    Geometry(&'static str),
    Payload(&'static str),
    PayloadLength { expected: usize, actual: usize },
}

impl fmt::Display for Nvfp4Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(reason) => write!(formatter, "geometria NVFP4 inválida: {reason}"),
            Self::Payload(reason) => write!(formatter, "payload NVFP4 inválido: {reason}"),
            Self::PayloadLength { expected, actual } => write!(
                formatter,
                "payload NVFP4 tem {actual} bytes, mas a geometria exige {expected}"
            ),
        }
    }
}

impl std::error::Error for Nvfp4Error {}

#[derive(Debug)]
pub enum Nvfp4ExecuteError {
    Cache(CacheError),
    Format(Nvfp4Error),
    Source(Q4LoadError),
    HiddenWidth { expected: usize, actual: usize },
}

impl fmt::Display for Nvfp4ExecuteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cache(error) => error.fmt(formatter),
            Self::Format(error) => error.fmt(formatter),
            Self::Source(error) => error.fmt(formatter),
            Self::HiddenWidth { expected, actual } => {
                write!(
                    formatter,
                    "hidden NVFP4 tem {actual} elementos, esperado {expected}"
                )
            }
        }
    }
}

impl std::error::Error for Nvfp4ExecuteError {}

impl From<CacheError> for Nvfp4ExecuteError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<Nvfp4Error> for Nvfp4ExecuteError {
    fn from(error: Nvfp4Error) -> Self {
        Self::Format(error)
    }
}

impl From<Q4LoadError> for Nvfp4ExecuteError {
    fn from(error: Q4LoadError) -> Self {
        Self::Source(error)
    }
}

pub struct Nvfp4Expert<'a> {
    hidden: usize,
    intermediate: usize,
    gate_up: &'a [u8],
    down: &'a [u8],
    gate_scale: f32,
    up_scale: f32,
    down_scale: f32,
    gate_input_scale: f32,
    up_input_scale: f32,
    down_input_scale: f32,
}

impl<'a> Nvfp4Expert<'a> {
    pub fn packed_bytes(hidden: usize, intermediate: usize) -> Result<usize, Nvfp4Error> {
        if hidden == 0 || intermediate == 0 {
            return Err(Nvfp4Error::Geometry("dimensão zero"));
        }
        if !hidden.is_multiple_of(BLOCK_ELEMENTS) || !intermediate.is_multiple_of(BLOCK_ELEMENTS) {
            return Err(Nvfp4Error::Geometry(
                "hidden e intermediate precisam fechar blocos de 64",
            ));
        }
        let gate_up = (2 * intermediate)
            .checked_mul(row_bytes(hidden)?)
            .ok_or(Nvfp4Error::Geometry("overflow em gate_up"))?;
        let down = hidden
            .checked_mul(row_bytes(intermediate)?)
            .ok_or(Nvfp4Error::Geometry("overflow em down"))?;
        PAYLOAD_HEADER_BYTES
            .checked_add(gate_up)
            .and_then(|bytes| bytes.checked_add(down))
            .ok_or(Nvfp4Error::Geometry("overflow no payload"))
    }

    pub fn from_payload(
        hidden: usize,
        intermediate: usize,
        payload: &'a [u8],
    ) -> Result<Self, Nvfp4Error> {
        let expected = Self::packed_bytes(hidden, intermediate)?;
        if payload.len() != expected {
            return Err(Nvfp4Error::PayloadLength {
                expected,
                actual: payload.len(),
            });
        }
        let gate_up_len = u32::from_le_bytes(
            payload[..4]
                .try_into()
                .map_err(|_| Nvfp4Error::Payload("cabeçalho gate_up ausente"))?,
        ) as usize;
        let down_len = u32::from_le_bytes(
            payload[4..8]
                .try_into()
                .map_err(|_| Nvfp4Error::Payload("cabeçalho down ausente"))?,
        ) as usize;
        let expected_gate_up = (2 * intermediate) * row_bytes(hidden)?;
        let expected_down = hidden * row_bytes(intermediate)?;
        if gate_up_len != expected_gate_up || down_len != expected_down {
            return Err(Nvfp4Error::Payload(
                "tamanhos gate_up/down não correspondem à geometria",
            ));
        }
        let mut scales = [1.0f32; SCALE_COUNT];
        for (index, scale) in scales.iter_mut().enumerate() {
            let start = 8 + index * 4;
            *scale = f32::from_le_bytes(
                payload[start..start + 4]
                    .try_into()
                    .map_err(|_| Nvfp4Error::Payload("escala ausente"))?,
            );
            if !scale.is_finite() {
                return Err(Nvfp4Error::Payload("escala não finita"));
            }
        }
        let gate_up_start = PAYLOAD_HEADER_BYTES;
        let down_start = gate_up_start + gate_up_len;
        if down_start + down_len != payload.len() {
            return Err(Nvfp4Error::Payload("seções não cobrem o payload"));
        }
        Ok(Self {
            hidden,
            intermediate,
            gate_up: &payload[gate_up_start..down_start],
            down: &payload[down_start..],
            gate_scale: scales[0],
            up_scale: scales[1],
            down_scale: scales[2],
            gate_input_scale: scales[3],
            up_input_scale: scales[4],
            down_input_scale: scales[5],
        })
    }

    pub fn run(&self, hidden: &[f32]) -> Result<Vec<f32>, Nvfp4ExecuteError> {
        if hidden.len() != self.hidden {
            return Err(Nvfp4ExecuteError::HiddenWidth {
                expected: self.hidden,
                actual: hidden.len(),
            });
        }
        let gate_input: Vec<f32> = hidden
            .iter()
            .map(|value| value * self.gate_input_scale)
            .collect();
        let up_input: Vec<f32> = hidden
            .iter()
            .map(|value| value * self.up_input_scale)
            .collect();
        let gate = mat_vec_nvfp4(
            &self.gate_up[..self.intermediate * row_bytes(self.hidden).unwrap()],
            self.intermediate,
            self.hidden,
            &gate_input,
        )
        .into_iter()
        .map(|value| value * self.gate_scale)
        .collect::<Vec<_>>();
        let up = mat_vec_nvfp4(
            &self.gate_up[self.intermediate * row_bytes(self.hidden).unwrap()..],
            self.intermediate,
            self.hidden,
            &up_input,
        )
        .into_iter()
        .map(|value| value * self.up_scale)
        .collect::<Vec<_>>();
        let gated = gate
            .into_iter()
            .zip(up)
            .map(|(gate, up)| gelu(gate) * up)
            .map(|value| value * self.down_input_scale)
            .collect::<Vec<_>>();
        Ok(
            mat_vec_nvfp4(self.down, self.hidden, self.intermediate, &gated)
                .into_iter()
                .map(|value| value * self.down_scale)
                .collect(),
        )
    }
}

fn pack_payload(
    gate_up: &[u8],
    down: &[u8],
    scales: [f32; SCALE_COUNT],
) -> Result<Vec<u8>, Nvfp4Error> {
    if gate_up.len() > u32::MAX as usize || down.len() > u32::MAX as usize {
        return Err(Nvfp4Error::Payload("bank excede u32"));
    }
    let mut payload = Vec::with_capacity(PAYLOAD_HEADER_BYTES + gate_up.len() + down.len());
    payload.extend_from_slice(&(gate_up.len() as u32).to_le_bytes());
    payload.extend_from_slice(&(down.len() as u32).to_le_bytes());
    for scale in scales {
        payload.extend_from_slice(&scale.to_le_bytes());
    }
    payload.extend_from_slice(gate_up);
    payload.extend_from_slice(down);
    Ok(payload)
}

enum GateUpTensors {
    Fused(Vec<TensorSpec>),
    Split {
        gate: Vec<TensorSpec>,
        up: Vec<TensorSpec>,
    },
}

pub struct Nvfp4Source {
    mmap: Mmap,
    data_start: usize,
    tensors: Vec<TensorSpec>,
    experts: u32,
    hidden: usize,
    intermediate: usize,
    gate_up: GateUpTensors,
    down: Vec<TensorSpec>,
    router: Vec<TensorSpec>,
}

impl Nvfp4Source {
    pub fn open_infer(path: impl AsRef<Path>, experts: u32) -> Result<Self, Q4LoadError> {
        let file = File::open(path.as_ref()).map_err(|error| Q4LoadError::Io(error.to_string()))?;
        // SAFETY: the descriptor remains valid while the mapping is created.
        let mmap =
            unsafe { Mmap::map(&file) }.map_err(|error| Q4LoadError::Io(error.to_string()))?;
        let (_, tensors) = parse_tensor_table(&mmap)?;
        let fused = tensors
            .iter()
            .find(|tensor| tensor.name.ends_with("ffn_gate_up_exps.weight"));
        let (hidden, intermediate, tensor_experts) = if let Some(tensor) = fused {
            ensure_nvfp4(tensor)?;
            if tensor.dims.len() != 3 {
                return Err(Q4LoadError::InvalidTensor(
                    "gate_up NVFP4 precisa ser rank 3",
                ));
            }
            let hidden = dimension(tensor, 0, "hidden")?;
            let two_intermediate = dimension(tensor, 1, "intermediate")?;
            let tensor_experts = dimension_u32(tensor, 2, "experts")?;
            if !two_intermediate.is_multiple_of(2) {
                return Err(Q4LoadError::InvalidTensor(
                    "gate_up NVFP4 tem linhas ímpares",
                ));
            }
            (hidden, two_intermediate / 2, tensor_experts)
        } else {
            let gate = tensors
                .iter()
                .find(|tensor| tensor.name.ends_with("ffn_gate_exps.weight"))
                .ok_or_else(|| Q4LoadError::MissingTensor("ffn_gate_exps.weight".into()))?;
            let up = tensors
                .iter()
                .find(|tensor| tensor.name.ends_with("ffn_up_exps.weight"))
                .ok_or_else(|| Q4LoadError::MissingTensor("ffn_up_exps.weight".into()))?;
            ensure_nvfp4(gate)?;
            ensure_nvfp4(up)?;
            if gate.dims != up.dims || gate.dims.len() != 3 {
                return Err(Q4LoadError::InvalidTensor(
                    "gate e up NVFP4 têm geometrias diferentes",
                ));
            }
            let (hidden, intermediate) = split_geometry(gate)?;
            (hidden, intermediate, dimension_u32(gate, 2, "experts")?)
        };
        if tensor_experts != experts {
            return Err(Q4LoadError::InvalidTensor(
                "quantidade de experts não corresponde aos metadados MoE",
            ));
        }
        drop(mmap);
        Self::open(path, experts, hidden, intermediate)
    }

    fn open(
        path: impl AsRef<Path>,
        experts: u32,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, Q4LoadError> {
        Nvfp4Expert::packed_bytes(hidden, intermediate)
            .map_err(|_| Q4LoadError::InvalidTensor("geometria não fecha NVFP4"))?;
        let file = File::open(path.as_ref()).map_err(|error| Q4LoadError::Io(error.to_string()))?;
        // SAFETY: the file descriptor stays alive until the mmap is created.
        let mmap =
            unsafe { Mmap::map(&file) }.map_err(|error| Q4LoadError::Io(error.to_string()))?;
        let (data_start, tensors) = parse_tensor_table(&mmap)?;
        let mut fused = Vec::new();
        let mut gate = Vec::new();
        let mut up = Vec::new();
        let mut down = Vec::new();
        let mut router = Vec::new();
        for tensor in &tensors {
            if tensor.name.starts_with("blk.") && tensor.name.ends_with("ffn_gate_up_exps.weight") {
                fused.push(tensor.clone());
            } else if tensor.name.starts_with("blk.")
                && tensor.name.ends_with("ffn_gate_exps.weight")
            {
                gate.push(tensor.clone());
            } else if tensor.name.starts_with("blk.") && tensor.name.ends_with("ffn_up_exps.weight")
            {
                up.push(tensor.clone());
            } else if tensor.name.starts_with("blk.")
                && tensor.name.ends_with("ffn_down_exps.weight")
            {
                down.push(tensor.clone());
            } else if tensor.name.starts_with("blk.")
                && tensor.name.ends_with("ffn_gate_inp.weight")
            {
                router.push(tensor.clone());
            }
        }
        if down.is_empty() {
            return Err(Q4LoadError::MissingTensor(
                "blk.<layer>.ffn_down_exps.weight".into(),
            ));
        }
        let gate_up = if !fused.is_empty() {
            if !gate.is_empty() || !up.is_empty() {
                return Err(Q4LoadError::InvalidTensor(
                    "convenções fundida e separada de experts misturadas",
                ));
            }
            GateUpTensors::Fused(fused)
        } else if gate.is_empty() || up.is_empty() {
            return Err(Q4LoadError::MissingTensor(
                "ffn_gate_up_exps.weight ou ffn_gate_exps.weight + ffn_up_exps.weight".into(),
            ));
        } else {
            GateUpTensors::Split { gate, up }
        };
        Ok(Self {
            mmap,
            data_start,
            tensors,
            experts,
            hidden,
            intermediate,
            gate_up,
            down,
            router,
        })
    }

    pub fn hidden(&self) -> usize {
        self.hidden
    }

    pub fn intermediate(&self) -> usize {
        self.intermediate
    }

    pub fn has_router(&self) -> bool {
        !self.router.is_empty()
    }

    pub fn layer_count(&self) -> u32 {
        let tensors = match &self.gate_up {
            GateUpTensors::Fused(tensors) => tensors,
            GateUpTensors::Split { gate, .. } => gate,
        };
        tensors
            .iter()
            .filter_map(|tensor| {
                tensor
                    .name
                    .strip_prefix("blk.")?
                    .split('.')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .max()
            .map_or(0, |last| last.saturating_add(1))
    }

    pub fn load_expert(&self, layer: u32, expert: u32) -> Result<Vec<u8>, Q4LoadError> {
        if expert >= self.experts {
            return Err(Q4LoadError::ExpertOutOfRange {
                expert,
                count: self.experts,
            });
        }
        let down = self.tensor_for_layer(&self.down, layer, "ffn_down_exps.weight")?;
        // GGUF stores matrix dimensions as [input, output, experts]. The
        // down projection is therefore [intermediate, hidden, experts].
        validate_tensor(down, self.experts, self.intermediate, self.hidden)?;
        let down_per_expert = self
            .hidden
            .checked_mul(
                row_bytes(self.intermediate)
                    .map_err(|_| Q4LoadError::InvalidTensor("geometria da linha down NVFP4"))?,
            )
            .ok_or(Q4LoadError::InvalidTensor("overflow em down NVFP4"))?;
        let down_bytes = self.expert_slice(down, expert, down_per_expert)?;
        let (gate_bytes, up_bytes, gate_tensor, up_tensor) = match &self.gate_up {
            GateUpTensors::Fused(tensors) => {
                let tensor = self.tensor_for_layer(tensors, layer, "ffn_gate_up_exps.weight")?;
                validate_tensor(tensor, self.experts, self.hidden, 2 * self.intermediate)?;
                let bytes = (2 * self.intermediate)
                    .checked_mul(row_bytes(self.hidden).map_err(|_| {
                        Q4LoadError::InvalidTensor("geometria da linha gate_up NVFP4")
                    })?)
                    .ok_or(Q4LoadError::InvalidTensor("overflow em gate_up NVFP4"))?;
                let bytes = self.expert_slice(tensor, expert, bytes)?;
                (bytes, &[][..], tensor, tensor)
            }
            GateUpTensors::Split { gate, up } => {
                let gate = self.tensor_for_layer(gate, layer, "ffn_gate_exps.weight")?;
                let up = self.tensor_for_layer(up, layer, "ffn_up_exps.weight")?;
                validate_tensor(gate, self.experts, self.hidden, self.intermediate)?;
                validate_tensor(up, self.experts, self.hidden, self.intermediate)?;
                let bytes =
                    self.intermediate
                        .checked_mul(row_bytes(self.hidden).map_err(|_| {
                            Q4LoadError::InvalidTensor("geometria da linha gate NVFP4")
                        })?)
                        .ok_or(Q4LoadError::InvalidTensor("overflow em gate/up NVFP4"))?;
                (
                    self.expert_slice(gate, expert, bytes)?,
                    self.expert_slice(up, expert, bytes)?,
                    gate,
                    up,
                )
            }
        };
        let mut gate_up = Vec::with_capacity(gate_bytes.len() + up_bytes.len());
        gate_up.extend_from_slice(gate_bytes);
        gate_up.extend_from_slice(up_bytes);
        let scales = [
            self.sidecar_scale(gate_tensor, "scale", expert)?,
            self.sidecar_scale(up_tensor, "scale", expert)?,
            self.sidecar_scale(down, "scale", expert)?,
            self.sidecar_scale(gate_tensor, "input_scale", expert)?,
            self.sidecar_scale(up_tensor, "input_scale", expert)?,
            self.sidecar_scale(down, "input_scale", expert)?,
        ];
        pack_payload(&gate_up, down_bytes, scales)
            .map_err(|_| Q4LoadError::InvalidTensor("payload NVFP4"))
    }

    pub fn router_logits(&self, layer: u32, hidden: &[f32]) -> Result<Vec<f32>, Q4LoadError> {
        if hidden.len() != self.hidden {
            return Err(Q4LoadError::InvalidTensor(
                "hidden não corresponde à largura do router",
            ));
        }
        let tensor = self.tensor_for_layer(&self.router, layer, "ffn_gate_inp.weight")?;
        let elements = (self.experts as u64)
            .checked_mul(self.hidden as u64)
            .ok_or(Q4LoadError::InvalidTensor("overflow no router"))?;
        let declared = tensor
            .dims
            .iter()
            .try_fold(1u64, |total, dimension| total.checked_mul(*dimension))
            .ok_or(Q4LoadError::InvalidTensor(
                "overflow nas dimensões do router",
            ))?;
        if declared != elements || tensor.dims.len() != 2 {
            return Err(Q4LoadError::InvalidTensor(
                "dimensões do router não correspondem ao modelo",
            ));
        }
        let element_bytes = scalar_bytes(tensor.dtype)?;
        let total_bytes = usize::try_from(elements)
            .ok()
            .and_then(|count| count.checked_mul(element_bytes))
            .ok_or(Q4LoadError::InvalidTensor("tamanho do router excede usize"))?;
        let raw = self.tensor_bytes(tensor, total_bytes)?;
        let input_scale = self
            .tensor_sidecar(tensor, "scale")
            .map(|scale| self.read_scale_vector(scale))
            .transpose()?
            .unwrap_or_else(|| vec![1.0; self.hidden]);
        if input_scale.len() != self.hidden {
            return Err(Q4LoadError::InvalidTensor(
                "escala do router tem geometria errada",
            ));
        }
        let mut logits = Vec::with_capacity(self.experts as usize);
        for expert in 0..self.experts as usize {
            let row = &raw
                [expert * self.hidden * element_bytes..(expert + 1) * self.hidden * element_bytes];
            let value = row
                .chunks_exact(element_bytes)
                .zip(hidden.iter().zip(&input_scale))
                .map(|(encoded, (&input, &scale))| {
                    decode_scalar(encoded, tensor.dtype) * input * scale
                })
                .sum();
            logits.push(value);
        }
        Ok(logits)
    }

    fn tensor_for_layer<'a>(
        &self,
        tensors: &'a [TensorSpec],
        layer: u32,
        suffix: &str,
    ) -> Result<&'a TensorSpec, Q4LoadError> {
        tensors
            .iter()
            .find(|tensor| tensor.name == format!("blk.{layer}.{suffix}"))
            .ok_or_else(|| Q4LoadError::MissingTensor(format!("blk.{layer}.{suffix}")))
    }

    fn tensor_sidecar<'a>(&'a self, tensor: &TensorSpec, suffix: &str) -> Option<&'a TensorSpec> {
        let name = format!("{}.{}", tensor.name, suffix);
        self.tensors.iter().find(|candidate| candidate.name == name)
    }

    fn sidecar_scale(
        &self,
        tensor: &TensorSpec,
        suffix: &str,
        expert: u32,
    ) -> Result<f32, Q4LoadError> {
        let Some(sidecar) = self.tensor_sidecar(tensor, suffix) else {
            return Ok(1.0);
        };
        let values = self.read_scale_vector(sidecar)?;
        values
            .get(expert as usize)
            .copied()
            .ok_or(Q4LoadError::InvalidTensor("escala de expert ausente"))
    }

    fn read_scale_vector(&self, tensor: &TensorSpec) -> Result<Vec<f32>, Q4LoadError> {
        let count = tensor
            .dims
            .iter()
            .try_fold(1u64, |total, dimension| total.checked_mul(*dimension))
            .ok_or(Q4LoadError::InvalidTensor("overflow na escala"))?;
        let count = usize::try_from(count)
            .map_err(|_| Q4LoadError::InvalidTensor("escala excede usize"))?;
        let element_bytes = scalar_bytes(tensor.dtype)?;
        let bytes = count
            .checked_mul(element_bytes)
            .ok_or(Q4LoadError::InvalidTensor("bytes da escala excedem usize"))?;
        let raw = self.tensor_bytes(tensor, bytes)?;
        Ok(raw
            .chunks_exact(element_bytes)
            .map(|value| decode_scalar(value, tensor.dtype))
            .collect())
    }

    fn tensor_bytes<'a>(
        &'a self,
        tensor: &TensorSpec,
        total_bytes: usize,
    ) -> Result<&'a [u8], Q4LoadError> {
        let offset = usize::try_from(tensor.offset)
            .ok()
            .and_then(|value| self.data_start.checked_add(value))
            .ok_or(Q4LoadError::InvalidTensor("offset GGUF inválido"))?;
        let end = offset
            .checked_add(total_bytes)
            .ok_or(Q4LoadError::InvalidTensor("tamanho do tensor excede usize"))?;
        self.mmap
            .get(offset..end)
            .ok_or(Q4LoadError::InvalidTensor("tensor excede o arquivo"))
    }

    fn expert_slice<'a>(
        &'a self,
        tensor: &TensorSpec,
        expert: u32,
        bytes_per_expert: usize,
    ) -> Result<&'a [u8], Q4LoadError> {
        ensure_nvfp4(tensor)?;
        let total = bytes_per_expert
            .checked_mul(self.experts as usize)
            .ok_or(Q4LoadError::InvalidTensor("overflow no tensor NVFP4"))?;
        let offset = usize::try_from(tensor.offset)
            .ok()
            .and_then(|value| self.data_start.checked_add(value))
            .and_then(|value| {
                bytes_per_expert
                    .checked_mul(expert as usize)
                    .and_then(|delta| value.checked_add(delta))
            })
            .ok_or(Q4LoadError::InvalidTensor("offset GGUF inválido"))?;
        let end = offset
            .checked_add(bytes_per_expert)
            .ok_or(Q4LoadError::InvalidTensor("fim do tensor excede usize"))?;
        let expert_delta = bytes_per_expert
            .checked_mul(expert as usize)
            .ok_or(Q4LoadError::InvalidTensor("expert excede o tensor"))?;
        let tensor_end = offset
            .checked_sub(expert_delta)
            .and_then(|start| start.checked_add(total))
            .ok_or(Q4LoadError::InvalidTensor("tamanho GGUF inválido"))?;
        if tensor_end > self.mmap.len() || end > tensor_end {
            return Err(Q4LoadError::InvalidTensor("tensor excede o arquivo"));
        }
        Ok(&self.mmap[offset..end])
    }
}

fn ensure_nvfp4(tensor: &TensorSpec) -> Result<(), Q4LoadError> {
    if tensor.dtype != 40 {
        return Err(Q4LoadError::UnsupportedTensorType(tensor.dtype));
    }
    Ok(())
}

fn validate_tensor(
    tensor: &TensorSpec,
    experts: u32,
    rows: usize,
    columns: usize,
) -> Result<(), Q4LoadError> {
    ensure_nvfp4(tensor)?;
    if tensor.dims.len() != 3 || tensor.dims[0] != rows as u64 || tensor.dims[1] != columns as u64 {
        return Err(Q4LoadError::InvalidTensor(
            "dimensões não correspondem ao modelo",
        ));
    }
    if tensor.dims[2] != experts as u64 {
        return Err(Q4LoadError::InvalidTensor("quantidade de experts inválida"));
    }
    Ok(())
}

fn dimension(tensor: &TensorSpec, index: usize, name: &'static str) -> Result<usize, Q4LoadError> {
    tensor
        .dims
        .get(index)
        .copied()
        .ok_or(Q4LoadError::InvalidTensor("rank do tensor NVFP4"))
        .and_then(|value| usize::try_from(value).map_err(|_| Q4LoadError::InvalidTensor(name)))
}

fn dimension_u32(
    tensor: &TensorSpec,
    index: usize,
    name: &'static str,
) -> Result<u32, Q4LoadError> {
    tensor
        .dims
        .get(index)
        .copied()
        .ok_or(Q4LoadError::InvalidTensor("rank do tensor NVFP4"))
        .and_then(|value| u32::try_from(value).map_err(|_| Q4LoadError::InvalidTensor(name)))
}

fn split_geometry(gate: &TensorSpec) -> Result<(usize, usize), Q4LoadError> {
    if gate.dims.len() != 3 {
        return Err(Q4LoadError::InvalidTensor("gate NVFP4 precisa ser rank 3"));
    }
    // GGUF/ggml matrix weights use [input, output, experts]. In Gemma4 the
    // real tensors are [2816, 704, 128], so hidden is dims[0] and the FFN
    // intermediate width is dims[1].
    Ok((
        dimension(gate, 0, "hidden")?,
        dimension(gate, 1, "intermediate")?,
    ))
}

fn row_bytes(columns: usize) -> Result<usize, Nvfp4Error> {
    if !columns.is_multiple_of(BLOCK_ELEMENTS) {
        return Err(Nvfp4Error::Geometry("linha não fecha bloco de 64"));
    }
    columns
        .checked_div(BLOCK_ELEMENTS)
        .and_then(|blocks| blocks.checked_mul(BLOCK_BYTES))
        .ok_or(Nvfp4Error::Geometry("overflow na linha"))
}

fn scalar_bytes(dtype: u32) -> Result<usize, Q4LoadError> {
    match dtype {
        0 => Ok(4),
        1 | 30 => Ok(2),
        other => Err(Q4LoadError::UnsupportedTensorType(other)),
    }
}

fn decode_scalar(encoded: &[u8], dtype: u32) -> f32 {
    match dtype {
        0 => f32::from_le_bytes(encoded.try_into().unwrap()),
        1 => f16_to_f32(u16::from_le_bytes(encoded.try_into().unwrap())),
        30 => f32::from_bits(u32::from(u16::from_le_bytes(encoded.try_into().unwrap())) << 16),
        _ => 0.0,
    }
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

fn mat_vec_nvfp4(matrix: &[u8], rows: usize, columns: usize, input: &[f32]) -> Vec<f32> {
    let row_size = columns / BLOCK_ELEMENTS * BLOCK_BYTES;
    (0..rows)
        .map(|row| {
            let row_bytes = &matrix[row * row_size..(row + 1) * row_size];
            row_bytes
                .chunks_exact(BLOCK_BYTES)
                .enumerate()
                .map(|(block, packed)| {
                    let mut result = 0.0;
                    for subblock in 0..(BLOCK_ELEMENTS / SUBBLOCK_ELEMENTS) {
                        let scale = ue4m3_to_f32(packed[subblock]);
                        let values = &packed[4 + subblock * (SUBBLOCK_ELEMENTS / 2)
                            ..4 + (subblock + 1) * (SUBBLOCK_ELEMENTS / 2)];
                        let input_start = block * BLOCK_ELEMENTS + subblock * SUBBLOCK_ELEMENTS;
                        for (index, &quant) in values.iter().enumerate() {
                            result += scale * fp4_value(quant & 0x0f) * input[input_start + index];
                            result += scale
                                * fp4_value(quant >> 4)
                                * input[input_start + index + SUBBLOCK_ELEMENTS / 2];
                        }
                    }
                    result
                })
                .sum()
        })
        .collect()
}

fn fp4_value(value: u8) -> f32 {
    [
        0.0, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 0.0, -0.5, -1.0, -1.5, -2.0, -3.0, -4.0, -6.0,
    ][value as usize]
}

fn ue4m3_to_f32(value: u8) -> f32 {
    if value == 0 || value == 0x7f {
        return 0.0;
    }
    let exponent = ((value >> 3) & 0x0f) as i32;
    let mantissa = (value & 0x07) as f32;
    let raw = if exponent == 0 {
        mantissa * 2.0f32.powi(-9)
    } else {
        (1.0 + mantissa / 8.0) * 2.0f32.powi(exponent - 7)
    };
    raw * 0.5
}

fn gelu(value: f32) -> f32 {
    0.5 * value * (1.0 + (0.797_884_6 * (value + 0.044_715 * value.powi(3))).tanh())
}

pub struct Nvfp4Executor {
    cache: Arc<ExpertCache>,
    hidden: usize,
    intermediate: usize,
}

impl Nvfp4Executor {
    pub fn new(
        cache: Arc<ExpertCache>,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, Nvfp4Error> {
        Nvfp4Expert::packed_bytes(hidden, intermediate)?;
        Ok(Self {
            cache,
            hidden,
            intermediate,
        })
    }

    pub fn run_from_source(
        &self,
        source: &Nvfp4Source,
        key: ExpertKey,
        layer: u32,
        hidden: &[f32],
    ) -> Result<Vec<f32>, Nvfp4ExecuteError> {
        let expert = key.expert;
        self.run_with_loader(
            key,
            || {
                source
                    .load_expert(layer, expert)
                    .map_err(Nvfp4ExecuteError::Source)
            },
            hidden,
        )
    }

    fn run_with_loader<F>(
        &self,
        key: ExpertKey,
        load: F,
        hidden: &[f32],
    ) -> Result<Vec<f32>, Nvfp4ExecuteError>
    where
        F: FnOnce() -> Result<Vec<u8>, Nvfp4ExecuteError>,
    {
        let lease = match self.cache.lookup(&key) {
            Some(lease) => lease,
            None => self
                .cache
                .admit(key, load()?)
                .map_err(Nvfp4ExecuteError::Cache)?,
        };
        let expert = Nvfp4Expert::from_payload(self.hidden, self.intermediate, lease.bytes())?;
        expert.run(hidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed_rows(rows: usize, columns: usize, nibble: u8) -> Vec<u8> {
        let row_size = columns / BLOCK_ELEMENTS * BLOCK_BYTES;
        let mut output = vec![0u8; rows * row_size];
        for row in output.chunks_exact_mut(row_size) {
            for block in row.chunks_exact_mut(BLOCK_BYTES) {
                block[..4].fill(0x38);
                block[4..].fill(nibble | (nibble << 4));
            }
        }
        output
    }

    #[test]
    fn nvfp4_decodifica_subblocos_ue4m3_e_nibbles_e2m1() {
        let hidden = 64;
        let intermediate = 64;
        let gate_up = packed_rows(2 * intermediate, hidden, 0x22);
        let down = packed_rows(hidden, intermediate, 0x22);
        let payload = pack_payload(&gate_up, &down, [1.0; SCALE_COUNT]).unwrap();
        let expert = Nvfp4Expert::from_payload(hidden, intermediate, &payload).unwrap();
        let output = expert.run(&[1.0; 64]).unwrap();
        assert_eq!(output.len(), hidden);
        assert!(output.iter().all(|value| value.is_finite()));
        assert!(output.iter().all(|value| *value > 0.0));
    }

    #[test]
    fn nvfp4_rejeita_largura_que_nao_fecha_bloco() {
        assert!(matches!(
            Nvfp4Expert::packed_bytes(32, 64),
            Err(Nvfp4Error::Geometry(_))
        ));
    }

    #[test]
    fn nvfp4_split_usa_a_ordem_de_dimensoes_do_gguf() {
        let gate = TensorSpec {
            name: "blk.0.ffn_gate_exps.weight".into(),
            dims: vec![2816, 704, 128],
            dtype: 40,
            offset: 0,
        };

        assert_eq!(split_geometry(&gate).unwrap(), (2816, 704));
    }
}
