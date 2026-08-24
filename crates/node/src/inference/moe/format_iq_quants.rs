//! Loader e executor dos IQ-quants priorizados no corpus real.
//!
//! A política de cache, o parser GGUF, o router e a multiplicação matricial
//! continuam neste motor. A expansão de um bloco IQ usa as primitivas C de
//! dequantização do GGML já linkadas pelo `llama-cpp-sys-2`; isso mantém os
//! codebooks oficiais do formato sem copiar suas tabelas para este projeto.

use super::cache::{CacheError, ExpertCache, ExpertKey};
use super::format_q4_0::{parse_tensor_table, Q4LoadError, TensorSpec};
use memmap2::Mmap;
use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

const BLOCK_ELEMENTS: usize = 256;
const PAYLOAD_HEADER_BYTES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqQuantFormat {
    Iq3S,
    Iq2S,
    Iq4XS,
}

impl IqQuantFormat {
    pub fn from_dtype(dtype: u32) -> Result<Self, Q4LoadError> {
        match dtype {
            21 => Ok(Self::Iq3S),
            22 => Ok(Self::Iq2S),
            23 => Ok(Self::Iq4XS),
            other => Err(Q4LoadError::UnsupportedTensorType(other)),
        }
    }

    pub fn dtype(self) -> u32 {
        match self {
            Self::Iq3S => 21,
            Self::Iq2S => 22,
            Self::Iq4XS => 23,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Iq3S => "iq3_s",
            Self::Iq2S => "iq2_s",
            Self::Iq4XS => "iq4_xs",
        }
    }

    fn block_bytes(self) -> usize {
        match self {
            Self::Iq3S => 110,
            Self::Iq2S => 82,
            Self::Iq4XS => 136,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IqQuantError {
    Geometry(&'static str),
    Payload(&'static str),
    PayloadLength { expected: usize, actual: usize },
}

impl fmt::Display for IqQuantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(reason) => write!(formatter, "geometria IQ-quant inválida: {reason}"),
            Self::Payload(reason) => write!(formatter, "payload IQ-quant inválido: {reason}"),
            Self::PayloadLength { expected, actual } => write!(
                formatter,
                "payload IQ-quant tem {actual} bytes, mas a geometria exige {expected}"
            ),
        }
    }
}

impl std::error::Error for IqQuantError {}

#[derive(Debug)]
pub enum IqQuantExecuteError {
    Cache(CacheError),
    Format(IqQuantError),
    Source(Q4LoadError),
    HiddenWidth { expected: usize, actual: usize },
}

impl fmt::Display for IqQuantExecuteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cache(error) => error.fmt(formatter),
            Self::Format(error) => error.fmt(formatter),
            Self::Source(error) => error.fmt(formatter),
            Self::HiddenWidth { expected, actual } => {
                write!(
                    formatter,
                    "hidden IQ-quant tem {actual} elementos, esperado {expected}"
                )
            }
        }
    }
}

impl std::error::Error for IqQuantExecuteError {}

impl From<CacheError> for IqQuantExecuteError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<IqQuantError> for IqQuantExecuteError {
    fn from(error: IqQuantError) -> Self {
        Self::Format(error)
    }
}

impl From<Q4LoadError> for IqQuantExecuteError {
    fn from(error: Q4LoadError) -> Self {
        Self::Source(error)
    }
}

pub struct IqQuantExpert<'a> {
    format: IqQuantFormat,
    hidden: usize,
    intermediate: usize,
    gate_up: &'a [u8],
    down: &'a [u8],
}

impl<'a> IqQuantExpert<'a> {
    pub fn packed_bytes(
        format: IqQuantFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<usize, IqQuantError> {
        let gate_up = (2 * intermediate)
            .checked_mul(row_bytes(format, hidden)?)
            .ok_or(IqQuantError::Geometry("overflow em gate_up"))?;
        let down = hidden
            .checked_mul(row_bytes(format, intermediate)?)
            .ok_or(IqQuantError::Geometry("overflow em down"))?;
        PAYLOAD_HEADER_BYTES
            .checked_add(gate_up)
            .and_then(|bytes| bytes.checked_add(down))
            .ok_or(IqQuantError::Geometry("overflow no payload"))
    }

    pub fn from_payload(
        format: IqQuantFormat,
        hidden: usize,
        intermediate: usize,
        payload: &'a [u8],
    ) -> Result<Self, IqQuantError> {
        let expected = Self::packed_bytes(format, hidden, intermediate)?;
        if payload.len() != expected {
            return Err(IqQuantError::PayloadLength {
                expected,
                actual: payload.len(),
            });
        }
        let gate_up_len = u32::from_le_bytes(
            payload[..4]
                .try_into()
                .map_err(|_| IqQuantError::Payload("cabeçalho gate_up ausente"))?,
        ) as usize;
        let down_len = u32::from_le_bytes(
            payload[4..8]
                .try_into()
                .map_err(|_| IqQuantError::Payload("cabeçalho down ausente"))?,
        ) as usize;
        let expected_gate_up = (2 * intermediate) * row_bytes(format, hidden)?;
        let expected_down = hidden * row_bytes(format, intermediate)?;
        if gate_up_len != expected_gate_up || down_len != expected_down {
            return Err(IqQuantError::Payload(
                "tamanhos gate_up/down não correspondem à geometria",
            ));
        }
        let gate_up_start = PAYLOAD_HEADER_BYTES;
        let down_start = gate_up_start + gate_up_len;
        if down_start + down_len != payload.len() {
            return Err(IqQuantError::Payload("seções não cobrem o payload"));
        }
        Ok(Self {
            format,
            hidden,
            intermediate,
            gate_up: &payload[gate_up_start..down_start],
            down: &payload[down_start..],
        })
    }

    pub fn run(&self, hidden: &[f32]) -> Result<Vec<f32>, IqQuantExecuteError> {
        if hidden.len() != self.hidden {
            return Err(IqQuantExecuteError::HiddenWidth {
                expected: self.hidden,
                actual: hidden.len(),
            });
        }
        let gate_up = mat_vec(
            self.format,
            self.gate_up,
            2 * self.intermediate,
            self.hidden,
            hidden,
        )?;
        let gated: Vec<f32> = gate_up[..self.intermediate]
            .iter()
            .zip(&gate_up[self.intermediate..])
            .map(|(&gate, &up)| silu(gate) * up)
            .collect();
        mat_vec(
            self.format,
            self.down,
            self.hidden,
            self.intermediate,
            &gated,
        )
        .map_err(Into::into)
    }
}

fn pack_payload(gate_up: &[u8], down: &[u8]) -> Result<Vec<u8>, Q4LoadError> {
    if gate_up.len() > u32::MAX as usize || down.len() > u32::MAX as usize {
        return Err(Q4LoadError::InvalidTensor("payload IQ-quant excede u32"));
    }
    let mut payload = Vec::with_capacity(PAYLOAD_HEADER_BYTES + gate_up.len() + down.len());
    payload.extend_from_slice(&(gate_up.len() as u32).to_le_bytes());
    payload.extend_from_slice(&(down.len() as u32).to_le_bytes());
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

pub struct IqQuantSource {
    mmap: Mmap,
    data_start: usize,
    experts: u32,
    hidden: usize,
    intermediate: usize,
    format: IqQuantFormat,
    gate_up: GateUpTensors,
    down: Vec<TensorSpec>,
    router: Vec<TensorSpec>,
}

impl IqQuantSource {
    pub fn open_infer(path: impl AsRef<Path>, experts: u32) -> Result<Self, Q4LoadError> {
        let file = File::open(path.as_ref()).map_err(|error| Q4LoadError::Io(error.to_string()))?;
        // SAFETY: the descriptor remains valid while the mapping is created.
        let mmap =
            unsafe { Mmap::map(&file) }.map_err(|error| Q4LoadError::Io(error.to_string()))?;
        let (_, tensors) = parse_tensor_table(&mmap)?;
        let fused = tensors
            .iter()
            .find(|tensor| tensor.name.ends_with("ffn_gate_up_exps.weight"));
        let (format, hidden, intermediate, tensor_experts) = if let Some(tensor) = fused {
            let format = IqQuantFormat::from_dtype(tensor.dtype)?;
            if tensor.dims.len() != 3 {
                return Err(Q4LoadError::InvalidTensor(
                    "gate_up IQ-quant precisa ser rank 3",
                ));
            }
            let hidden = dimension(tensor, 0, "hidden")?;
            let two_intermediate = dimension(tensor, 1, "intermediate")?;
            let tensor_experts = dimension_u32(tensor, 2, "experts")?;
            if !two_intermediate.is_multiple_of(2) {
                return Err(Q4LoadError::InvalidTensor(
                    "gate_up IQ-quant tem linhas ímpares",
                ));
            }
            (format, hidden, two_intermediate / 2, tensor_experts)
        } else {
            let gate = tensors
                .iter()
                .find(|tensor| tensor.name.ends_with("ffn_gate_exps.weight"))
                .ok_or_else(|| Q4LoadError::MissingTensor("ffn_gate_exps.weight".into()))?;
            let up = tensors
                .iter()
                .find(|tensor| tensor.name.ends_with("ffn_up_exps.weight"))
                .ok_or_else(|| Q4LoadError::MissingTensor("ffn_up_exps.weight".into()))?;
            let format = IqQuantFormat::from_dtype(gate.dtype)?;
            if up.dtype != gate.dtype || gate.dims != up.dims || gate.dims.len() != 3 {
                return Err(Q4LoadError::InvalidTensor(
                    "gate e up IQ-quant têm geometrias ou dtypes diferentes",
                ));
            }
            (
                format,
                dimension(gate, 1, "hidden")?,
                dimension(gate, 0, "intermediate")?,
                dimension_u32(gate, 2, "experts")?,
            )
        };
        if tensor_experts != experts {
            return Err(Q4LoadError::InvalidTensor(
                "quantidade de experts não corresponde aos metadados MoE",
            ));
        }
        drop(mmap);
        Self::open(path, experts, format, hidden, intermediate)
    }

    fn open(
        path: impl AsRef<Path>,
        experts: u32,
        format: IqQuantFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, Q4LoadError> {
        IqQuantExpert::packed_bytes(format, hidden, intermediate)
            .map_err(|_| Q4LoadError::InvalidTensor("geometria não fecha IQ-quant"))?;
        if experts == 0 {
            return Err(Q4LoadError::InvalidTensor("contagem de experts zero"));
        }
        let file = File::open(path.as_ref()).map_err(|error| Q4LoadError::Io(error.to_string()))?;
        // SAFETY: Mmap owns the mapping for the lifetime of IqQuantSource.
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
            experts,
            hidden,
            intermediate,
            format,
            gate_up,
            down,
            router,
        })
    }

    pub fn format(&self) -> IqQuantFormat {
        self.format
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
        validate_tensor(
            down,
            self.format,
            self.experts,
            self.hidden,
            self.intermediate,
        )?;
        let down_per_expert = self
            .hidden
            .checked_mul(
                row_bytes(self.format, self.intermediate)
                    .map_err(|_| Q4LoadError::InvalidTensor("geometria da linha down IQ-quant"))?,
            )
            .ok_or(Q4LoadError::InvalidTensor("overflow em down IQ-quant"))?;
        let down_bytes = self.expert_slice(down, expert, down_per_expert)?;
        let (gate_bytes, up_bytes) = match &self.gate_up {
            GateUpTensors::Fused(tensors) => {
                let tensor = self.tensor_for_layer(tensors, layer, "ffn_gate_up_exps.weight")?;
                validate_tensor(
                    tensor,
                    self.format,
                    self.experts,
                    2 * self.intermediate,
                    self.hidden,
                )?;
                let bytes = (2 * self.intermediate)
                    .checked_mul(row_bytes(self.format, self.hidden).map_err(|_| {
                        Q4LoadError::InvalidTensor("geometria da linha gate_up IQ-quant")
                    })?)
                    .ok_or(Q4LoadError::InvalidTensor("overflow em gate_up IQ-quant"))?;
                (
                    self.expert_slice(tensor, expert, bytes)?.to_vec(),
                    Vec::new(),
                )
            }
            GateUpTensors::Split { gate, up } => {
                let gate = self.tensor_for_layer(gate, layer, "ffn_gate_exps.weight")?;
                let up = self.tensor_for_layer(up, layer, "ffn_up_exps.weight")?;
                validate_tensor(
                    gate,
                    self.format,
                    self.experts,
                    self.intermediate,
                    self.hidden,
                )?;
                validate_tensor(
                    up,
                    self.format,
                    self.experts,
                    self.intermediate,
                    self.hidden,
                )?;
                let bytes = self
                    .intermediate
                    .checked_mul(row_bytes(self.format, self.hidden).map_err(|_| {
                        Q4LoadError::InvalidTensor("geometria da linha gate IQ-quant")
                    })?)
                    .ok_or(Q4LoadError::InvalidTensor("overflow em gate/up IQ-quant"))?;
                (
                    self.expert_slice(gate, expert, bytes)?.to_vec(),
                    self.expert_slice(up, expert, bytes)?.to_vec(),
                )
            }
        };
        let mut gate_up = Vec::with_capacity(gate_bytes.len() + up_bytes.len());
        gate_up.extend_from_slice(&gate_bytes);
        gate_up.extend_from_slice(&up_bytes);
        pack_payload(&gate_up, down_bytes)
    }

    pub fn router_logits(&self, layer: u32, hidden: &[f32]) -> Result<Vec<f32>, Q4LoadError> {
        if hidden.len() != self.hidden {
            return Err(Q4LoadError::InvalidTensor(
                "hidden não corresponde à largura do router",
            ));
        }
        let tensor = self.tensor_for_layer(&self.router, layer, "ffn_gate_inp.weight")?;
        if tensor.dims.len() != 2
            || tensor.dims[0] != self.hidden as u64
            || tensor.dims[1] != self.experts as u64
        {
            return Err(Q4LoadError::InvalidTensor(
                "dimensões do router não correspondem ao modelo",
            ));
        }
        let elements = (self.experts as u64)
            .checked_mul(self.hidden as u64)
            .ok_or(Q4LoadError::InvalidTensor("overflow no router"))?;
        let element_bytes = scalar_bytes(tensor.dtype)?;
        let total_bytes = usize::try_from(elements)
            .ok()
            .and_then(|count| count.checked_mul(element_bytes))
            .ok_or(Q4LoadError::InvalidTensor("tamanho do router excede usize"))?;
        let raw = self.tensor_bytes(tensor, total_bytes)?;
        let mut logits = Vec::with_capacity(self.experts as usize);
        for expert in 0..self.experts as usize {
            let row = &raw
                [expert * self.hidden * element_bytes..(expert + 1) * self.hidden * element_bytes];
            logits.push(
                row.chunks_exact(element_bytes)
                    .zip(hidden)
                    .map(|(encoded, &input)| decode_scalar(encoded, tensor.dtype) * input)
                    .sum(),
            );
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
        validate_format(tensor, self.format)?;
        let total = bytes_per_expert
            .checked_mul(self.experts as usize)
            .ok_or(Q4LoadError::InvalidTensor("overflow no tensor IQ-quant"))?;
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

fn validate_format(tensor: &TensorSpec, format: IqQuantFormat) -> Result<(), Q4LoadError> {
    if tensor.dtype != format.dtype() {
        return Err(Q4LoadError::UnsupportedTensorType(tensor.dtype));
    }
    Ok(())
}

fn validate_tensor(
    tensor: &TensorSpec,
    format: IqQuantFormat,
    experts: u32,
    rows: usize,
    columns: usize,
) -> Result<(), Q4LoadError> {
    validate_format(tensor, format)?;
    if tensor.dims.len() != 3
        || tensor.dims[0] != columns as u64
        || tensor.dims[1] != rows as u64
        || tensor.dims[2] != experts as u64
    {
        return Err(Q4LoadError::InvalidTensor(
            "dimensões não correspondem ao modelo",
        ));
    }
    Ok(())
}

fn dimension(tensor: &TensorSpec, index: usize, name: &'static str) -> Result<usize, Q4LoadError> {
    tensor
        .dims
        .get(index)
        .copied()
        .ok_or(Q4LoadError::InvalidTensor("rank do tensor IQ-quant"))
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
        .ok_or(Q4LoadError::InvalidTensor("rank do tensor IQ-quant"))
        .and_then(|value| u32::try_from(value).map_err(|_| Q4LoadError::InvalidTensor(name)))
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

pub struct IqQuantExecutor {
    cache: Arc<ExpertCache>,
    format: IqQuantFormat,
    hidden: usize,
    intermediate: usize,
}

impl IqQuantExecutor {
    pub fn new(
        cache: Arc<ExpertCache>,
        format: IqQuantFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, IqQuantError> {
        IqQuantExpert::packed_bytes(format, hidden, intermediate)?;
        Ok(Self {
            cache,
            format,
            hidden,
            intermediate,
        })
    }

    pub fn run_from_source(
        &self,
        source: &IqQuantSource,
        key: ExpertKey,
        layer: u32,
        hidden: &[f32],
    ) -> Result<Vec<f32>, IqQuantExecuteError> {
        let expert = key.expert;
        let lease = match self.cache.lookup(&key) {
            Some(lease) => lease,
            None => self
                .cache
                .admit(
                    key,
                    source
                        .load_expert(layer, expert)
                        .map_err(IqQuantExecuteError::Source)?,
                )
                .map_err(IqQuantExecuteError::Cache)?,
        };
        IqQuantExpert::from_payload(self.format, self.hidden, self.intermediate, lease.bytes())?
            .run(hidden)
    }
}

fn row_bytes(format: IqQuantFormat, columns: usize) -> Result<usize, IqQuantError> {
    if columns == 0 || !columns.is_multiple_of(BLOCK_ELEMENTS) {
        return Err(IqQuantError::Geometry(
            "dimensão precisa fechar blocos de 256",
        ));
    }
    columns
        .checked_div(BLOCK_ELEMENTS)
        .and_then(|blocks| blocks.checked_mul(format.block_bytes()))
        .ok_or(IqQuantError::Geometry("overflow na linha"))
}

fn mat_vec(
    format: IqQuantFormat,
    matrix: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
) -> Result<Vec<f32>, IqQuantError> {
    if input.len() != columns {
        return Err(IqQuantError::Geometry("entrada não corresponde às colunas"));
    }
    let row_size = row_bytes(format, columns)?;
    let expected = rows
        .checked_mul(row_size)
        .ok_or(IqQuantError::Geometry("overflow na matriz"))?;
    if matrix.len() != expected {
        return Err(IqQuantError::Payload("tamanho da matriz não corresponde"));
    }
    (0..rows)
        .map(|row| {
            let decoded = dequantize_row(format, &matrix[row * row_size..(row + 1) * row_size])?;
            Ok(decoded
                .iter()
                .zip(input)
                .map(|(&weight, &value)| weight * value)
                .sum())
        })
        .collect()
}

#[repr(C)]
struct BlockIq2S {
    d: u16,
    qs: [u8; 64],
    qh: [u8; 8],
    scales: [u8; 8],
}

#[repr(C)]
struct BlockIq3S {
    d: u16,
    qs: [u8; 64],
    qh: [u8; 8],
    signs: [u8; 32],
    scales: [u8; 4],
}

#[repr(C)]
struct BlockIq4Xs {
    d: u16,
    scales_h: u16,
    scales_l: [u8; 4],
    qs: [u8; 128],
}

unsafe extern "C" {
    fn dequantize_row_iq2_s(x: *const BlockIq2S, y: *mut f32, k: i64);
    fn dequantize_row_iq3_s(x: *const BlockIq3S, y: *mut f32, k: i64);
    fn dequantize_row_iq4_xs(x: *const BlockIq4Xs, y: *mut f32, k: i64);
}

pub fn dequantize_row(format: IqQuantFormat, packed: &[u8]) -> Result<Vec<f32>, IqQuantError> {
    let block_bytes = format.block_bytes();
    if packed.len() < block_bytes || !packed.len().is_multiple_of(block_bytes) {
        return Err(IqQuantError::PayloadLength {
            expected: block_bytes,
            actual: packed.len(),
        });
    }
    let blocks = packed.len() / block_bytes;
    let elements = blocks
        .checked_mul(BLOCK_ELEMENTS)
        .ok_or(IqQuantError::Geometry("overflow no resultado IQ-quant"))?;
    let mut output = vec![0.0; elements];
    // SAFETY: each branch checks the exact C block size and the output has
    // 256 f32 values per block, matching GGML's dequantizer contract.
    unsafe {
        match format {
            IqQuantFormat::Iq2S => dequantize_row_iq2_s(
                packed.as_ptr().cast(),
                output.as_mut_ptr(),
                i64::try_from(elements).map_err(|_| IqQuantError::Geometry("k excede i64"))?,
            ),
            IqQuantFormat::Iq3S => dequantize_row_iq3_s(
                packed.as_ptr().cast(),
                output.as_mut_ptr(),
                i64::try_from(elements).map_err(|_| IqQuantError::Geometry("k excede i64"))?,
            ),
            IqQuantFormat::Iq4XS => dequantize_row_iq4_xs(
                packed.as_ptr().cast(),
                output.as_mut_ptr(),
                i64::try_from(elements).map_err(|_| IqQuantError::Geometry("k excede i64"))?,
            ),
        }
    }
    Ok(output)
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
        31 if fraction == 0 => sign * f32::INFINITY,
        31 => f32::NAN,
        exponent => sign * (1.0 + fraction as f32 / 1024.0) * 2.0f32.powi(exponent - 15),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn cada_iq_priorizado_tem_bloco_e_dequantizacao_determinados() {
        for (format, bytes) in [
            (IqQuantFormat::Iq3S, 110usize),
            (IqQuantFormat::Iq2S, 82usize),
            (IqQuantFormat::Iq4XS, 136usize),
        ] {
            let values = dequantize_row(format, &vec![0; bytes]).unwrap();
            assert_eq!(values.len(), BLOCK_ELEMENTS);
            assert!(values.iter().all(|value| value.is_finite()));
        }
    }

    #[test]
    fn formatos_iq_rejeitam_payload_truncado() {
        for format in [
            IqQuantFormat::Iq3S,
            IqQuantFormat::Iq2S,
            IqQuantFormat::Iq4XS,
        ] {
            assert!(dequantize_row(format, &vec![0; format.block_bytes() - 1]).is_err());
        }
    }

    #[test]
    fn dtype_iq_fora_da_fase_recusa_com_id_e_nome() {
        let error = IqQuantFormat::from_dtype(20).unwrap_err().to_string();
        assert!(error.contains("20"));
        assert!(error.contains("IQ4_NL"));
    }

    #[test]
    fn source_carrega_iq3_s_iq2_s_e_iq4_xs_fundidos_e_separados() {
        for (dtype, format) in [
            (21u32, IqQuantFormat::Iq3S),
            (22u32, IqQuantFormat::Iq2S),
            (23u32, IqQuantFormat::Iq4XS),
        ] {
            for split in [false, true] {
                let path = std::env::temp_dir().join(format!(
                    "pom-ftk-iq-source-{dtype}-{split}-{}.gguf",
                    std::process::id()
                ));
                write_iq_source(&path, dtype, format, split);
                let source = IqQuantSource::open_infer(&path, 2).unwrap();
                assert_eq!(source.format(), format);
                let payload = source.load_expert(0, 1).unwrap();
                let expert = IqQuantExpert::from_payload(format, 256, 256, &payload).unwrap();
                assert_eq!(expert.run(&[1.0; 256]).unwrap().len(), 256);
                drop(source);
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    pub(crate) fn write_iq_source(
        path: &std::path::Path,
        dtype: u32,
        format: IqQuantFormat,
        split_gate_up: bool,
    ) {
        use std::fs::File;
        use std::io::Write;

        let hidden = 256usize;
        let intermediate = 256usize;
        let experts = 2usize;
        let gate_up = packed_rows(experts * 2 * intermediate, hidden, format);
        let gate = packed_rows(experts * intermediate, hidden, format);
        let up = packed_rows(experts * intermediate, hidden, format);
        let down = packed_rows(experts * hidden, intermediate, format);
        let mut router = Vec::with_capacity(experts * hidden * 4);
        for expert in 0..experts {
            for _ in 0..hidden {
                router.extend_from_slice(&((expert + 1) as f32).to_le_bytes());
            }
        }
        let gate_up_bytes = if split_gate_up {
            gate.len() + up.len()
        } else {
            gate_up.len()
        };
        let mut file = Vec::new();
        file.extend_from_slice(&0x4655_4747u32.to_le_bytes());
        file.extend_from_slice(&2u32.to_le_bytes());
        let tensor_count = if split_gate_up { 4u64 } else { 3u64 };
        file.extend_from_slice(&tensor_count.to_le_bytes());
        file.extend_from_slice(&1u64.to_le_bytes());
        gguf_string(&mut file, "general.alignment");
        file.extend_from_slice(&4u32.to_le_bytes());
        file.extend_from_slice(&32u32.to_le_bytes());
        if split_gate_up {
            gguf_tensor(
                &mut file,
                "blk.0.ffn_gate_exps.weight",
                &[hidden as u64, intermediate as u64, experts as u64],
                0,
                dtype,
            );
            gguf_tensor(
                &mut file,
                "blk.0.ffn_up_exps.weight",
                &[hidden as u64, intermediate as u64, experts as u64],
                gate.len() as u64,
                dtype,
            );
        } else {
            gguf_tensor(
                &mut file,
                "blk.0.ffn_gate_up_exps.weight",
                &[hidden as u64, (2 * intermediate) as u64, experts as u64],
                0,
                dtype,
            );
        }
        gguf_tensor(
            &mut file,
            "blk.0.ffn_down_exps.weight",
            &[intermediate as u64, hidden as u64, experts as u64],
            gate_up_bytes as u64,
            dtype,
        );
        gguf_tensor(
            &mut file,
            "blk.0.ffn_gate_inp.weight",
            &[hidden as u64, experts as u64],
            (gate_up_bytes + down.len()) as u64,
            0,
        );
        let data_start = (file.len() + 31) / 32 * 32;
        file.resize(data_start, 0);
        if split_gate_up {
            file.extend_from_slice(&gate);
            file.extend_from_slice(&up);
        } else {
            file.extend_from_slice(&gate_up);
        }
        file.extend_from_slice(&down);
        file.extend_from_slice(&router);
        File::create(path).unwrap().write_all(&file).unwrap();
    }

    fn packed_rows(rows: usize, columns: usize, format: IqQuantFormat) -> Vec<u8> {
        let row_size = row_bytes(format, columns).unwrap();
        let mut output = vec![0u8; rows * row_size];
        for row in output.chunks_exact_mut(row_size) {
            for block in row.chunks_exact_mut(format.block_bytes()) {
                block[..2].copy_from_slice(&0x3c00u16.to_le_bytes());
            }
        }
        output
    }

    fn gguf_string(output: &mut Vec<u8>, value: &str) {
        output.extend_from_slice(&(value.len() as u64).to_le_bytes());
        output.extend_from_slice(value.as_bytes());
    }

    fn gguf_tensor(output: &mut Vec<u8>, name: &str, dims: &[u64], offset: u64, dtype: u32) {
        gguf_string(output, name);
        output.extend_from_slice(&(dims.len() as u32).to_le_bytes());
        for dimension in dims {
            output.extend_from_slice(&dimension.to_le_bytes());
        }
        output.extend_from_slice(&dtype.to_le_bytes());
        output.extend_from_slice(&offset.to_le_bytes());
    }
}
