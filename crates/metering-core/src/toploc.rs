//! TOPLOC proofs (spec 3): a Rust port of Prime Intellect's reference
//! implementation (github.com/PrimeIntellect-ai/toploc, MIT License,
//! Copyright (c) 2024 Prime Intellect; paper arXiv:2501.16007, CC BY 4.0).
//!
//! A proof covers one chunk of a hop's output: the hidden state of `chunk`
//! consecutive positions, flattened. The prover keeps the top-k entries by
//! absolute value (signed values, as bf16 bits) and encodes them as the
//! polynomial over Z_65497 that maps each flat index (reduced by an injective
//! modulus) to its bf16 bits. Serialized: 2 bytes modulus big-endian, then
//! the k coefficients, 2 bytes big-endian each, ascending degree (2 + 2k
//! bytes: 258 for k = 128).
//!
//! The verifier recomputes the hidden state, takes its own top-k indices,
//! evaluates the proof's polynomial at them and compares bf16 exponent and
//! mantissa: the chunk fails when the exponent mismatches exceed `t_exp`, or
//! the mean or median mantissa difference (over positions whose exponents
//! match) exceeds `t_mean` / `t_median`. The paper's bf16 thresholds are
//! 38 / 10 / 8 for k = 128.
//!
//! Differences from the reference, all deliberate:
//! - Every chunk is `chunk` positions, the prompt included (the reference
//!   proves the whole prompt as one chunk). Uniform chunks let a challenge
//!   ask for any chunk of a session.
//! - The prover takes f32 and rounds to bf16 (nearest even) itself; the
//!   verifier rounds its recomputation the same way before comparing, so both
//!   mantissas are 7 bits.
//! - Ties in the top-k are broken by the lower index, so the choice is
//!   deterministic across machines.
//! - The median is the upper middle, as the reference's C++ path.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const MOD_P: u32 = 65_497;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub t_exp: u32,
    pub t_mean: f64,
    pub t_median: f64,
}

/// The network constants of `toploc/v1` (spec 3). Chunk and k follow the
/// paper; thresholds are the paper's bf16 ones until calibration at the end
/// of a layer range (plan task 2.5) replaces them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Params {
    pub chunk: u32,
    pub k: u32,
    pub thresholds: Thresholds,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            chunk: 32,
            k: 128,
            thresholds: Thresholds { t_exp: 38, t_mean: 10.0, t_median: 8.0 },
        }
    }
}

pub const PARAMS_VERSION: &str = "toploc/v1";

/// f32 to bf16 bits, round to nearest even; NaN stays a quiet NaN.
pub fn bf16_bits(v: f32) -> u16 {
    let bits = v.to_bits();
    if v.is_nan() {
        return ((bits >> 16) as u16) | 0x0040;
    }
    let lsb = (bits >> 16) & 1;
    ((bits.wrapping_add(0x7FFF + lsb)) >> 16) as u16
}

/// Indices of the k largest |v|, ties by lower index, in that order.
fn topk(values: &[f32], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..values.len()).collect();
    let key = |i: &usize| {
        let a = values[*i].abs();
        if a.is_nan() { f32::INFINITY } else { a }
    };
    let k = k.min(values.len());
    if k == 0 {
        return Vec::new();
    }
    idx.select_nth_unstable_by(k - 1, |a, b| key(b).total_cmp(&key(a)).then(a.cmp(b)));
    idx.truncate(k);
    idx.sort_by(|a, b| key(b).total_cmp(&key(a)).then(a.cmp(b)));
    idx
}

fn pow_mod(mut b: u64, mut e: u64) -> u64 {
    let p = MOD_P as u64;
    let mut r = 1u64;
    b %= p;
    while e > 0 {
        if e & 1 == 1 {
            r = r * b % p;
        }
        b = b * b % p;
        e >>= 1;
    }
    r
}

fn inv(a: u64) -> u64 {
    pow_mod(a, MOD_P as u64 - 2)
}

/// The largest modulus `<= 65497` under which every index stays distinct.
fn injective_modulus(xs: &[usize]) -> Option<u32> {
    let mut seen = vec![false; MOD_P as usize + 1];
    'outer: for m in (1..=MOD_P).rev() {
        let mut touched = Vec::with_capacity(xs.len());
        for &x in xs {
            let r = x % m as usize;
            if seen[r] {
                for t in touched {
                    seen[t] = false;
                }
                continue 'outer;
            }
            seen[r] = true;
            touched.push(r);
        }
        return Some(m);
    }
    None
}

/// Monomial coefficients, ascending, of the polynomial through (x_i, y_i)
/// over Z_p, by Newton divided differences.
fn interpolate(xs: &[u64], ys: &[u64]) -> Vec<u64> {
    let p = MOD_P as u64;
    let k = xs.len();
    let mut dd: Vec<u64> = ys.iter().map(|y| y % p).collect();
    for j in 1..k {
        for i in (j..k).rev() {
            let num = (dd[i] + p - dd[i - 1]) % p;
            let den = (xs[i] + p - xs[i - j]) % p;
            dd[i] = num * inv(den) % p;
        }
    }
    // poly = dd[k-1]; poly = poly * (X - x_i) + dd[i], from the top.
    let mut poly = vec![0u64; k.max(1)];
    if k == 0 {
        return poly;
    }
    poly[0] = dd[k - 1];
    let mut deg = 0usize;
    for i in (0..k - 1).rev() {
        // multiply by (X - x_i)
        deg += 1;
        for d in (1..=deg).rev() {
            poly[d] = (poly[d - 1] + p - poly[d] * xs[i] % p) % p;
        }
        poly[0] = (p - poly[0] * xs[i] % p) % p;
        poly[0] = (poly[0] + dd[i]) % p;
    }
    poly
}

fn evaluate(coeffs: &[u16], x: u64) -> u16 {
    let p = MOD_P as u64;
    let mut r = 0u64;
    for c in coeffs.iter().rev() {
        r = (r * x + *c as u64) % p;
    }
    r as u16
}

/// The proof of one chunk (flattened hidden state of its positions).
pub fn prove_chunk(values: &[f32], k: u32) -> Result<Vec<u8>> {
    let k = k as usize;
    if values.len() < k || k == 0 {
        bail!("a chunk needs at least k values");
    }
    let idx = topk(values, k);
    let Some(modulus) = injective_modulus(&idx) else {
        bail!("no injective modulus for this chunk");
    };
    let xs: Vec<u64> = idx.iter().map(|i| (*i % modulus as usize) as u64).collect();
    let ys: Vec<u64> = idx.iter().map(|i| bf16_bits(values[*i]) as u64).collect();
    let coeffs = interpolate(&xs, &ys);
    let mut out = Vec::with_capacity(2 + 2 * k);
    out.extend_from_slice(&(modulus as u16).to_be_bytes());
    for c in coeffs.iter().take(k) {
        out.extend_from_slice(&(*c as u16).to_be_bytes());
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub exp_mismatches: u32,
    pub mant_err_mean: f64,
    pub mant_err_median: f64,
    pub pass: bool,
}

fn parse(proof: &[u8]) -> Result<(u32, Vec<u16>)> {
    if proof.len() < 4 || proof.len() % 2 != 0 {
        bail!("malformed proof");
    }
    let modulus = u16::from_be_bytes([proof[0], proof[1]]) as u32;
    if modulus == 0 {
        bail!("null proof");
    }
    let coeffs = proof[2..].chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    Ok((modulus, coeffs))
}

/// Checks a chunk proof against the verifier's recomputation of the same
/// chunk.
pub fn check_chunk(proof: &[u8], recomputed: &[f32], params: &Params) -> Result<Verdict> {
    let (modulus, coeffs) = parse(proof)?;
    let k = params.k as usize;
    if coeffs.len() != k {
        bail!("proof has {} coefficients, expected {k}", coeffs.len());
    }
    if recomputed.len() < k {
        bail!("recomputed chunk is shorter than k");
    }
    let idx = topk(recomputed, k);
    let mut mismatches = 0u32;
    let mut errs: Vec<f64> = Vec::with_capacity(k);
    for i in idx {
        let proved = evaluate(&coeffs, (i % modulus as usize) as u64);
        let mine = bf16_bits(recomputed[i]);
        let (pe, pm) = ((proved & 0x7F80) >> 7, proved & 0x007F);
        let (me, mm) = ((mine & 0x7F80) >> 7, mine & 0x007F);
        if pe != me {
            mismatches += 1;
        } else {
            errs.push((pm as f64 - mm as f64).abs());
        }
    }
    let (mean, median) = if errs.is_empty() {
        (2f64.powi(64), 2f64.powi(64))
    } else {
        let mean = errs.iter().sum::<f64>() / errs.len() as f64;
        errs.sort_by(|a, b| a.total_cmp(b));
        (mean, errs[errs.len() / 2])
    };
    let t = &params.thresholds;
    Ok(Verdict {
        exp_mismatches: mismatches,
        mant_err_mean: mean,
        mant_err_median: median,
        pass: mismatches <= t.t_exp && mean <= t.t_mean && median <= t.t_median,
    })
}

/// Splits a session's output (rows of `width`) into chunks of `chunk` rows;
/// the last may be short. A short chunk with fewer than k values has no proof.
pub fn chunks<'a>(rows: &'a [f32], width: usize, chunk: u32) -> impl Iterator<Item = &'a [f32]> + 'a {
    rows.chunks(width.max(1) * chunk.max(1) as usize)
}

/// A stored chunk: the positions its rows cover (a chunk may skip positions,
/// for example the speculative batches it does not prove) and its proof.
/// Layout: u32 LE count, the positions as u32 LE, then the proof. This is the
/// Merkle leaf, so the positions are signed together with the proof.
pub fn encode_leaf(positions: &[u32], proof: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + positions.len() * 4 + proof.len());
    out.extend_from_slice(&(positions.len() as u32).to_le_bytes());
    for p in positions {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out.extend_from_slice(proof);
    out
}

pub fn decode_leaf(leaf: &[u8]) -> Result<(Vec<u32>, &[u8])> {
    if leaf.len() < 4 {
        bail!("malformed chunk");
    }
    let n = u32::from_le_bytes([leaf[0], leaf[1], leaf[2], leaf[3]]) as usize;
    let end = 4 + n * 4;
    if leaf.len() < end {
        bail!("malformed chunk");
    }
    let positions = leaf[4..end]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok((positions, &leaf[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    fn hidden(seed: u64, n: usize) -> Vec<f32> {
        let mut s = seed;
        (0..n).map(|_| lcg(&mut s) * 4.0).collect()
    }

    #[test]
    fn the_modulus_is_prime() {
        let n = MOD_P;
        assert!((2..=(n as f64).sqrt() as u32).all(|i| n % i != 0));
    }

    #[test]
    fn proof_size_matches_the_paper_and_the_reference_header() {
        let v = hidden(1, 32 * 64);
        let p = prove_chunk(&v, 128).unwrap();
        assert_eq!(p.len(), 258);
        // Small chunks keep 65497 (0xffd9) as modulus, as the reference README shows.
        assert_eq!(&p[..2], &[0xff, 0xd9]);
        assert_eq!(prove_chunk(&hidden(2, 16), 4).unwrap().len(), 10);
    }

    #[test]
    fn the_polynomial_returns_every_top_value() {
        let v = hidden(3, 2048);
        let p = prove_chunk(&v, 128).unwrap();
        let (m, coeffs) = parse(&p).unwrap();
        for i in topk(&v, 128) {
            assert_eq!(evaluate(&coeffs, (i % m as usize) as u64) as u32, bf16_bits(v[i]) as u32 % MOD_P);
        }
    }

    #[test]
    fn the_same_computation_passes_exactly() {
        let v = hidden(4, 4096);
        let p = prove_chunk(&v, 128).unwrap();
        let verdict = check_chunk(&p, &v, &Params::default()).unwrap();
        assert!(verdict.pass);
        assert_eq!(verdict.exp_mismatches, 0);
        assert_eq!(verdict.mant_err_mean, 0.0);
    }

    #[test]
    fn small_numeric_noise_of_another_gpu_passes() {
        let v = hidden(5, 4096);
        let p = prove_chunk(&v, 128).unwrap();
        let mut s = 99u64;
        let noisy: Vec<f32> = v.iter().map(|x| x * (1.0 + lcg(&mut s) * 0.002)).collect();
        assert!(check_chunk(&p, &noisy, &Params::default()).unwrap().pass);
    }

    #[test]
    fn another_computation_fails() {
        let p = prove_chunk(&hidden(6, 4096), 128).unwrap();
        let verdict = check_chunk(&p, &hidden(7, 4096), &Params::default()).unwrap();
        assert!(!verdict.pass, "{verdict:?}");
    }

    #[test]
    fn a_coarse_quantization_fails() {
        let v = hidden(8, 4096);
        let p = prove_chunk(&v, 128).unwrap();
        // 4-bit-like rounding of every value.
        let q: Vec<f32> = v.iter().map(|x| (x * 2.0).round() / 2.0).collect();
        assert!(!check_chunk(&p, &q, &Params::default()).unwrap().pass);
    }

    #[test]
    fn large_chunks_find_an_injective_modulus() {
        let v = hidden(9, 32 * 8192);
        let p = prove_chunk(&v, 128).unwrap();
        assert!(check_chunk(&p, &v, &Params::default()).unwrap().pass);
    }

    #[test]
    fn malformed_and_null_proofs_are_refused() {
        let v = hidden(10, 4096);
        assert!(check_chunk(&[0, 0, 1, 2], &v, &Params::default()).is_err());
        assert!(check_chunk(&[1, 2, 3], &v, &Params::default()).is_err());
        assert!(prove_chunk(&v[..10], 128).is_err());
    }

    #[test]
    fn a_leaf_round_trips_positions_and_proof() {
        let leaf = encode_leaf(&[3, 4, 9], &[1, 2, 3, 4]);
        let (pos, proof) = decode_leaf(&leaf).unwrap();
        assert_eq!(pos, vec![3, 4, 9]);
        assert_eq!(proof, &[1, 2, 3, 4]);
        assert!(decode_leaf(&[5, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn bf16_rounds_to_nearest_even() {
        assert_eq!(bf16_bits(1.0), 0x3F80);
        assert_eq!(bf16_bits(-2.0), 0xC000);
        assert_eq!(bf16_bits(f32::from_bits(0x3F80_8000)), 0x3F80); // tie to even
        assert_eq!(bf16_bits(f32::from_bits(0x3F81_8000)), 0x3F82);
    }
}
