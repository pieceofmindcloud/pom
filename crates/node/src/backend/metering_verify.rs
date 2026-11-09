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
    if let Some(engine) = request.engine.clone().filter(|e| e != "llama.cpp") {
        return Ok(json!({ "job": start_plugin_audit(state, engine, request, tokens).await }));
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

/// A re-execution on a plugin engine split in stages (`crate::plugin_metering`):
/// the plugin's audit engines load the stages up to the audited one on this
/// machine, chained through loopback relays that reproduce each boundary's
/// codec (the same `to_network`/`from_network` as the real stage link), and
/// this node asks the audited stage for its rows as prompt pieces of the
/// whole sequence, the protocol its later stage uses.
async fn start_plugin_audit(state: &GatewayState, code: String, request: ReexecuteRequest, tokens: Vec<i32>) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    if let Ok(mut jobs) = jobs().lock() {
        jobs.insert(id.clone(), (std::time::Instant::now(), None));
    }
    let (job, state) = (id.clone(), state.clone());
    tokio::spawn(async move {
        let _permit = ONE_AT_A_TIME.acquire().await;
        let verdict = match crate::webui::whole_model_by_label(&state, &request.model).await {
            None => void("the verifier does not hold this model".into()),
            Some((model_path, total)) => {
                let total = total.unwrap_or(request.ranges.last().map(|r| r[1]).unwrap_or(0));
                if request.ranges[request.audited][1] >= total {
                    // The serving stage outputs tokens, not rows: nothing to compare.
                    void("the serving stage keeps no proofs".into())
                } else {
                    let audit = StageAudit::new(&state, &code);
                    let verdict = audit.run(&request, &tokens, &model_path, total).await;
                    audit.stop().await;
                    verdict
                }
            }
        };
        if let Ok(mut jobs) = jobs().lock() {
            jobs.insert(job, (std::time::Instant::now(), Some(json!(verdict))));
        }
    });
    id
}

/// How long an audit stage may take to load (a first load builds its pack).
const AUDIT_LOAD: std::time::Duration = std::time::Duration::from_secs(4 * 3600);
/// Tokens per prompt piece sent to the audited stage.
const AUDIT_PIECE: usize = 256;

struct StageAudit {
    plugins: std::sync::Arc<crate::plugins::PluginManager>,
    code: String,
    ids: std::sync::Mutex<Vec<String>>,
}

impl StageAudit {
    fn new(state: &GatewayState, code: &str) -> Self {
        Self { plugins: state.plugins.clone(), code: code.to_string(), ids: Default::default() }
    }

    async fn query(&self, op: &str, params: Value) -> Result<Value, String> {
        self.plugins
            .query_audit(&self.code, op, json!({ "params": params }))
            .await
            .ok_or_else(|| format!("the plugin '{}' did not answer {op}", self.code))
    }

    /// Starts the audit stage `[first, last)` and waits until it serves its
    /// link; `prev` is the loopback port and secret of its earlier stage.
    async fn start_stage(
        &self,
        model_path: &str,
        first: u32,
        last: u32,
        total: u32,
        prev: Option<(u16, String)>,
    ) -> Result<(u16, String), String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut node = crate::runtime::NodeConfig::from_params(&crate::webui::RuntimeParams::default(), true);
        node.model_path = model_path.to_string();
        node.model_label = std::path::Path::new(model_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        node.start_layer = first;
        node.end_layer = last - 1;
        node.total_layers = total;
        node.max_sessions = 1;
        node.peer_count = 2;
        node.inference_service = Some(self.code.clone());
        let link = match &prev {
            None => json!({}),
            Some((port, token)) => json!({ "token": token, "prev_port": port, "max_inflight": 1 }),
        };
        let (gpus, devices) = crate::plugin_engine::node_gpus().await;
        let load = crate::plugin_engine::load_params(&node, &gpus, &devices, Some(&link));
        let started = self.query("engine.audit.start", json!({ "id": id, "load": load })).await?;
        if started["status"] != "accepted" {
            return Err(started["error"].as_str().unwrap_or("the audit engine was refused").to_string());
        }
        self.ids.lock().map_err(|e| e.to_string())?.push(id.clone());
        let deadline = std::time::Instant::now() + AUDIT_LOAD;
        loop {
            let status = self.query("engine.audit.status", json!({ "id": id })).await?;
            match status["status"]["state"].as_str() {
                Some("ready") => {
                    let port = status["link"]["port"].as_u64().and_then(|p| u16::try_from(p).ok());
                    let token = status["link"]["token"].as_str().map(str::to_string);
                    if let (Some(port), Some(token)) = (port, token) {
                        return Ok((port, token));
                    }
                }
                Some("error") => {
                    return Err(status["status"]["detail"].as_str().unwrap_or("the audit stage failed").to_string())
                }
                _ => {}
            }
            if std::time::Instant::now() > deadline {
                return Err("the audit stage did not load in time".into());
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    async fn run(&self, r: &ReexecuteRequest, tokens: &[i32], model_path: &str, total: u32) -> ReexecuteVerdict {
        let mut prev: Option<(u16, String)> = None;
        for (index, [first, last]) in r.ranges.iter().copied().enumerate().take(r.audited + 1) {
            // A later audit stage reaches its earlier one through a relay that
            // reproduces the boundary's codec.
            let link = match prev.take() {
                None => None,
                Some((port, token)) => {
                    let dtype = r.wire_out.get(index - 1).map(String::as_str).unwrap_or("f16");
                    let dtype = crate::wire::WireDtype::parse(dtype).unwrap_or(crate::wire::WireDtype::F16);
                    match codec_relay(port, dtype).await {
                        Ok(relay_port) => Some((relay_port, token)),
                        Err(e) => return void(format!("relay: {e}")),
                    }
                }
            };
            match self.start_stage(model_path, first, last, total, link).await {
                Ok(served) => prev = Some(served),
                Err(e) => return void(format!("audit stage {first}..{last}: {e}")),
            }
        }
        let Some((port, token)) = prev else { return void("no audit stage".into()) };
        match prompt_rows(port, &token, tokens).await {
            Ok((rows, width)) => check_chunks(&rows, width, &r.chunks, &r.params.unwrap_or_default()),
            Err(e) => void(format!("reading the audited stage: {e}")),
        }
    }

    async fn stop(&self) {
        let ids = std::mem::take(&mut *self.ids.lock().unwrap_or_else(|p| p.into_inner()));
        for id in ids {
            let _ = self.query("engine.audit.stop", json!({ "id": id })).await;
        }
    }
}

/// A loopback relay between a later audit stage and the earlier one at
/// `prev_port`: requests pass as they are, tensor answers cross the wire's
/// codec in `dtype`. Returns the port the later stage connects to.
async fn codec_relay(prev_port: u16, dtype: crate::wire::WireDtype) -> Result<u16, String> {
    use crate::plugin_stage_link::{from_network, read_frame, to_network, write_frame};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    tokio::spawn(async move {
        let Ok((mut later, _)) = listener.accept().await else { return };
        let Ok(mut earlier) = tokio::net::TcpStream::connect(("127.0.0.1", prev_port)).await else { return };
        loop {
            let Ok(Some(request)) = read_frame(&mut later).await else { break };
            if write_frame(&mut earlier, &request).await.is_err() {
                break;
            }
            let Ok(Some(answer)) = read_frame(&mut earlier).await else { break };
            let answer = match to_network(answer.clone(), dtype).and_then(|net| from_network(&net)) {
                Ok(decoded) => decoded,
                Err(_) => answer,
            };
            if write_frame(&mut later, &answer).await.is_err() {
                break;
            }
        }
    });
    Ok(port)
}

/// The audited stage's rows for the whole sequence: hello with its secret,
/// a new session, then prompt pieces. Returns the rows and their width.
async fn prompt_rows(port: u16, token: &str, tokens: &[i32]) -> Result<(Vec<f32>, usize), String> {
    use crate::plugin_stage_link::{read_frame, write_frame};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
    let mut call = |payload: Vec<u8>| {
        let mut frame = vec![1u8];
        frame.extend(payload);
        frame
    };
    let mut hello = vec![b'H'];
    hello.extend(token.as_bytes());
    write_frame(&mut stream, &call(hello)).await.map_err(|e| e.to_string())?;
    let answer = read_frame(&mut stream).await.map_err(|e| e.to_string())?.ok_or("no hello")?;
    // kind 1, status 0, then u32 lo, hi, window_floats, prompt_floats
    if answer.len() < 18 || answer[1] != 0 {
        return Err("the audited stage refused the hello".into());
    }
    let width = u32::from_le_bytes([answer[14], answer[15], answer[16], answer[17]]) as usize;
    write_frame(&mut stream, &call(vec![b'Z'])).await.map_err(|e| e.to_string())?;
    read_frame(&mut stream).await.map_err(|e| e.to_string())?.ok_or("no answer to the reset")?;
    let mut rows = Vec::with_capacity(tokens.len() * width);
    for (index, piece) in tokens.chunks(AUDIT_PIECE).enumerate() {
        let mut p = vec![b'P'];
        p.extend(((index * AUDIT_PIECE) as i64).to_le_bytes());
        p.extend((piece.len() as u32).to_le_bytes());
        for t in piece {
            p.extend((*t as i64).to_le_bytes());
        }
        p.push(0);
        write_frame(&mut stream, &call(p)).await.map_err(|e| e.to_string())?;
        let answer = read_frame(&mut stream).await.map_err(|e| e.to_string())?.ok_or("no rows")?;
        let values = crate::plugin_metering::answer_rows(&answer).ok_or("an answer without rows")?;
        if values.len() != piece.len() * width {
            return Err("an answer of the wrong size".into());
        }
        rows.extend(values);
    }
    Ok((rows, width))
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

    /// A fake audited stage: answers the hello, the reset and each prompt
    /// piece with rows that are the token id repeated `width` times.
    async fn fake_stage(width: u32) -> u16 {
        use crate::plugin_stage_link::{read_frame, write_frame};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            while let Ok(Some(frame)) = read_frame(&mut s).await {
                let answer = match frame[1] {
                    b'H' => {
                        let mut a = vec![1u8, 0];
                        for v in [0u32, 10, width, width, 1] {
                            a.extend(v.to_le_bytes());
                        }
                        a
                    }
                    b'P' => {
                        let n = u32::from_le_bytes(frame[10..14].try_into().unwrap()) as usize;
                        let mut a = vec![2u8];
                        a.extend(1u32.to_le_bytes());
                        a.push(0);
                        for i in 0..n {
                            let t = i64::from_le_bytes(frame[14 + 8 * i..22 + 8 * i].try_into().unwrap()) as f32;
                            for _ in 0..width {
                                a.extend(t.to_le_bytes());
                            }
                        }
                        a
                    }
                    _ => vec![1u8, 0],
                };
                write_frame(&mut s, &answer).await.unwrap();
            }
        });
        port
    }

    #[tokio::test]
    async fn the_audited_stage_is_read_as_prompt_pieces_of_the_whole_sequence() {
        let port = fake_stage(4).await;
        let tokens: Vec<i32> = (0..300).collect();
        let (rows, width) = prompt_rows(port, "secret", &tokens).await.unwrap();
        assert_eq!(width, 4);
        assert_eq!(rows.len(), 300 * 4);
        assert_eq!(rows[299 * 4], 299.0);
    }

    #[tokio::test]
    async fn the_codec_relay_hands_the_later_stage_what_the_wire_delivers() {
        let port = fake_stage(64).await;
        let relay = codec_relay(port, crate::wire::WireDtype::F8).await.unwrap();
        let tokens: Vec<i32> = vec![3, 301, 7];
        let (through, _) = prompt_rows(relay, "secret", &tokens).await.unwrap();
        // f8 cannot hold 301 exactly; the relay answers what the wire delivers.
        let direct: Vec<f32> = tokens.iter().flat_map(|t| std::iter::repeat(*t as f32).take(64)).collect();
        let expected = roundtrip(&direct, 64 * 3, "f8").unwrap();
        assert_eq!(through, expected);
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
