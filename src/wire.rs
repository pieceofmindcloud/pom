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
//! Two deliberate choices, both about bytes on the wire — which is what decides
//! time-to-first-word once the cluster runs over the internet rather than a LAN:
//!
//! * the body is **binary**, where it used to be base64 (which inflated every
//!   transfer by a third for nothing);
//! * the tensor travels as **f16**, half the size of f32. The maths stays f32
//!   on both ends; f16 is the precision the GPU computes in anyway, so this
//!   costs no quality — unlike int8, which was considered and refused.

use anyhow::{anyhow, Result};
use serde_json::Value;

/// Precision a tensor takes on the wire. Adding one means adding a branch in
/// `encode`/`decode` and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireDtype {
    F32,
    F16,
}

impl WireDtype {
    pub fn as_str(&self) -> &'static str {
        match self {
            WireDtype::F32 => "f32",
            WireDtype::F16 => "f16",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "f32" => Ok(WireDtype::F32),
            "f16" => Ok(WireDtype::F16),
            other => Err(anyhow!("dtype de fio desconhecido: {other}")),
        }
    }

    pub fn bytes_per_value(&self) -> usize {
        match self {
            WireDtype::F32 => 4,
            WireDtype::F16 => 2,
        }
    }
}

/// Frame a header and an optional tensor.
pub fn encode(meta: &Value, tensor: Option<(&[f32], [usize; 2])>, dtype: WireDtype) -> Result<Vec<u8>> {
    let mut meta = meta.clone();
    let payload = match tensor {
        Some((values, shape)) => {
            if values.len() != shape[0] * shape[1] {
                return Err(anyhow!(
                    "tensor com {} valores não bate com shape {:?}",
                    values.len(),
                    shape
                ));
            }
            meta["dtype"] = Value::from(dtype.as_str());
            meta["shape"] = Value::from(vec![shape[0], shape[1]]);
            match dtype {
                WireDtype::F32 => values.iter().flat_map(|v| v.to_le_bytes()).collect(),
                WireDtype::F16 => values
                    .iter()
                    .flat_map(|v| f32_to_f16_bits(*v).to_le_bytes())
                    .collect(),
            }
        }
        None => Vec::new(),
    };

    let head = serde_json::to_vec(&meta)?;
    let mut out = Vec::with_capacity(4 + head.len() + payload.len());
    out.extend_from_slice(&(head.len() as u32).to_be_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Inverse of [`encode`]. The tensor always comes back as f32 — reduced
/// precision is a property of the wire, never of the maths.
pub fn decode(buf: &[u8]) -> Result<(Value, Option<(Vec<f32>, [usize; 2])>)> {
    if buf.len() < 4 {
        return Err(anyhow!("quadro curto demais: {} bytes", buf.len()));
    }
    let head_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + head_len {
        return Err(anyhow!("cabeçalho truncado"));
    }
    let meta: Value = serde_json::from_slice(&buf[4..4 + head_len])?;
    let payload = &buf[4 + head_len..];
    if payload.is_empty() {
        return Ok((meta, None));
    }

    let dtype = WireDtype::parse(meta.get("dtype").and_then(|v| v.as_str()).unwrap_or("f32"))?;
    let shape: Vec<usize> = meta
        .get("shape")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as usize)).collect())
        .unwrap_or_default();
    if shape.len() != 2 {
        return Err(anyhow!("shape inválido: {:?}", shape));
    }
    let expected = shape[0] * shape[1];

    let values: Vec<f32> = match dtype {
        WireDtype::F32 => payload
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        WireDtype::F16 => payload
            .chunks_exact(2)
            .map(|c| f16_bits_to_f32(u16::from_le_bytes([c[0], c[1]])))
            .collect(),
    };
    if values.len() != expected {
        return Err(anyhow!(
            "tensor com {} valores, esperado {expected}",
            values.len()
        ));
    }
    Ok((meta, Some((values, [shape[0], shape[1]]))))
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
        return sign | 0x7c00 | if mantissa != 0 && exp == 0x1f + 112 { 0x0200 } else { 0 };
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
        let rounded = if (mantissa & round_bit) != 0 { half + 1 } else { half };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tensor_survives_a_round_trip_in_f32() {
        let values = vec![0.0f32, 1.5, -2.25, 1e-8, 1e8];
        let meta = serde_json::json!({ "session_id": "abc" });
        let framed = encode(&meta, Some((&values, [1, 5])), WireDtype::F32).unwrap();
        let (back_meta, tensor) = decode(&framed).unwrap();
        assert_eq!(back_meta["session_id"], "abc");
        let (back, shape) = tensor.unwrap();
        assert_eq!(shape, [1, 5]);
        assert_eq!(back, values);
    }

    /// f16 is lossy by design; what matters is that it is *close*, and that the
    /// framing survives. Activations live around unit magnitude, which is where
    /// f16 has ~3 decimal digits.
    #[test]
    fn f16_round_trip_stays_close_for_activation_sized_values() {
        let values: Vec<f32> = vec![0.0, 1.0, -1.0, 0.5, -0.125, 12.5, -37.75, 0.001];
        let meta = serde_json::json!({});
        let framed = encode(&meta, Some((&values, [1, 8])), WireDtype::F16).unwrap();
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
        let as_f32 = encode(&meta, Some((&values, [1, 1024])), WireDtype::F32).unwrap();
        let as_f16 = encode(&meta, Some((&values, [1, 1024])), WireDtype::F16).unwrap();
        // Headers differ by a couple of bytes; the payload is what matters.
        assert_eq!(as_f32.len() - as_f16.len(), 1024 * 2);
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
        let mut framed = encode(&serde_json::json!({}), Some((&values, [1, 4])), WireDtype::F16).unwrap();
        framed.truncate(framed.len() - 2);
        assert!(decode(&framed).is_err());
    }
}
