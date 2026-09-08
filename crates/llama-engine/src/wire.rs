//! Wire format for hidden state moving between peers.
//!
//! A chunk crosses as:
//!
//! ```text
//! [4 bytes big-endian: JSON header length][JSON header][raw tensor]
//! ```
//!
//! The header names `dtype` and `shape`, so **precision and encoding are a
//! parameter, not an assumption**: changing what travels on the wire touches
//! `encode`/`decode` here and nothing else in the pipeline.
//!
//! Two deliberate choices, both about bytes on the wire - which is what decides
//! time-to-first-word once the cluster runs over the internet rather than a LAN:
//!
//! * the body is **binary**, where it used to be base64 (which inflated every
//!   transfer by a third for nothing);
//! * the tensor travels as **f16** by default, half the size of f32. The maths
//!   stays f32 on both ends; f8 is also available as an explicit experiment.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
/// Largest inference frame body accepted from an authenticated peer. The
/// previous 16 MiB ceiling was smaller than a normal multi-session hidden
/// batch for wide models, so it rejected valid work before the peer could
/// produce a token.
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// The first and only inference wire version. The frame already carries one
/// or more independent items, so there is no separate single-item legacy
/// format to negotiate.
pub const BATCH_FRAME_VERSION: u32 = 1;

pub const MAX_BATCH_HEADER_BYTES: usize = 1024 * 1024;

/// Payload budget that leaves room for the largest permitted batch header and
/// its four-byte length prefix. The relay uses this when splitting coalesced
/// calls, so the encoded body also fits the Axum limit.
pub const MAX_BATCH_PAYLOAD_BYTES: usize = MAX_FRAME_PAYLOAD_BYTES
    .saturating_sub(MAX_BATCH_HEADER_BYTES)
    .saturating_sub(4);

/// Shape carried by an inference tensor. The first axis is the token count;
/// the remaining axes preserve the feature layout required by the receiving
/// model. A normal residual is `[tokens, n_embd]`; a model with additional
/// streams may carry `[tokens, n_embd, stream, ...]`.
pub type TensorShape = Vec<usize>;

/// Marker retained at call boundaries while all inference traffic uses the
/// canonical batch frame. Its variant selects the tensor precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameEncoding {
    F32,
    F16,
    F8,
}

/// Precision a tensor takes on the wire. Inference always receives f32 after
/// decoding, regardless of the selected wire representation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireDtype {
    F32,
    #[default]
    F16,
    F8,
}

impl WireDtype {
    pub fn as_str(&self) -> &'static str {
        match self {
            WireDtype::F32 => "f32",
            WireDtype::F16 => "f16",
            WireDtype::F8 => "f8",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "f32" => Ok(WireDtype::F32),
            "f16" => Ok(WireDtype::F16),
            "f8" => Ok(WireDtype::F8),
            other => Err(anyhow!("dtype de fio desconhecido: {other}")),
        }
    }

    pub fn bytes_per_value(&self) -> usize {
        match self {
            WireDtype::F32 => 4,
            WireDtype::F16 => 2,
            WireDtype::F8 => 1,
        }
    }

    pub fn frame_encoding(self) -> FrameEncoding {
        match self {
            WireDtype::F32 => FrameEncoding::F32,
            WireDtype::F16 => FrameEncoding::F16,
            WireDtype::F8 => FrameEncoding::F8,
        }
    }
}

/// One tensor region inside a [`BatchFrame`].
///
/// `offset` and `len` refer to the concatenated payload in the frame. The
/// decoder validates that the region is in bounds, matches `shape` and
/// `dtype`, and does not overlap another item before returning it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchItem {
    pub item_id: String,
    pub session_id: String,
    pub shape: TensorShape,
    pub dtype: String,
    pub offset: usize,
    pub len: usize,
    #[serde(default)]
    pub meta: Value,
}

/// Versioned frame carrying independent tensors from one microbatch.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchFrame {
    pub version: u32,
    pub kind: String,
    pub batch_id: String,
    pub items: Vec<BatchItem>,
    pub payload: Vec<u8>,
}

/// An owned item used to build a request or response batch.
///
/// The tensor is kept as f32 until the final encoder pass. This makes the
/// batch coordinator independent from the selected wire precision and avoids
/// serialising an item twice while it waits in the coalescing window.
#[derive(Debug, Clone)]
pub struct BatchPayloadItem {
    pub item_id: String,
    pub session_id: String,
    pub meta: Value,
    pub tensor: Option<(Vec<f32>, TensorShape)>,
    pub dtype: WireDtype,
}

/// One decoded item, with its payload already converted to the f32 format used
/// by the inference engine.
#[derive(Debug, Clone)]
pub struct DecodedBatchItem {
    pub item_id: String,
    pub meta: Value,
    pub tensor: Option<(Vec<f32>, TensorShape)>,
    pub dtype: WireDtype,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchHeader {
    #[serde(rename = "frame_version")]
    version: u32,
    kind: String,
    batch_id: String,
    items: Vec<BatchItem>,
}

/// Encode a validated multi-item frame as
/// `[4-byte header length][JSON header][concatenated payload]`.
pub fn encode_batch(batch: &BatchFrame) -> Result<Vec<u8>> {
    validate_batch(batch)?;
    let header = serde_json::to_vec(&BatchHeader {
        version: batch.version,
        kind: batch.kind.clone(),
        batch_id: batch.batch_id.clone(),
        items: batch.items.clone(),
    })?;
    if header.len() > MAX_BATCH_HEADER_BYTES {
        return Err(anyhow!(
            "cabeçalho do batch excede o limite de {} bytes",
            MAX_BATCH_HEADER_BYTES
        ));
    }
    frame_bytes(header, &batch.payload)
}

/// Build a batch from independently owned inference items. Each item's
/// metadata is copied into the header, while all tensors share one contiguous
/// payload. The caller is responsible for grouping only compatible routes.
/// O tensor é escrito **uma vez**, direto no buffer que vai para o fio: as
/// posições de cada item saem de [`tensor_payload_len`], que as calcula sem
/// alocar, e o cabeçalho JSON fica pronto antes de qualquer conversão. Antes
/// os mesmos bytes eram tocados quatro vezes (converter, concatenar, clonar a
/// `BatchFrame`, copiar para a saída) - num salto de 5120 dimensões em f16 é
/// 8 KiB copiados três vezes por travessia, por sessão.
pub fn encode_batch_items(
    kind: impl Into<String>,
    batch_id: impl Into<String>,
    items: &[BatchPayloadItem],
) -> Result<Vec<u8>> {
    let kind = kind.into();
    let batch_id = batch_id.into();
    let mut headers = Vec::with_capacity(items.len());
    let mut payload_len = 0usize;
    for item in items {
        if item.item_id.trim().is_empty() {
            return Err(anyhow!("batch item sem item_id"));
        }
        if item.session_id.trim().is_empty() {
            return Err(anyhow!("batch item {} sem session_id", item.item_id));
        }
        let (shape, len) = match item.tensor.as_ref() {
            Some((values, shape)) => (
                shape.clone(),
                tensor_payload_len(Some((values.as_slice(), shape.clone())), item.dtype)?,
            ),
            None => (vec![0, 0], 0),
        };
        let mut meta = item.meta.clone();
        if !meta.is_object() {
            return Err(anyhow!(
                "meta do item {} não é um objeto JSON",
                item.item_id
            ));
        }
        meta["frame_version"] = Value::from(BATCH_FRAME_VERSION);
        meta["batch_id"] = Value::from(batch_id.clone());
        meta["item_id"] = Value::from(item.item_id.clone());
        headers.push(BatchItem {
            item_id: item.item_id.clone(),
            session_id: item.session_id.clone(),
            shape,
            dtype: item.dtype.as_str().into(),
            offset: payload_len,
            len,
            meta,
        });
        payload_len = payload_len
            .checked_add(len)
            .ok_or_else(|| anyhow!("payload do batch excede o limite numérico"))?;
    }
    validate_items(BATCH_FRAME_VERSION, &kind, &batch_id, &headers, payload_len)?;
    let header = serde_json::to_vec(&BatchHeader {
        version: BATCH_FRAME_VERSION,
        kind,
        batch_id,
        items: headers,
    })?;
    if header.len() > MAX_BATCH_HEADER_BYTES {
        return Err(anyhow!(
            "cabeçalho do batch excede o limite de {} bytes",
            MAX_BATCH_HEADER_BYTES
        ));
    }
    let mut out = frame_prefix(&header, payload_len)?;
    for item in items {
        if let Some((values, shape)) = item.tensor.as_ref() {
            encode_values_into(values, shape, item.dtype, &mut out)?;
        }
    }
    Ok(out)
}

/// Identificação de um quadro lida **sem** copiar o payload: é o suficiente
/// para rotear o quadro (o `kind` é a rota) e para decidir se ele carrega um
/// ou vários itens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchHeaderInfo {
    pub version: u32,
    pub kind: String,
    pub batch_id: String,
    pub item_count: usize,
}

/// Separa `[4B tamanho][cabeçalho JSON]` do payload, **emprestando** o payload
/// do buffer recebido. É o caminho de decodificação quente: copiar o payload
/// aqui custa uma cópia do estado oculto inteiro em cada travessia, de cada
/// lado.
fn split_frame(buf: &[u8]) -> Result<(BatchHeader, &[u8])> {
    if buf.len() < 4 {
        return Err(anyhow!("quadro batch curto demais: {} bytes", buf.len()));
    }
    let head_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if head_len > MAX_BATCH_HEADER_BYTES {
        return Err(anyhow!(
            "cabeçalho do batch excede o limite de {} bytes",
            MAX_BATCH_HEADER_BYTES
        ));
    }
    let head_end = 4usize
        .checked_add(head_len)
        .ok_or_else(|| anyhow!("tamanho do cabeçalho do batch inválido"))?;
    if buf.len() < head_end {
        return Err(anyhow!("cabeçalho do batch truncado"));
    }
    let header: BatchHeader = serde_json::from_slice(&buf[4..head_end])?;
    Ok((header, &buf[head_end..]))
}

/// Lê a identidade do quadro sem tocar no tensor. Quem só precisa saber a
/// rota, o `batch_id` ou quantos itens vêm no quadro usa isto.
pub fn peek_batch_header(buf: &[u8]) -> Result<BatchHeaderInfo> {
    let (header, payload) = split_frame(buf)?;
    validate_items(
        header.version,
        &header.kind,
        &header.batch_id,
        &header.items,
        payload.len(),
    )?;
    Ok(BatchHeaderInfo {
        version: header.version,
        kind: header.kind,
        batch_id: header.batch_id,
        item_count: header.items.len(),
    })
}

/// Confere que os bytes formam um quadro batch válido, sem copiar nem
/// converter nada. É o que um transporte quer antes de repassar o corpo
/// adiante.
pub fn validate_batch_bytes(buf: &[u8]) -> Result<()> {
    peek_batch_header(buf).map(|_| ())
}

/// Decode and fully validate a multi-item frame before exposing any tensor
/// region to inference code.
pub fn decode_batch(buf: &[u8]) -> Result<BatchFrame> {
    let (header, payload) = split_frame(buf)?;
    let batch = BatchFrame {
        version: header.version,
        kind: header.kind,
        batch_id: header.batch_id,
        items: header.items,
        payload: payload.to_vec(),
    };
    validate_batch(&batch)?;
    Ok(batch)
}

/// Decode every item in a validated frame. Validation happens before any
/// tensor conversion, so a malformed item cannot partially touch inference
/// state. O payload é lido **emprestado** do buffer recebido: a única cópia do
/// tensor é a expansão para f32 que a inferência consome.
pub fn decode_batch_items(buf: &[u8]) -> Result<(BatchHeaderInfo, Vec<DecodedBatchItem>)> {
    let (header, payload) = split_frame(buf)?;
    validate_items(
        header.version,
        &header.kind,
        &header.batch_id,
        &header.items,
        payload.len(),
    )?;
    let mut decoded = Vec::with_capacity(header.items.len());
    for item in &header.items {
        let (meta, tensor) = decode_item_from(payload, &header.batch_id, item)?;
        decoded.push(DecodedBatchItem {
            item_id: item.item_id.clone(),
            meta,
            tensor,
            dtype: WireDtype::parse(&item.dtype)?,
        });
    }
    Ok((
        BatchHeaderInfo {
            version: header.version,
            kind: header.kind,
            batch_id: header.batch_id,
            item_count: header.items.len(),
        },
        decoded,
    ))
}

/// Re-encode one item from a batch as a one-item batch. This is used by the
/// receiver fan-out adapter: the local scheduler still receives independent
/// commands, while the commands are submitted concurrently and therefore can
/// form one local llama batch.
pub fn encode_batch_item(batch: &BatchFrame, item: &BatchItem) -> Result<Vec<u8>> {
    let end = item
        .offset
        .checked_add(item.len)
        .ok_or_else(|| anyhow!("item excede o limite"))?;
    if end > batch.payload.len() {
        return Err(anyhow!("item aponta para fora do payload"));
    }
    encode_batch(&BatchFrame {
        version: BATCH_FRAME_VERSION,
        kind: batch.kind.clone(),
        batch_id: batch.batch_id.clone(),
        items: vec![BatchItem {
            offset: 0,
            ..item.clone()
        }],
        payload: batch.payload[item.offset..end].to_vec(),
    })
}

/// Encode one request as a one-item batch. One-item requests and coalesced
/// requests therefore share exactly the same protocol and validation path.
pub fn encode_batch_single(
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
) -> Result<Vec<u8>> {
    let (shape, payload) = match tensor {
        Some((values, shape)) => {
            let expected_values = checked_tensor_values(&shape)?;
            if values.len() != expected_values {
                return Err(anyhow!(
                    "tensor com {} valores não bate com shape {:?}",
                    values.len(),
                    shape
                ));
            }
            let payload = encode_values(values, &shape, dtype)?;
            (shape, payload)
        }
        None => (vec![0, 0], Vec::new()),
    };
    let item_id = meta
        .get("item_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or("item-0");
    let batch_id = meta
        .get("batch_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(item_id);
    let session_id = meta
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or("__session__");
    encode_batch(&BatchFrame {
        version: BATCH_FRAME_VERSION,
        kind: "inference".into(),
        batch_id: batch_id.into(),
        items: vec![BatchItem {
            item_id: item_id.into(),
            session_id: session_id.into(),
            shape,
            dtype: dtype.as_str().into(),
            offset: 0,
            len: payload.len(),
            meta: meta.clone(),
        }],
        payload,
    })
}

/// Decode a one-item frame for call sites that need the convenient historical
/// `(meta, tensor)` shape. Multi-item receivers use `decode_batch_items`.
pub fn decode_batch_single(buf: &[u8]) -> Result<(Value, Option<(Vec<f32>, TensorShape)>)> {
    let (header, payload) = split_frame(buf)?;
    validate_items(
        header.version,
        &header.kind,
        &header.batch_id,
        &header.items,
        payload.len(),
    )?;
    if header.items.len() != 1 {
        return Err(anyhow!(
            "o endpoint de inferência ainda aceita um item por quadro batch"
        ));
    }
    decode_item_from(payload, &header.batch_id, &header.items[0])
}

/// Decode the canonical inference request. All inference traffic is a batch
/// frame, including a request carrying only one item.
pub fn decode_inference_request(buf: &[u8]) -> Result<(Value, Option<(Vec<f32>, TensorShape)>)> {
    decode_batch_single(buf)
}

fn validate_batch(batch: &BatchFrame) -> Result<()> {
    validate_items(
        batch.version,
        &batch.kind,
        &batch.batch_id,
        &batch.items,
        batch.payload.len(),
    )
}

/// A validação inteira do quadro expressa sobre o **tamanho** do payload, não
/// sobre o payload. É o que permite validar sem copiar - e é a mesma regra
/// para quem codifica (que ainda nem escreveu os bytes) e para quem decodifica.
fn validate_items(
    version: u32,
    kind: &str,
    batch_id: &str,
    items: &[BatchItem],
    payload_len: usize,
) -> Result<()> {
    if version != BATCH_FRAME_VERSION {
        return Err(anyhow!(
            "versão de batch inválida: {}, esperada {}",
            version,
            BATCH_FRAME_VERSION
        ));
    }
    if kind.trim().is_empty() {
        return Err(anyhow!("batch sem kind"));
    }
    if batch_id.trim().is_empty() {
        return Err(anyhow!("batch sem batch_id"));
    }
    if items.is_empty() {
        return Err(anyhow!("batch sem itens"));
    }
    if payload_len > MAX_FRAME_PAYLOAD_BYTES {
        return Err(anyhow!(
            "payload do batch excede limite de {} bytes",
            MAX_FRAME_PAYLOAD_BYTES
        ));
    }

    let mut item_ids = HashSet::with_capacity(items.len());
    let mut ranges = Vec::with_capacity(items.len());
    for item in items {
        if item.item_id.trim().is_empty() {
            return Err(anyhow!("item sem item_id"));
        }
        if item.session_id.trim().is_empty() {
            return Err(anyhow!("item {} sem session_id", item.item_id));
        }
        if !item_ids.insert(&item.item_id) {
            return Err(anyhow!("item_id duplicado: {}", item.item_id));
        }
        let dtype = WireDtype::parse(&item.dtype)?;
        let expected_len = checked_tensor_bytes(&item.shape, dtype)?;
        if item.len != expected_len {
            return Err(anyhow!(
                "item {} declara {} bytes, esperado {} para shape {:?} em {}",
                item.item_id,
                item.len,
                expected_len,
                item.shape,
                item.dtype
            ));
        }
        let end = item
            .offset
            .checked_add(item.len)
            .ok_or_else(|| anyhow!("offset do item {} excede o limite", item.item_id))?;
        if end > payload_len {
            return Err(anyhow!("item {} aponta para fora do payload", item.item_id));
        }
        ranges.push((item.offset, end, item.item_id.as_str()));
    }

    ranges.sort_unstable_by_key(|(offset, _, _)| *offset);
    for pair in ranges.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(anyhow!("itens {} e {} se sobrepõem", pair[0].2, pair[1].2));
        }
    }
    Ok(())
}

fn decode_item_from(
    payload: &[u8],
    batch_id: &str,
    item: &BatchItem,
) -> Result<(Value, Option<(Vec<f32>, TensorShape)>)> {
    let end = item
        .offset
        .checked_add(item.len)
        .ok_or_else(|| anyhow!("item excede o limite"))?;
    if end > payload.len() {
        return Err(anyhow!("item {} aponta para fora do payload", item.item_id));
    }
    let mut meta = item.meta.clone();
    if !meta.is_object() {
        return Err(anyhow!("meta do item batch não é um objeto JSON"));
    }
    meta["dtype"] = Value::from(item.dtype.clone());
    meta["shape"] = Value::from(item.shape.clone());
    meta["frame_version"] = Value::from(BATCH_FRAME_VERSION);
    meta["batch_id"] = Value::from(batch_id.to_string());
    meta["item_id"] = Value::from(item.item_id.clone());
    decode_tensor_payload(&meta, &payload[item.offset..end])
}

/// Canonical one-item encoder used by control responses and by the few
/// internal paths that do not need to coalesce with another request.
pub fn encode(
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
) -> Result<Vec<u8>> {
    encode_batch_single(meta, tensor, dtype)
}

/// The first public protocol has one tensor representation per item. Keep this
/// small marker parameter at existing call boundaries while routing all bytes
/// through `frame/v1-batch`.
// Retido apenas pelo teste de equivalência entre o encoder de item único e o
// frame de batch; o runtime usa `encode` com `WireDtype`.
#[cfg(test)]
pub fn encode_with_encoding(
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    encoding: FrameEncoding,
) -> Result<Vec<u8>> {
    let dtype = match encoding {
        FrameEncoding::F32 => WireDtype::F32,
        FrameEncoding::F16 => WireDtype::F16,
        FrameEncoding::F8 => WireDtype::F8,
    };
    encode_batch_single(meta, tensor, dtype)
}

fn frame_bytes(head: Vec<u8>, payload: &[u8]) -> Result<Vec<u8>> {
    let mut out = frame_prefix(&head, payload.len())?;
    out.extend_from_slice(payload);
    Ok(out)
}

/// `[4-byte header length][JSON header]` já dimensionado para receber
/// `payload_len` bytes de tensor logo em seguida. Existe para que o encoder de
/// lote escreva o tensor **uma vez**, direto no buffer que vai para o fio, em
/// vez de montar o payload à parte e copiá-lo no fim.
fn frame_prefix(head: &[u8], payload_len: usize) -> Result<Vec<u8>> {
    if head.len() > u32::MAX as usize {
        return Err(anyhow!("cabeçalho excede o limite do protocolo"));
    }
    let frame_len = 4usize
        .checked_add(head.len())
        .and_then(|len| len.checked_add(payload_len))
        .ok_or_else(|| anyhow!("corpo do frame excede o limite numérico"))?;
    if frame_len > MAX_FRAME_PAYLOAD_BYTES {
        return Err(anyhow!(
            "corpo do frame excede limite de {} bytes",
            MAX_FRAME_PAYLOAD_BYTES
        ));
    }
    let mut out = Vec::with_capacity(frame_len);
    out.extend_from_slice(&(head.len() as u32).to_be_bytes());
    out.extend_from_slice(head);
    Ok(out)
}

/// Inverse of [`encode`]. The tensor always comes back as f32 - reduced
/// precision is a property of the wire, never of the maths.
pub fn decode(buf: &[u8]) -> Result<(Value, Option<(Vec<f32>, TensorShape)>)> {
    decode_batch_single(buf)
}

fn decode_tensor_payload(
    meta: &Value,
    payload: &[u8],
) -> Result<(Value, Option<(Vec<f32>, TensorShape)>)> {
    if payload.is_empty() {
        return Ok((meta.clone(), None));
    }

    let dtype = WireDtype::parse(meta.get("dtype").and_then(|v| v.as_str()).unwrap_or("f32"))?;
    let shape: Vec<usize> = meta
        .get("shape")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|x| {
                    x.as_u64()
                        .map(|n| n as usize)
                        .ok_or_else(|| anyhow!("dimensão de shape não é um inteiro: {x}"))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    if shape.len() < 2 {
        return Err(anyhow!("shape inválido: {:?}", shape));
    }
    let expected = checked_tensor_values(&shape)?;
    let expected_bytes = expected
        .checked_mul(dtype.bytes_per_value())
        .ok_or_else(|| anyhow!("shape excede o limite"))?;
    if expected_bytes > MAX_FRAME_PAYLOAD_BYTES {
        return Err(anyhow!(
            "payload do tensor excede limite de {} bytes",
            MAX_FRAME_PAYLOAD_BYTES
        ));
    }

    if payload.len() != expected_bytes {
        return Err(anyhow!(
            "payload tem {} bytes, esperado {expected_bytes}",
            payload.len()
        ));
    }

    let values: Vec<f32> = match dtype {
        WireDtype::F32 => payload
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        WireDtype::F16 => payload
            .chunks_exact(2)
            .map(|c| f16_bits_to_f32(u16::from_le_bytes([c[0], c[1]])))
            .collect(),
        WireDtype::F8 => payload
            .iter()
            .map(|byte| f8_bits_to_f32(*byte))
            .collect::<Result<Vec<_>>>()?,
    };
    if values.len() != expected {
        return Err(anyhow!(
            "tensor com {} valores, esperado {expected}",
            values.len()
        ));
    }
    Ok((meta.clone(), Some((values, shape))))
}

fn checked_tensor_values(shape: &[usize]) -> Result<usize> {
    if shape.len() < 2 {
        return Err(anyhow!("shape inválido: {:?}", shape));
    }
    shape.iter().try_fold(1usize, |values, &dimension| {
        values
            .checked_mul(dimension)
            .ok_or_else(|| anyhow!("shape excede o limite"))
    })
}

fn checked_tensor_bytes(shape: &[usize], dtype: WireDtype) -> Result<usize> {
    let values = checked_tensor_values(shape)?;
    let bytes = values
        .checked_mul(dtype.bytes_per_value())
        .ok_or_else(|| anyhow!("shape excede o limite"))?;
    if bytes > MAX_FRAME_PAYLOAD_BYTES {
        return Err(anyhow!(
            "payload do tensor excede limite de {} bytes",
            MAX_FRAME_PAYLOAD_BYTES
        ));
    }
    Ok(bytes)
}

/// Number of bytes an owned tensor occupies after wire conversion, without
/// allocating the converted payload. The relay uses this to split a
/// multi-item frame before encoding it.
pub fn tensor_payload_len(
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
) -> Result<usize> {
    let Some((values, shape)) = tensor else {
        return Ok(0);
    };
    let expected_values = checked_tensor_values(&shape)?;
    if values.len() != expected_values {
        return Err(anyhow!(
            "tensor com {} valores não bate com shape {:?}",
            values.len(),
            shape
        ));
    }
    checked_tensor_bytes(&shape, dtype)
}

fn encode_values(values: &[f32], shape: &[usize], dtype: WireDtype) -> Result<Vec<u8>> {
    let mut payload = Vec::with_capacity(checked_tensor_bytes(shape, dtype)?);
    encode_values_into(values, shape, dtype, &mut payload)?;
    Ok(payload)
}

/// Converte o tensor **direto** para o buffer de saída. Separado de
/// [`encode_values`] porque o encoder de lote não quer o `Vec` intermediário:
/// ele já sabe a posição final de cada item.
fn encode_values_into(
    values: &[f32],
    shape: &[usize],
    dtype: WireDtype,
    out: &mut Vec<u8>,
) -> Result<()> {
    let expected_values = checked_tensor_values(shape)?;
    if values.len() != expected_values {
        return Err(anyhow!(
            "tensor com {} valores não bate com shape {:?}",
            values.len(),
            shape
        ));
    }
    let bytes = checked_tensor_bytes(shape, dtype)?;
    out.reserve(bytes);
    // A conversão passa por um bloco na pilha em vez de crescer o vetor valor
    // a valor: uma chamada de `extend_from_slice` por valor custa mais que a
    // conversão em si, e foi medida em quase o dobro do enquadramento inteiro.
    const BLOCO: usize = 1024;
    match dtype {
        WireDtype::F32 => {
            let mut staging = [0u8; BLOCO * 4];
            for block in values.chunks(BLOCO) {
                for (slot, value) in staging.chunks_exact_mut(4).zip(block) {
                    slot.copy_from_slice(&value.to_le_bytes());
                }
                out.extend_from_slice(&staging[..block.len() * 4]);
            }
        }
        WireDtype::F16 => {
            let mut staging = [0u8; BLOCO * 2];
            for block in values.chunks(BLOCO) {
                for (slot, value) in staging.chunks_exact_mut(2).zip(block) {
                    slot.copy_from_slice(&f32_to_f16_bits(*value).to_le_bytes());
                }
                out.extend_from_slice(&staging[..block.len() * 2]);
            }
        }
        WireDtype::F8 => {
            let mut staging = [0u8; BLOCO];
            for block in values.chunks(BLOCO) {
                for (slot, value) in staging.iter_mut().zip(block) {
                    *slot = f32_to_f8_bits(*value)?;
                }
                out.extend_from_slice(&staging[..block.len()]);
            }
        }
    }
    Ok(())
}

// ── f16 conversion ───────────────────────────────────────────────────────────
// Written out rather than pulled in as a dependency: it is twenty lines, and
// the rounding behaviour is something this pipeline should own explicitly.

fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let mut exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x007f_ffff;

    if exp >= 0x1f {
        // Overflow, infinity or NaN.
        return sign
            | 0x7c00
            | if mantissa != 0 && exp == 0x1f + 112 {
                0x0200
            } else {
                0
            };
    }
    if exp <= 0 {
        // Subnormal, or too small to represent: flush toward zero.
        if exp < -10 {
            return sign;
        }
        let mantissa = mantissa | 0x0080_0000;
        let shift = (14 - exp) as u32;
        let half = (mantissa >> shift) as u16;
        // Round to nearest, ties away from zero.
        let round_bit = 1u32 << (shift - 1);
        let rounded = if (mantissa & round_bit) != 0 {
            half + 1
        } else {
            half
        };
        return sign | rounded;
    }

    let half = ((exp as u16) << 10) | (mantissa >> 13) as u16;
    // Round to nearest on the dropped mantissa bits.
    if (mantissa & 0x1000) != 0 {
        let bumped = half.wrapping_add(1);
        // Carrying into the exponent is fine; saturating into inf is correct.
        exp = ((bumped >> 10) & 0x1f) as i32;
        let _ = exp;
        return sign | bumped;
    }
    sign | half
}

fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = ((bits & 0x8000) as u32) << 16;
    let exp = ((bits >> 10) & 0x1f) as u32;
    let mantissa = (bits & 0x03ff) as u32;

    if exp == 0 {
        if mantissa == 0 {
            return f32::from_bits(sign);
        }
        // Subnormal: normalise it.
        let mut e = -1i32;
        let mut m = mantissa;
        while (m & 0x0400) == 0 {
            m <<= 1;
            e -= 1;
        }
        let m = m & 0x03ff;
        let exp32 = ((127 - 15 + 1 + e) as u32) << 23;
        return f32::from_bits(sign | exp32 | (m << 13));
    }
    if exp == 0x1f {
        return f32::from_bits(sign | 0x7f80_0000 | (mantissa << 13));
    }
    let exp32 = (exp + 127 - 15) << 23;
    f32::from_bits(sign | exp32 | (mantissa << 13))
}

// ── f8 E4M3FN conversion ────────────────────────────────────────────────────
// E4M3FN has no infinity encoding. Exponent 0xF and mantissa 0..6 are finite,
// while 0xF/7 is reserved. Values outside the finite range are refused so a
// selected lower precision never saturates an activation without an error.

fn f32_to_f8_bits(value: f32) -> Result<u8> {
    if !value.is_finite() {
        return Err(anyhow!("valor não finito não é representável em f8"));
    }
    let sign = if value.is_sign_negative() { 0x80 } else { 0 };
    let magnitude = value.abs();
    if magnitude == 0.0 {
        return Ok(sign);
    }
    if magnitude > 448.0 {
        return Err(anyhow!(
            "valor {value} excede o alcance finito de f8 E4M3FN"
        ));
    }

    const MIN_NORMAL: f32 = 0.015625;
    const SUBNORMAL_STEP: f32 = 0.001953125;
    if magnitude < MIN_NORMAL {
        let mantissa = (magnitude / SUBNORMAL_STEP).round() as u8;
        return Ok(if mantissa >= 8 {
            sign | 0x08
        } else {
            sign | mantissa
        });
    }

    let exponent = magnitude.log2().floor() as i32;
    let base = 2.0_f32.powi(exponent);
    let mut encoded_exponent = exponent + 7;
    let mut mantissa = ((magnitude / base - 1.0) * 8.0).round() as i32;
    if mantissa >= 8 {
        encoded_exponent += 1;
        mantissa = 0;
    }
    if encoded_exponent > 15 {
        return Err(anyhow!(
            "valor {value} excede o alcance finito de f8 E4M3FN"
        ));
    }
    if encoded_exponent == 15 {
        mantissa = mantissa.min(6);
    }
    Ok(sign | ((encoded_exponent as u8) << 3) | mantissa as u8)
}

fn f8_bits_to_f32(bits: u8) -> Result<f32> {
    let sign = if bits & 0x80 != 0 { -1.0 } else { 1.0 };
    let exponent = (bits >> 3) & 0x0f;
    let mantissa = bits & 0x07;
    let magnitude = match exponent {
        0 => f32::from(mantissa) * 0.001953125,
        1..=14 => (1.0 + f32::from(mantissa) / 8.0) * 2.0_f32.powi(i32::from(exponent) - 7),
        15 if mantissa <= 6 => (1.0 + f32::from(mantissa) / 8.0) * 256.0,
        15 => return Err(anyhow!("codificação reservada de f8 E4M3FN: {bits:#04x}")),
        _ => unreachable!(),
    };
    Ok(sign * magnitude)
}

#[cfg(test)]
mod tests {
    /// Custo do enquadramento por travessia, isolado do transporte: uma
    /// codificação e uma decodificação de um item de `[1, 5120]` em f16 - a
    /// forma exata do estado oculto de um salto interno do cluster do captain.
    ///
    /// Não afirma nada sobre tempo absoluto (a máquina de CI não é a do
    /// captain); serve para registrar o número no commit e para tornar visível
    /// uma regressão de ordem de grandeza. O teto é folgado de propósito.
    #[test]
    fn custo_do_enquadramento_por_travessia() {
        use super::*;
        const DIMS: usize = 5120;
        const RODADAS: usize = 2000;
        let tensor = vec![0.25f32; DIMS];
        let items = [BatchPayloadItem {
            item_id: "item-0".into(),
            session_id: "s".into(),
            meta: serde_json::json!({"session_id": "s", "is_last": true}),
            tensor: Some((tensor.clone(), vec![1, DIMS])),
            dtype: WireDtype::F16,
        }];

        let comecou = std::time::Instant::now();
        let mut bytes = 0usize;
        for _ in 0..RODADAS {
            let encoded = encode_batch_items("/v1/step", "batch", &items).expect("codifica");
            bytes += encoded.len();
            let decoded = decode_batch_items(&encoded).expect("decodifica");
            assert_eq!(decoded.1.len(), 1);
        }
        let por_travessia = comecou.elapsed().as_secs_f64() * 1e6 / RODADAS as f64;
        println!(
            "enquadramento: {por_travessia:.1} us/travessia, {} bytes/quadro",
            bytes / RODADAS
        );
        assert!(
            por_travessia < 2000.0,
            "enquadramento de uma travessia levou {por_travessia:.1} us - \
             ordem de grandeza acima do esperado"
        );
    }

    use super::*;

    #[test]
    fn one_item_and_multi_item_requests_use_the_same_first_protocol() {
        let values = vec![0.25_f32; 8];
        let single = encode_with_encoding(
            &serde_json::json!({"session_id": "single"}),
            Some((&values, vec![1, 8])),
            FrameEncoding::F16,
        )
        .unwrap();
        let batch = decode_batch(&single).unwrap();
        assert_eq!(batch.version, BATCH_FRAME_VERSION);
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.items[0].session_id, "single");
    }

    #[test]
    fn a_tensor_survives_a_round_trip_in_f32() {
        let values = vec![0.0f32, 1.5, -2.25, 1e-8, 1e8];
        let meta = serde_json::json!({ "session_id": "abc" });
        let framed = encode(&meta, Some((&values, vec![1, 5])), WireDtype::F32).unwrap();
        let (back_meta, tensor) = decode(&framed).unwrap();
        assert_eq!(back_meta["session_id"], "abc");
        let (back, shape) = tensor.unwrap();
        assert_eq!(shape, vec![1, 5]);
        assert_eq!(back, values);
    }

    /// f16 is lossy by design; what matters is that it is *close*, and that the
    /// framing survives. Activations live around unit magnitude, which is where
    /// f16 has ~3 decimal digits.
    #[test]
    fn f16_round_trip_stays_close_for_activation_sized_values() {
        let values: Vec<f32> = vec![0.0, 1.0, -1.0, 0.5, -0.125, 12.5, -37.75, 0.001];
        let meta = serde_json::json!({});
        let framed = encode(&meta, Some((&values, vec![1, 8])), WireDtype::F16).unwrap();
        let (_, tensor) = decode(&framed).unwrap();
        let (back, _) = tensor.unwrap();
        for (a, b) in values.iter().zip(back.iter()) {
            let tolerance = a.abs() * 0.001 + 1e-6;
            assert!((a - b).abs() <= tolerance, "{a} virou {b}");
        }
    }

    #[test]
    fn f16_halves_the_bytes_on_the_wire() {
        let values = vec![1.0f32; 1024];
        let meta = serde_json::json!({});
        let as_f32 = encode(&meta, Some((&values, vec![1, 1024])), WireDtype::F32).unwrap();
        let as_f16 = encode(&meta, Some((&values, vec![1, 1024])), WireDtype::F16).unwrap();
        // Headers differ by a couple of bytes; the payload is what matters.
        assert_eq!(as_f32.len() - as_f16.len(), 1024 * 2);
    }

    #[test]
    fn f8_round_trip_stays_within_activation_tolerance() {
        let values: Vec<f32> = vec![0.0, 1.0, -1.0, 0.5, -0.125, 12.5, -37.75, 0.001];
        let framed = encode(
            &serde_json::json!({}),
            Some((&values, vec![1, 8])),
            WireDtype::F8,
        )
        .unwrap();
        let (meta, tensor) = decode(&framed).unwrap();
        assert_eq!(meta["dtype"], "f8");
        let (back, shape) = tensor.unwrap();
        assert_eq!(shape, vec![1, 8]);
        assert!(back.iter().all(|value| value.is_finite()));
        for (original, decoded) in values.iter().zip(back.iter()) {
            let tolerance = original.abs() * 0.2 + 0.002;
            assert!(
                (original - decoded).abs() <= tolerance,
                "{original} virou {decoded}"
            );
        }
    }

    #[test]
    fn f8_uses_one_byte_per_value() {
        let values = vec![1.0_f32; 1024];
        assert_eq!(
            tensor_payload_len(Some((&values, vec![1, 1024])), WireDtype::F8).unwrap(),
            1024
        );
        assert_eq!(
            tensor_payload_len(Some((&values, vec![1, 1024])), WireDtype::F16).unwrap(),
            1024 * 2
        );
    }

    #[test]
    fn extra_feature_axes_survive_a_round_trip() {
        let values: Vec<f32> = (0..24).map(|value| value as f32).collect();
        let shape = vec![2, 4, 3];
        let framed = encode(
            &serde_json::json!({}),
            Some((&values, shape.clone())),
            WireDtype::F16,
        )
        .unwrap();
        let (_, tensor) = decode(&framed).unwrap();
        let (decoded, decoded_shape) = tensor.unwrap();
        assert_eq!(decoded_shape, shape);
        assert_eq!(decoded.len(), values.len());
        assert!(decoded
            .iter()
            .zip(values)
            .all(|(actual, expected)| (actual - expected).abs() < 0.01));
    }

    #[test]
    fn residual_payload_cost_is_measured_per_token_and_feature_width() {
        let shape = vec![1, 4096, 4];
        let values = vec![1.0_f32; shape.iter().product()];
        assert_eq!(
            tensor_payload_len(Some((&values, shape.clone())), WireDtype::F16).unwrap(),
            4 * 4096 * 2
        );
        assert_eq!(
            tensor_payload_len(Some((&values, shape.clone())), WireDtype::F32).unwrap(),
            4 * 4096 * 4
        );
        assert_eq!(
            tensor_payload_len(Some((&values, shape)), WireDtype::F8).unwrap(),
            4 * 4096
        );
    }

    #[test]
    fn f8_rejects_non_finite_or_out_of_range_values() {
        for value in [f32::NAN, f32::INFINITY, -f32::INFINITY, 449.0] {
            let error = encode(
                &serde_json::json!({}),
                Some((&[value], vec![1, 1])),
                WireDtype::F8,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("f8"), "erro inesperado: {error}");
        }
    }

    #[test]
    fn f8_decode_rejects_reserved_encoding() {
        let frame = BatchFrame {
            version: BATCH_FRAME_VERSION,
            kind: "inference".into(),
            batch_id: "batch".into(),
            items: vec![BatchItem {
                item_id: "item".into(),
                session_id: "session".into(),
                shape: vec![1, 1],
                dtype: "f8".into(),
                offset: 0,
                len: 1,
                meta: serde_json::json!({}),
            }],
            payload: vec![0x7f],
        };
        let error = decode(&encode_batch(&frame).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("f8"), "erro inesperado: {error}");
    }

    #[test]
    fn a_header_with_no_tensor_round_trips() {
        let meta = serde_json::json!({ "done": true, "token_id": 42 });
        let framed = encode(&meta, None, WireDtype::F16).unwrap();
        let (back, tensor) = decode(&framed).unwrap();
        assert!(tensor.is_none());
        assert_eq!(back["token_id"], 42);
    }

    #[test]
    fn a_truncated_frame_is_refused_instead_of_panicking() {
        assert!(decode(&[0, 0]).is_err());
        assert!(decode(&[0, 0, 0, 200, 1, 2, 3]).is_err());
    }

    #[test]
    fn a_shape_that_disagrees_with_the_payload_is_refused() {
        let values = vec![1.0f32; 4];
        let mut framed = encode(
            &serde_json::json!({}),
            Some((&values, vec![1, 4])),
            WireDtype::F16,
        )
        .unwrap();
        framed.truncate(framed.len() - 2);
        assert!(decode(&framed).is_err());
    }

    fn sample_batch() -> BatchFrame {
        BatchFrame {
            version: BATCH_FRAME_VERSION,
            kind: "hidden-state".into(),
            batch_id: "batch-001".into(),
            items: vec![
                BatchItem {
                    item_id: "item-a".into(),
                    session_id: "session-a".into(),
                    shape: vec![1, 2],
                    dtype: "f16".into(),
                    offset: 0,
                    len: 4,
                    meta: serde_json::json!({"phase": "prefill"}),
                },
                BatchItem {
                    item_id: "item-b".into(),
                    session_id: "session-b".into(),
                    shape: vec![1, 2],
                    dtype: "f16".into(),
                    offset: 4,
                    len: 4,
                    meta: serde_json::json!({"phase": "step", "token": 7}),
                },
            ],
            payload: (0u8..8).collect(),
        }
    }

    fn rewrite_batch_header(mut framed: Vec<u8>, rewrite: impl FnOnce(&mut Value)) -> Vec<u8> {
        let head_len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        let mut header: Value = serde_json::from_slice(&framed[4..4 + head_len]).unwrap();
        rewrite(&mut header);
        let payload = framed.split_off(4 + head_len);
        let head = serde_json::to_vec(&header).unwrap();
        let mut rewritten = Vec::with_capacity(4 + head.len() + payload.len());
        rewritten.extend_from_slice(&(head.len() as u32).to_be_bytes());
        rewritten.extend_from_slice(&head);
        rewritten.extend_from_slice(&payload);
        rewritten
    }

    #[test]
    fn batch_frame_round_trip_preserves_items_and_payload() {
        let original = sample_batch();
        let framed = encode_batch(&original).unwrap();
        let decoded = decode_batch(&framed).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn single_item_batch_is_accepted_by_the_rolling_upgrade_adapter() {
        let meta = serde_json::json!({
            "session_id": "session-single",
            "chunk_index": 2,
        });
        let values = vec![0.5_f32, -1.25];
        let framed =
            encode_batch_single(&meta, Some((&values, vec![1, 2])), WireDtype::F16).unwrap();
        let (decoded_meta, decoded_tensor) = decode_inference_request(&framed).unwrap();
        assert_eq!(decoded_meta["session_id"], "session-single");
        assert_eq!(decoded_meta["frame_version"], BATCH_FRAME_VERSION);
        let (decoded_values, shape) = decoded_tensor.unwrap();
        assert_eq!(shape, vec![1, 2]);
        assert!((decoded_values[0] - values[0]).abs() < 0.001);
        assert!((decoded_values[1] - values[1]).abs() < 0.001);
    }

    #[test]
    fn batch_frame_rejects_overlapping_offsets() {
        let framed = encode_batch(&sample_batch()).unwrap();
        let framed = rewrite_batch_header(framed, |header| {
            header["items"][1]["offset"] = Value::from(2);
        });
        let error = decode_batch(&framed).unwrap_err().to_string();
        assert!(error.contains("sobrepõem"), "erro inesperado: {error}");
    }

    #[test]
    fn batch_frame_rejects_truncated_payload() {
        let mut framed = encode_batch(&sample_batch()).unwrap();
        framed.pop();
        assert!(decode_batch(&framed).is_err());
    }

    #[test]
    fn batch_frame_rejects_unknown_dtype() {
        let framed = encode_batch(&sample_batch()).unwrap();
        let framed = rewrite_batch_header(framed, |header| {
            header["items"][0]["dtype"] = Value::from("int8");
        });
        let error = decode_batch(&framed).unwrap_err().to_string();
        assert!(
            error.contains("dtype de fio desconhecido"),
            "erro inesperado: {error}"
        );
    }

    #[test]
    fn batch_frame_rejects_duplicate_item_ids() {
        let framed = encode_batch(&sample_batch()).unwrap();
        let framed = rewrite_batch_header(framed, |header| {
            header["items"][1]["item_id"] = Value::from("item-a");
        });
        let error = decode_batch(&framed).unwrap_err().to_string();
        assert!(
            error.contains("item_id duplicado"),
            "erro inesperado: {error}"
        );
    }

    #[test]
    fn batch_frame_rejects_payload_larger_than_the_wire_limit() {
        let mut frame = sample_batch();
        frame.items[0].shape = vec![1, MAX_FRAME_PAYLOAD_BYTES / 2 + 1];
        frame.items[0].len = MAX_FRAME_PAYLOAD_BYTES + 2;
        frame.items[1].offset = frame.items[0].len;
        frame.items[1].len = 0;
        frame.payload = vec![0; MAX_FRAME_PAYLOAD_BYTES + 2];
        let error = encode_batch(&frame).unwrap_err().to_string();
        assert!(error.contains("limite"), "erro inesperado: {error}");
    }
}
