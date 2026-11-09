//! Re-execution for the metering ledger (spec 7 of
//! `docs/superpowers/specs/2026-11-08-gpu-work-metering-design.md`).
//!
//! `node.reexecute` runs on the verifier (the ledger admin, or the machine
//! `metering.verifier` names). Given the head's computed token sequence, the
//! plan's ranges and the wire format of each boundary, it reproduces what
//! every hop up to the audited one received and computed:
//!
//! 1. finds the whole model locally by its catalog name;
//! 2. for each range in plan order, cuts it (a header-only virtual slice when
//!    the engine supports it, else a real slice under the virtual slice
//!    folder), loads it alone, and runs the sequence teacher-forced: token
//!    ids into the first range, the previous range's output into the next;
//! 3. between ranges, applies the boundary's codec
//!    (`wire::decode(wire::encode(..))` with the sending hop's format), so a
//!    lossy f8/i4 boundary feeds the next range exactly what the real hop
//!    decoded (the "decoded" choice of the spec);
//! 4. for the audited hop, gathers the rows at each chunk's positions and
//!    runs the TOPLOC check against the chunk's proof.
//!
//! The work is blocking and slow (loading slices, a prefill of the whole
//! conversation per range): it runs on a blocking thread and answers when
//! done. One re-execution at a time per node.

use crate::gateway::GatewayState;
use metering_core::toploc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Chunks checked per re-execution; the ledger picks a random subset.
pub(crate) const MAX_AUDIT_CHUNKS: usize = 32;

pub(crate) fn decode_tokens(b64: &str) -> Result<Vec<i32>, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| format!("tokens: {e}"))?;
    if bytes.len() % 4 != 0 {
        return Err("tokens: not whole i32 values".into());
    }
    Ok(bytes.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

static ONE_AT_A_TIME: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Debug, Deserialize)]
pub(crate) struct ReexecuteRequest {
    /// Catalog name of the model (the receipts' `model.id`).
    pub(crate) model: String,
    /// The plan's ranges in order, `[first, last)`.
    pub(crate) ranges: Vec<[u32; 2]>,
    /// Wire format each range sent on (`receipt.wire.out`); the last is unused.
    #[serde(default)]
    pub(crate) wire_out: Vec<String>,
    /// The head's computed sequence, LE i32 in base64 (peer bodies are
    /// capped at 256 KiB, which holds about 48k tokens this way).
    pub(crate) tokens: String,
    /// Index into `ranges` of the audited hop.
    pub(crate) audited: usize,
    /// The audited hop's chunk leaves (`toploc::encode_leaf`), hex.
    pub(crate) chunks: Vec<String>,
    #[serde(default)]
    pub(crate) params: Option<toploc::Params>,
    /// The audited receipt's engine; only llama.cpp re-executes here.
    #[serde(default)]
    pub(crate) engine: Option<String>,
}

/// Verdict of one re-execution.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ReexecuteVerdict {
    pub(crate) pass: bool,
    pub(crate) chunks: Vec<toploc::Verdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) void: Option<String>,
}

/// Finished and running jobs, by id. A peer call cannot wait minutes, so
/// `node.reexecute` answers a job id at once and the ledger polls
/// `node.reexecute_status`. Kept for an hour.
fn jobs() -> &'static std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Option<Value>)>> {
    static JOBS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Option<Value>)>>> =
        std::sync::OnceLock::new();
    JOBS.get_or_init(Default::default)
}

const JOB_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// `node.reexecute`: validates, starts the job and answers `{job}`.
pub(crate) async fn reexecute(state: &GatewayState, params: &Value) -> Result<Value, String> {
    let request: ReexecuteRequest = serde_json::from_value(params.clone()).map_err(|e| format!("invalid request: {e}"))?;
    validate(&request)?;
    let tokens = decode_tokens(&request.tokens)?;
    if let Some(engine) = request.engine.as_deref().filter(|e| *e != "llama.cpp") {
        return Ok(json!({ "job": finished_job(json!(void(format!("re-executing {engine} work needs its engine.verify")))) }));
    }
    let found = crate::webui::whole_model_by_label(state, &request.model).await;
    let id = uuid::Uuid::new_v4().simple().to_string();
    {
        let mut jobs = jobs().lock().map_err(|e| e.to_string())?;
        jobs.retain(|_, (at, _)| at.elapsed() < JOB_TTL);
        jobs.insert(id.clone(), (std::time::Instant::now(), None));
    }
    let job = id.clone();
    tokio::spawn(async move {
        let verdict = match found {
            None => json!(void("the verifier does not hold this model".into())),
            Some((model_path, total)) => {
                let _permit = ONE_AT_A_TIME.acquire().await;
                tokio::task::spawn_blocking(move || json!(run(&request, &tokens, &model_path, total)))
                    .await
                    .unwrap_or_else(|e| json!(void(format!("the re-execution crashed: {e}"))))
            }
        };
        if let Ok(mut jobs) = jobs().lock() {
            jobs.insert(job, (std::time::Instant::now(), Some(verdict)));
        }
    });
    Ok(json!({ "job": id }))
}

/// A job that is already decided.
fn finished_job(verdict: Value) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    if let Ok(mut jobs) = jobs().lock() {
        jobs.insert(id.clone(), (std::time::Instant::now(), Some(verdict)));
    }
    id
}

/// `node.reexecute_status`: `{done: false}` while it runs, else the verdict.
pub(crate) fn reexecute_status(params: &Value) -> Result<Value, String> {
    let id = params["job"].as_str().ok_or("job is required")?;
    let jobs = jobs().lock().map_err(|e| e.to_string())?;
    match jobs.get(id) {
        None => Ok(json!({ "done": true, "verdict": void("unknown job".into()) })),
        Some((_, None)) => Ok(json!({ "done": false })),
        Some((_, Some(verdict))) => Ok(json!({ "done": true, "verdict": verdict })),
    }
}

fn validate(r: &ReexecuteRequest) -> Result<(), String> {
    if r.ranges.is_empty() || r.audited >= r.ranges.len() {
        return Err("the audited hop is not in the plan".into());
    }
    if r.tokens.is_empty() {
        return Err("no tokens to re-execute".into());
    }
    if r.chunks.len() > MAX_AUDIT_CHUNKS {
        return Err("too many chunks for one re-execution".into());
    }
    if r.chunks.is_empty() {
        return Err("no chunks to check".into());
    }
    for pair in r.ranges.windows(2) {
        if pair[0][1] != pair[1][0] {
            return Err("the plan's ranges are not contiguous".into());
        }
    }
    if r.ranges[0][0] != 0 || r.ranges.iter().any(|[a, b]| a >= b) {
        return Err("the plan does not start at layer 0".into());
    }
    Ok(())
}

/// The rows of `output` (width `width`) at `positions`, flattened.
pub(crate) fn rows_at(output: &[f32], width: usize, positions: &[u32]) -> Option<Vec<f32>> {
    let mut rows = Vec::with_capacity(positions.len() * width);
    for p in positions {
        let start = *p as usize * width;
        rows.extend_from_slice(output.get(start..start + width)?);
    }
    Some(rows)
}

/// Checks the audited hop's chunks against its recomputed output.
pub(crate) fn check_chunks(output: &[f32], width: usize, chunks: &[String], params: &toploc::Params) -> ReexecuteVerdict {
    let mut verdicts = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let Ok(leaf) = hex::decode(chunk) else {
            return failed("a chunk is not hex", verdicts);
        };
        let Ok((positions, proof)) = toploc::decode_leaf(&leaf) else {
            return failed("a chunk is malformed", verdicts);
        };
        let Some(rows) = rows_at(output, width, &positions) else {
            return failed("a chunk names positions past the sequence", verdicts);
        };
        match toploc::check_chunk(proof, &rows, params) {
            Ok(v) => verdicts.push(v),
            Err(e) => return failed(&format!("a proof does not open: {e}"), verdicts),
        }
    }
    ReexecuteVerdict { pass: verdicts.iter().all(|v| v.pass), chunks: verdicts, void: None }
}

/// A proof that does not open, or names rows that do not exist, is the
/// node's fault: a failed sample, not a void one.
fn failed(reason: &str, chunks: Vec<toploc::Verdict>) -> ReexecuteVerdict {
    tracing::warn!("metering re-execution: {reason}");
    ReexecuteVerdict { pass: false, chunks, void: None }
}

fn void(reason: String) -> ReexecuteVerdict {
    tracing::warn!("metering re-execution void: {reason}");
    ReexecuteVerdict { pass: false, chunks: vec![], void: Some(reason) }
}

fn run(r: &ReexecuteRequest, tokens: &[i32], model_path: &str, total: Option<u32>) -> ReexecuteVerdict {
    let params = r.params.unwrap_or_default();
    let total = total.unwrap_or(r.ranges.last().map(|x| x[1]).unwrap_or(0));
    let mut input: Option<(Vec<f32>, usize)> = None;
    for (index, [first, last]) in r.ranges.iter().copied().enumerate().take(r.audited + 1) {
        let is_first = first == 0;
        let is_last = last >= total;
        let out = match run_range(model_path, first, last, total, is_first, is_last, tokens, input.take()) {
            Ok(out) => out,
            Err(e) => return void(format!("range {first}..{last}: {e}")),
        };
        if index == r.audited {
            return check_chunks(&out.0, out.1, &r.chunks, &params);
        }
        // The boundary's codec: what the next hop really decoded.
        let dtype = r.wire_out.get(index).map(String::as_str).unwrap_or("f16");
        match roundtrip(&out.0, out.1, dtype) {
            Ok(values) => input = Some((values, out.1)),
            Err(e) => return void(format!("codec {dtype}: {e}")),
        }
    }
    void("the audited hop was not reached".into())
}

/// `wire::decode(wire::encode(values))` with `dtype`.
pub(crate) fn roundtrip(values: &[f32], width: usize, dtype: &str) -> anyhow::Result<Vec<f32>> {
    let dtype = crate::wire::WireDtype::parse(dtype).map_err(|e| anyhow::anyhow!("{e}"))?;
    let rows = values.len() / width.max(1);
    let bytes = crate::wire::encode(&json!({}), Some((values, vec![rows, width])), dtype)?;
    match crate::wire::decode(&bytes)? {
        (_, Some((decoded, _))) => Ok(decoded),
        _ => anyhow::bail!("the frame lost its tensor"),
    }
}

/// Cuts `[first, last)` of the model, loads it alone, runs the sequence and
/// returns the range's output rows and width.
#[allow(clippy::too_many_arguments)]
fn run_range(
    model_path: &str,
    first: u32,
    last: u32,
    total: u32,
    is_first: bool,
    is_last: bool,
    tokens: &[i32],
    input: Option<(Vec<f32>, usize)>,
) -> anyhow::Result<(Vec<f32>, usize)> {
    let dir = crate::plan::virtual_slice_dir().join("metering");
    std::fs::create_dir_all(&dir)?;
    let whole = first == 0 && last >= total;
    let slice: PathBuf = if whole {
        PathBuf::from(model_path)
    } else {
        let dst = dir.join(format!("audit-{first}-{last}.gguf"));
        let end_inclusive = last - 1;
        if crate::plan::virtual_slices_enabled() {
            crate::splitter::write_virtual_slice(std::path::Path::new(model_path), &dst, first, end_inclusive, is_first, is_last)?;
        } else if !dst.exists() {
            crate::splitter::split(std::path::Path::new(model_path), &dst, first, end_inclusive, is_first, is_last, |_| {})?;
        }
        dst
    };
    let mut node = crate::runtime::NodeConfig::from_params(&crate::webui::RuntimeParams::default(), true);
    node.model_path = slice.to_string_lossy().into_owned();
    node.start_layer = first;
    node.end_layer = last - 1;
    node.total_layers = total;
    node.max_sessions = 1;
    node.n_ctx = (tokens.len() as u32 + 64).next_power_of_two().max(512);
    node.mtp_enabled = false;
    node.speculative_decoding_enabled = false;
    node.peer_count = 2;
    let engine_cfg = crate::runtime::engine_config_from_node(&node, true, false, Vec::new())?;
    let cache = crate::llm::ModelCache::new();
    let mut engine = crate::llm::LlmEngine::load_with_cache(&node.model_path, &engine_cfg, &cache, false)?;
    let width = engine.n_embd;
    let batch = engine_cfg.n_batch.max(1) as usize;
    let mut output = Vec::with_capacity(tokens.len() * width);
    let result = (|| -> anyhow::Result<()> {
        match &input {
            None => {
                for (i, chunk) in tokens.chunks(batch).enumerate() {
                    engine.decode_tokens(chunk, (i * batch) as u32, 0, false)?;
                    output.extend(engine.take_residual(chunk.len())?.values);
                }
            }
            Some((hidden, in_width)) => {
                let rows = hidden.len() / in_width.max(&1);
                for start in (0..rows).step_by(batch) {
                    let n = batch.min(rows - start);
                    let slice = &hidden[start * in_width..(start + n) * in_width];
                    let shape = vec![n, *in_width];
                    let ids = &tokens[start.min(tokens.len())..(start + n).min(tokens.len())];
                    engine.decode_hidden_with_positions(slice, &shape, n, start as u32, 0, false, Some(ids), None)?;
                    output.extend(engine.take_residual(n)?.values);
                }
            }
        }
        Ok(())
    })();
    engine.unload();
    result?;
    Ok((output, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(chunks: Vec<String>) -> ReexecuteRequest {
        ReexecuteRequest {
            model: "m".into(),
            ranges: vec![[0, 10], [10, 20]],
            wire_out: vec!["f8".into()],
            tokens: "AQAAAAIAAAA=".into(),
            audited: 1,
            chunks,
            params: None,
            engine: None,
        }
    }

    #[test]
    fn requests_with_a_broken_plan_are_refused() {
        assert!(validate(&request(vec!["00".into()])).is_ok());
        let mut r = request(vec!["00".into()]);
        r.ranges = vec![[0, 10], [11, 20]];
        assert!(validate(&r).is_err());
        let mut r = request(vec!["00".into()]);
        r.audited = 2;
        assert!(validate(&r).is_err());
        assert!(validate(&request(vec![])).is_err());
        assert!(validate(&request(vec!["00".into(); MAX_AUDIT_CHUNKS + 1])).is_err());
        assert_eq!(decode_tokens("AQAAAAIAAAA=").unwrap(), vec![1, 2]);
        assert!(decode_tokens("AQA=").is_err());
    }

    fn hidden(rows: usize, width: usize, seed: u32) -> Vec<f32> {
        (0..rows * width)
            .map(|i| (((i as u32).wrapping_mul(2654435761) ^ seed.wrapping_mul(0x9E37_79B9)) >> 8 & 0xffff) as f32 / 3000.0 - 10.0)
            .collect()
    }

    fn leaf(output: &[f32], width: usize, positions: &[u32]) -> String {
        let rows = rows_at(output, width, positions).unwrap();
        hex::encode(toploc::encode_leaf(positions, &toploc::prove_chunk(&rows, 128).unwrap()))
    }

    #[test]
    fn an_honest_recomputation_passes_and_another_fails() {
        let width = 256;
        let real = hidden(40, width, 1);
        let positions: Vec<u32> = (0..32).collect();
        let chunks = vec![leaf(&real, width, &positions), leaf(&real, width, &[32, 33, 35])];
        let params = toploc::Params::default();
        assert!(check_chunks(&real, width, &chunks, &params).pass);
        // Small numeric noise of another GPU still passes.
        let noisy: Vec<f32> = real.iter().map(|v| v * 1.001).collect();
        assert!(check_chunks(&noisy, width, &chunks, &params).pass);
        // Another computation fails, and is a failure, not void.
        let verdict = check_chunks(&hidden(40, width, 99), width, &chunks, &params);
        assert!(!verdict.pass && verdict.void.is_none());
    }

    #[test]
    fn a_chunk_past_the_sequence_or_malformed_fails() {
        let width = 256;
        let real = hidden(4, width, 1);
        let params = toploc::Params::default();
        let past = hex::encode(toploc::encode_leaf(&[9], &[0xff, 0xd9, 0, 1]));
        assert!(!check_chunks(&real, width, &[past], &params).pass);
        assert!(!check_chunks(&real, width, &["zz".into()], &params).pass);
    }

    #[test]
    fn a_lossy_boundary_round_trip_is_what_the_receiver_decodes() {
        let width = 128;
        let values = hidden(3, width, 5);
        for dtype in ["f16", "f8", "i4g64"] {
            let ours = roundtrip(&values, width, dtype).unwrap();
            let wire = crate::wire::WireDtype::parse(dtype).unwrap();
            let bytes = crate::wire::encode(&json!({"x": 1}), Some((&values, vec![3, width])), wire).unwrap();
            let (_, Some((theirs, _))) = crate::wire::decode(&bytes).unwrap() else { panic!() };
            assert_eq!(ours, theirs, "{dtype}");
        }
    }
}
