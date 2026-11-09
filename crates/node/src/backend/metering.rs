//! GPU work metering, the node's side
//! (`docs/superpowers/specs/2026-11-08-gpu-work-metering-design.md`).
//!
//! The ledger lives in the metering plugin of an admin
//! (`pom-plugin-metering`). The node:
//!
//! - keeps the **metering context** the inference server reads: this node's
//!   key and id, the network id and `NetworkSettings.metering`, refreshed
//!   from the settings every few seconds ([`spawn_refresher`]); with metering
//!   off there is no context and nothing is metered;
//! - as a hop, keeps the TOPLOC chunk proofs of its receipts
//!   (`metering_proofs`) until their cycle leaves the retention window;
//! - as a gateway, mints the bundle nonce, assembles and countersigns the
//!   bundle from the receipts the hops returned on release, keeps the token
//!   ids (`metering_tokens`) and delivers the bundle to the ledger through an
//!   outbox (`metering_outbox`), off the request path;
//! - answers the ledger's node operations (`node.tokens`, `node.proofs`,
//!   `node.roles`, `node.reexecute`), reached as plugin peer calls with code
//!   `metering` and an op starting with `node.`, from an admin only.

use crate::gateway::GatewayState;
use anyhow::Result;
use ed25519_dalek::SigningKey;
use metering_core::receipt::{self, BundleClaims, Consumer, StepRef};
use metering_core::settings::MeteringSettings;
use metering_core::toploc;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tracing::{debug, warn};

pub(crate) const PLUGIN_CODE: &str = "metering";
const REFRESH_EVERY: Duration = Duration::from_secs(5);
const DELIVER_EVERY: Duration = Duration::from_secs(10);
const PEER_TIMEOUT: Duration = Duration::from_secs(5);
/// Proofs answered per `node.proofs` call.
const MAX_PROOF_CHUNKS: usize = 64;

pub(crate) struct MeterContext {
    pub(crate) key: Arc<SigningKey>,
    pub(crate) node_id: String,
    pub(crate) net: String,
    pub(crate) settings: MeteringSettings,
    pub(crate) toploc: toploc::Params,
}

static CONTEXT: RwLock<Option<Arc<MeterContext>>> = RwLock::new(None);
static DATABASE: RwLock<Option<Arc<crate::db::PomDb>>> = RwLock::new(None);

/// The context, only while metering is enabled for this network.
pub(crate) fn context() -> Option<Arc<MeterContext>> {
    CONTEXT.read().ok().and_then(|c| c.clone())
}

pub(crate) fn set_context(ctx: Option<MeterContext>) {
    if let Ok(mut slot) = CONTEXT.write() {
        *slot = ctx.map(Arc::new);
    }
}

fn database() -> Option<Arc<crate::db::PomDb>> {
    DATABASE.read().ok().and_then(|d| d.clone())
}

async fn current_settings(state: &GatewayState) -> (MeteringSettings, String) {
    let configured = state.auth.read().await.network.clone();
    let network = state
        .database
        .network_settings()
        .ok()
        .flatten()
        .unwrap_or(configured);
    let net = state.admin_trust.document().anchor.cluster_id.clone();
    (network.metering.normalized(), net)
}

/// Reads the settings once and installs (or clears) the context.
pub(crate) async fn refresh(state: &GatewayState) {
    if let Ok(mut db) = DATABASE.write() {
        if db.is_none() {
            *db = Some(state.database.clone());
        }
    }
    let (settings, net) = current_settings(state).await;
    if !settings.enabled || net.is_empty() {
        set_context(None);
        return;
    }
    set_context(Some(MeterContext {
        key: state.authority_signing_key.clone(),
        node_id: state.network.node_id.clone(),
        net,
        settings,
        toploc: toploc::Params::default(),
    }));
}

/// Keeps the context current, delivers the outbox and drops data that left
/// the retention window.
pub(crate) fn spawn_refresher(state: GatewayState) {
    tokio::spawn(async move {
        let mut last_deliver = std::time::Instant::now() - DELIVER_EVERY;
        let mut last_prune_cycle = None;
        loop {
            refresh(&state).await;
            if let Some(ctx) = context() {
                let cycle = metering_core::cycle::cycle_of(now_ms(), ctx.settings.cycle_secs);
                if last_prune_cycle != Some(cycle) {
                    let oldest = metering_core::cycle::oldest_kept(cycle, ctx.settings.retain_cycles);
                    if let Err(error) = prune(&state.database, oldest) {
                        warn!("metering: pruning old cycles failed: {error}");
                    }
                    last_prune_cycle = Some(cycle);
                }
            }
            if last_deliver.elapsed() >= DELIVER_EVERY {
                deliver_outbox(&state).await;
                last_deliver = std::time::Instant::now();
            }
            tokio::time::sleep(REFRESH_EVERY).await;
        }
    });
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Store ────────────────────────────────────────────────────────────────────

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS metering_proofs (
        rid TEXT NOT NULL,
        chunk INTEGER NOT NULL,
        cycle INTEGER NOT NULL,
        proof BLOB NOT NULL,
        PRIMARY KEY(rid, chunk)
    );
    CREATE INDEX IF NOT EXISTS metering_proofs_cycle_idx ON metering_proofs(cycle);
    CREATE TABLE IF NOT EXISTS metering_tokens (
        bid TEXT PRIMARY KEY,
        cycle INTEGER NOT NULL,
        prompt BLOB NOT NULL,
        output BLOB NOT NULL
    );
    CREATE INDEX IF NOT EXISTS metering_tokens_cycle_idx ON metering_tokens(cycle);
    CREATE TABLE IF NOT EXISTS metering_outbox (
        bid TEXT PRIMARY KEY,
        cycle INTEGER NOT NULL,
        token TEXT NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0
    );
";

fn tokens_blob(tokens: &[i32]) -> Vec<u8> {
    tokens.iter().flat_map(|t| t.to_le_bytes()).collect()
}

fn blob_tokens(blob: &[u8]) -> Vec<i32> {
    blob.chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub(crate) fn prune(db: &crate::db::PomDb, oldest_kept: u64) -> Result<()> {
    db.with_write("metering prune", |c| {
        for table in ["metering_proofs", "metering_tokens", "metering_outbox"] {
            c.execute(&format!("DELETE FROM {table} WHERE cycle < ?1"), params![oldest_kept as i64])?;
        }
        Ok(())
    })
}

/// Stores a receipt's chunk proofs. Called from the inference server when it
/// signs the receipt.
pub(crate) fn proofs_commit(rid: &str, cycle: u64, chunks: Vec<Vec<u8>>) {
    if chunks.is_empty() {
        return;
    }
    let Some(db) = database() else { return };
    let rid = rid.to_string();
    // Off the inference lock: the caller holds it.
    std::thread::spawn(move || {
        let result = db.with_write("metering proofs", |c| {
            let tx = c.unchecked_transaction()?;
            for (i, proof) in chunks.iter().enumerate() {
                tx.execute(
                    "INSERT OR REPLACE INTO metering_proofs(rid, chunk, cycle, proof) VALUES (?1, ?2, ?3, ?4)",
                    params![rid, i as i64, cycle as i64, proof],
                )?;
            }
            tx.commit()?;
            Ok(())
        });
        if let Err(error) = result {
            warn!("metering: storing proofs failed: {error}");
        }
    });
}

pub(crate) fn proofs_read(db: &crate::db::PomDb, rid: &str) -> Result<Vec<Vec<u8>>> {
    db.with_read("metering proofs read", |c| {
        let mut stmt = c.prepare("SELECT proof FROM metering_proofs WHERE rid = ?1 ORDER BY chunk")?;
        let rows = stmt
            .query_map(params![rid], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub(crate) fn tokens_store(db: &crate::db::PomDb, bid: &str, cycle: u64, prompt: &[i32], output: &[i32]) -> Result<()> {
    db.with_write("metering tokens", |c| {
        c.execute(
            "INSERT OR REPLACE INTO metering_tokens(bid, cycle, prompt, output) VALUES (?1, ?2, ?3, ?4)",
            params![bid, cycle as i64, tokens_blob(prompt), tokens_blob(output)],
        )?;
        Ok(())
    })
}

pub(crate) fn tokens_read(db: &crate::db::PomDb, bid: &str) -> Result<Option<(Vec<i32>, Vec<i32>)>> {
    db.with_read("metering tokens read", |c| {
        let mut stmt = c.prepare("SELECT prompt, output FROM metering_tokens WHERE bid = ?1")?;
        let mut rows = stmt.query(params![bid])?;
        Ok(match rows.next()? {
            Some(row) => Some((blob_tokens(&row.get::<_, Vec<u8>>(0)?), blob_tokens(&row.get::<_, Vec<u8>>(1)?))),
            None => None,
        })
    })
}

fn outbox_put(db: &crate::db::PomDb, bid: &str, cycle: u64, token: &str) -> Result<()> {
    db.with_write("metering outbox", |c| {
        c.execute(
            "INSERT OR IGNORE INTO metering_outbox(bid, cycle, token) VALUES (?1, ?2, ?3)",
            params![bid, cycle as i64, token],
        )?;
        Ok(())
    })
}

fn outbox_pending(db: &crate::db::PomDb, limit: usize) -> Result<Vec<(String, String)>> {
    db.with_read("metering outbox read", |c| {
        let mut stmt = c.prepare("SELECT bid, token FROM metering_outbox ORDER BY attempts, cycle LIMIT ?1")?;
        let rows = stmt
            .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

fn outbox_done(db: &crate::db::PomDb, bid: &str, delivered: bool) -> Result<()> {
    db.with_write("metering outbox update", |c| {
        if delivered {
            c.execute("DELETE FROM metering_outbox WHERE bid = ?1", params![bid])?;
        } else {
            c.execute("UPDATE metering_outbox SET attempts = attempts + 1 WHERE bid = ?1", params![bid])?;
        }
        Ok(())
    })
}

// ── What a hop forwards to the next hop ──────────────────────────────────────

/// Link digests of the frames this node sent onward, by session. In ring,
/// direct and link modes a hop does not hand its reply to the next hop: it
/// encodes a new frame for it (`relay.rs`), maybe in another format. The
/// output link must hash what the next hop decodes, so the relay encoders
/// report each forwarded tensor here and the meter takes it at close.
type Forwarded = (metering_core::chain::LinkChain, String, std::time::Instant);
static FORWARDED: Mutex<Option<HashMap<String, Forwarded>>> = Mutex::new(None);
const MAX_FORWARDED: usize = 4096;

/// A frame for `session_id` is about to leave for the next hop.
pub(crate) fn note_forward(session_id: Option<&str>, tensor: Option<(&[f32], &[usize])>, dtype: crate::wire::WireDtype) {
    let (Some(sid), Some((values, shape))) = (session_id, tensor) else { return };
    if context().is_none() {
        return;
    }
    let rows = shape.first().copied().unwrap_or(0) as u32;
    let width = shape.get(1).copied().unwrap_or(0);
    let decoded = match crate::metering_verify::roundtrip(values, width, dtype.as_str()) {
        Ok(d) => d,
        Err(_) => return,
    };
    let bytes: Vec<u8> = decoded.iter().flat_map(|v| v.to_le_bytes()).collect();
    let Ok(mut map) = FORWARDED.lock() else { return };
    let map = map.get_or_insert_with(HashMap::new);
    if map.len() >= MAX_FORWARDED && !map.contains_key(sid) {
        if let Some(oldest) = map.iter().min_by_key(|(_, (_, _, at))| *at).map(|(k, _)| k.clone()) {
            map.remove(&oldest);
        }
    }
    let entry = map
        .entry(sid.to_string())
        .or_insert_with(|| (Default::default(), String::new(), std::time::Instant::now()));
    entry.0.absorb(0..rows, &bytes);
    entry.1 = dtype.as_str().to_string();
}

/// The forwarded link digest and format of `sid`, taken.
pub(crate) fn take_forwarded(sid: &str) -> Option<(String, String)> {
    FORWARDED
        .lock()
        .ok()?
        .as_mut()?
        .remove(sid)
        .map(|(chain, dtype, _)| (chain.digest(), dtype))
}

// ── Model digest ─────────────────────────────────────────────────────────────

/// A cheap fingerprint of the model file: SHA-256 of its size and first
/// MiB. Hashing a whole GGUF on every load would cost minutes; the header and
/// the first tensors already tell models and quantizations apart. Cached per
/// path.
pub(crate) fn model_digest(path: &str) -> String {
    static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
    if let Ok(cache) = CACHE.lock() {
        if let Some(d) = cache.as_ref().and_then(|c| c.get(path)) {
            return d.clone();
        }
    }
    let digest = (|| -> std::io::Result<String> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut buf = vec![0u8; 1 << 20];
        let mut read = 0;
        while read < buf.len() {
            let n = file.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        let mut h = Sha256::new();
        h.update(len.to_le_bytes());
        h.update(&buf[..read]);
        Ok(hex::encode(h.finalize()))
    })()
    .unwrap_or_default();
    if let Ok(mut cache) = CACHE.lock() {
        cache.get_or_insert_with(HashMap::new).insert(path.to_string(), digest.clone());
    }
    digest
}

// ── TOPLOC proofs of a session ───────────────────────────────────────────────

/// Accumulates a session's provable output rows (with their positions) and
/// proves each full chunk. Each stored chunk is a TOPLOC leaf
/// (`toploc::encode_leaf`): its positions and its proof.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProofBuffer {
    pending: Vec<f32>,
    positions: Vec<u32>,
    width: usize,
    chunks: Vec<Vec<u8>>,
    /// The width changed mid-session (never expected): stop proving.
    broken: bool,
}

impl ProofBuffer {
    pub(crate) fn push_rows(&mut self, values: &[f32], width: usize, first_pos: u32) {
        if self.broken || width == 0 || values.len() % width != 0 {
            return;
        }
        if self.width == 0 {
            self.width = width;
        } else if self.width != width {
            self.broken = true;
            return;
        }
        let Some(ctx) = context() else { return };
        let rows = values.len() / width;
        let chunk_rows = ctx.toploc.chunk.max(1) as usize;
        self.pending.extend_from_slice(values);
        self.positions.extend((0..rows as u32).map(|i| first_pos + i));
        while self.positions.len() >= chunk_rows {
            let rest = self.pending.split_off(chunk_rows * width);
            let chunk = std::mem::replace(&mut self.pending, rest);
            let rest_pos = self.positions.split_off(chunk_rows);
            let pos = std::mem::replace(&mut self.positions, rest_pos);
            if let Ok(proof) = toploc::prove_chunk(&chunk, ctx.toploc.k) {
                self.chunks.push(toploc::encode_leaf(&pos, &proof));
            }
        }
    }

    /// Proves the short last chunk when it holds at least k values, and
    /// returns the Merkle root and the chunk count.
    pub(crate) fn finish(&mut self, ctx: &MeterContext) -> (Option<String>, Option<u32>) {
        if !self.broken && self.pending.len() >= ctx.toploc.k as usize {
            if let Ok(proof) = toploc::prove_chunk(&self.pending, ctx.toploc.k) {
                self.chunks.push(toploc::encode_leaf(&self.positions, &proof));
            }
        }
        self.pending.clear();
        self.positions.clear();
        if self.chunks.is_empty() {
            return (None, None);
        }
        let leaves: Vec<&[u8]> = self.chunks.iter().map(|c| c.as_slice()).collect();
        (Some(metering_core::merkle::root(&leaves)), Some(self.chunks.len() as u32))
    }

    pub(crate) fn take_chunks(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.chunks)
    }
}

// ── Gateway: bundle ──────────────────────────────────────────────────────────

/// The bundle nonce of a lifecycle body (`metering_bundle`), when well formed.
pub(crate) fn nonce_of(body: &Value) -> Option<String> {
    body.get("metering_bundle")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .map(str::to_string)
}

tokio::task_local! {
    /// Who a generation runs for, set by the logged generation from the
    /// request's credential (`gateway::spawn_logged_generation_with_stops`).
    pub(crate) static CONSUMER: Consumer;
}

/// The consumer of the generation running on this task, if one was set.
pub(crate) fn current_consumer() -> Consumer {
    CONSUMER.try_with(|c| c.clone()).unwrap_or_default()
}

/// The consumer a request's credential names (spec 4). On a visitor
/// gateway, the visitor itself is the consumer.
pub(crate) fn consumer_of(caller: &crate::reqlog::Caller, visitor_self: Option<&str>) -> Consumer {
    Consumer {
        api_key_id: (!caller.key_id.is_empty()).then(|| caller.key_id.clone()),
        visitor_node: visitor_self.map(str::to_string),
        visitor_network: None,
    }
}

/// What the gateway remembers of a metered request until the release.
#[derive(Clone, Debug)]
pub(crate) struct BundleTicket {
    pub(crate) bnd: String,
    pub(crate) consumer: Consumer,
    pub(crate) gw_visitor: bool,
}

/// A fresh bundle nonce when this request should be metered: metering on and
/// the gateway allowed to countersign (spec 8). A visitor plans for itself
/// only in `direct` mode, and then countersigns as the consumer.
pub(crate) fn mint_ticket(consumer: Consumer, gw_visitor: bool) -> Option<BundleTicket> {
    context()?;
    let mut raw = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
    Some(BundleTicket { bnd: hex::encode(raw), consumer, gw_visitor })
}

/// Assembles, countersigns and queues the bundle of a finished request.
/// Never fails the request: problems are logged.
pub(crate) fn finish_bundle(
    database: &crate::db::PomDb,
    ticket: &BundleTicket,
    plan: &[StepRef],
    answers: &[Option<String>],
    prompt: &[i32],
    output: &[i32],
) {
    let Some(ctx) = context() else { return };
    let hops = receipt::assemble_hops(plan, &ticket.bnd, answers, prompt);
    let gw_key = hex::encode(ctx.key.verifying_key().as_bytes());
    let t = now_ms();
    let cyc = metering_core::cycle::cycle_of(t, ctx.settings.cycle_secs);
    let claims = BundleClaims {
        bid: uuid::Uuid::new_v4().simple().to_string(),
        net: ctx.net.clone(),
        cyc,
        bnd: ticket.bnd.clone(),
        gw: ctx.node_id.clone(),
        gw_key,
        gw_visitor: ticket.gw_visitor,
        plan: plan.to_vec(),
        hops,
        tokens_digest: receipt::tokens_digest(prompt, output),
        prompt_tokens: prompt.len() as u32,
        output_tokens: output.len() as u32,
        consumer: ticket.consumer.clone(),
        t,
    };
    let token = match receipt::sign_bundle(&claims, &ctx.key) {
        Ok(token) => token,
        Err(error) => {
            warn!("metering: countersigning the bundle failed: {error}");
            return;
        }
    };
    if let Err(error) = tokens_store(database, &claims.bid, cyc, prompt, output)
        .and_then(|_| outbox_put(database, &claims.bid, cyc, &token))
    {
        warn!("metering: queuing the bundle failed: {error}");
    }
}

async fn deliver_outbox(state: &GatewayState) {
    let pending = match outbox_pending(&state.database, 32) {
        Ok(p) => p,
        Err(error) => {
            warn!("metering: reading the outbox failed: {error}");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }
    for (bid, token) in pending {
        let delivered = submit(state, &token).await;
        if let Err(error) = outbox_done(&state.database, &bid, delivered) {
            warn!("metering: updating the outbox failed: {error}");
        }
    }
}

/// Delivers one bundle: this node's own plugin first, then each live admin
/// that runs it. `true` once a ledger accepted it (or refused it for good).
async fn submit(state: &GatewayState, token: &str) -> bool {
    let params = json!({ "bundle": token });
    if state.plugins.status(PLUGIN_CODE).await.is_some_and(|s| s.active) {
        let caller = json!({
            "node_id": state.network.node_id,
            "local": true,
            "role": crate::plugin_services::local_role(state).await,
        });
        if let Some(reply) = state
            .plugins
            .query_audit(PLUGIN_CODE, "peer.metering.submit", json!({ "params": params, "caller": caller }))
            .await
        {
            return accepted(&reply);
        }
    }
    let document = state.admin_trust.document();
    for peer in state.network.alive_peers().await {
        if peer.node_id == state.network.node_id {
            continue;
        }
        if !crate::admin_trust::is_admin_node(&document, &peer.node_id) {
            continue;
        }
        let path = format!("/api/internal/plugins/{PLUGIN_CODE}/peer/metering.submit");
        match state.control_post_json_routed(&peer, &path, &params, PEER_TIMEOUT).await {
            Ok(response) if response.status().is_success() => {
                if let Ok(reply) = response.json::<Value>().await {
                    return accepted(&reply);
                }
            }
            Ok(response) => debug!("metering: {} answered {}", peer.node_id, response.status()),
            Err(error) => debug!("metering: {} unreachable: {error}", peer.node_id),
        }
    }
    false
}

/// A ledger's answer: accepted, a duplicate, or a refusal it will never
/// change its mind about all end the delivery.
fn accepted(reply: &Value) -> bool {
    reply.get("accepted").and_then(Value::as_bool).unwrap_or(false)
        || reply.get("duplicate").and_then(Value::as_bool).unwrap_or(false)
        || reply.get("final").and_then(Value::as_bool).unwrap_or(false)
}

// ── Node operations for the ledger ───────────────────────────────────────────

/// Whether `op` is a node operation of the metering ledger (answered by the
/// node itself, not by a plugin).
pub(crate) fn is_node_op(code: &str, op: &str) -> bool {
    code == PLUGIN_CODE && op.starts_with("node.")
}

/// Answers a node operation. `caller_role` is the caller's role as the
/// trust chain proves it; only admins (and this node itself) get answers.
pub(crate) async fn node_op(state: &GatewayState, caller_role: &str, op: &str, params: &Value) -> Result<Value, String> {
    if !matches!(caller_role, "principal" | "admin") {
        return Err("only an admin may ask".into());
    }
    // Signing a statement and reading the ledger's settings are for the
    // ledger's own node only; a peer admin never gets this node's signature.
    if matches!(op, "node.sign_statement" | "node.settings") && params["local"] != true {
        return Err("only this node's own plugin may ask".into());
    }
    match op {
        "node.tokens" => {
            let bid = params["bid"].as_str().ok_or("bid is required")?;
            match tokens_read(&state.database, bid).map_err(|e| e.to_string())? {
                Some((prompt, output)) => {
                    use base64::Engine;
                    let b64 = |t: &[i32]| base64::engine::general_purpose::STANDARD.encode(tokens_blob(t));
                    Ok(json!({ "prompt": b64(&prompt), "output": b64(&output) }))
                }
                None => Ok(json!({ "missing": true })),
            }
        }
        "node.proofs" => {
            let rid = params["rid"].as_str().ok_or("rid is required")?;
            let wanted: Vec<usize> = params["chunks"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as usize)).take(MAX_PROOF_CHUNKS).collect())
                .unwrap_or_default();
            let all = proofs_read(&state.database, rid).map_err(|e| e.to_string())?;
            if all.is_empty() {
                return Ok(json!({ "missing": true }));
            }
            let leaves: Vec<&[u8]> = all.iter().map(|c| c.as_slice()).collect();
            let picked: Vec<Value> = wanted
                .iter()
                .filter(|i| **i < all.len())
                .map(|i| {
                    json!({
                        "chunk": i,
                        "proof": hex::encode(&all[*i]),
                        "path": metering_core::merkle::proof(&leaves, *i).unwrap_or_default(),
                    })
                })
                .collect();
            Ok(json!({ "chunks": all.len(), "proofs": picked }))
        }
        "node.roles" => {
            let ids: Vec<String> = params["node_ids"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).take(512).collect())
                .unwrap_or_default();
            Ok(json!({ "roles": roles_of(state, &ids).await }))
        }
        "node.reexecute" => crate::metering_verify::reexecute(state, params).await,
        "node.reexecute_status" => crate::metering_verify::reexecute_status(params),
        "node.sign_statement" => {
            // The ledger's own node signs a cycle statement for it. Only the
            // statement header: the token cannot pass for any other kind.
            let mut claims: metering_core::receipt::StatementClaims =
                serde_json::from_value(params["claims"].clone()).map_err(|e| e.to_string())?;
            claims.issuer = state.network.node_id.clone();
            claims.issuer_key = hex::encode(state.authority_signing_key.verifying_key().as_bytes());
            claims.net = state.admin_trust.document().anchor.cluster_id.clone();
            let token = metering_core::token::sign(
                &metering_core::receipt::statement_header(),
                &claims,
                &state.authority_signing_key,
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({ "statement": token }))
        }
        "node.settings" => {
            let (settings, net) = current_settings(state).await;
            Ok(json!({ "settings": settings, "net": net, "node_id": state.network.node_id }))
        }
        _ => Err("unknown operation".into()),
    }
}

/// Each node's role from this node's view: `admin` (named by the trust
/// chain), `worker`, `visitor`, or `unknown` (not a live member).
async fn roles_of(state: &GatewayState, ids: &[String]) -> HashMap<String, &'static str> {
    let document = state.admin_trust.document();
    let peers = state.network.alive_peers().await;
    ids.iter()
        .map(|id| {
            let role = if crate::admin_trust::is_admin_node(&document, id) {
                "admin"
            } else if let Some(peer) = peers.iter().find(|p| p.node_id == *id) {
                if crate::gateway::peer_is_visitor(state, peer) {
                    "visitor"
                } else if crate::membership::peer_role(peer, &state.admin_trust).is_some() {
                    "worker"
                } else {
                    "unknown"
                }
            } else if *id == state.network.node_id {
                "worker"
            } else {
                "unknown"
            };
            (id.clone(), role)
        })
        .collect()
}

/// The metering context is process-global: tests that set it hold this.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Runs `f` with metering on for a test network, then turns it off.
#[cfg(test)]
pub(crate) fn with_test_context<T>(f: impl FnOnce() -> T) -> T {
    let _guard = test_guard();
    let key = SigningKey::from_bytes(&[9u8; 32]);
    let node_id = metering_core::node_id_of_pubkey_hex(&hex::encode(key.verifying_key().as_bytes())).unwrap();
    set_context(Some(MeterContext {
        key: Arc::new(key),
        node_id,
        net: "net-1".into(),
        settings: MeteringSettings { enabled: true, ..Default::default() },
        toploc: toploc::Params::default(),
    }));
    let out = f();
    set_context(None);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, crate::db::PomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::PomDb::open(dir.path().join("pom.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn token_log_round_trips_and_drops_cycles_outside_the_window() {
        let (_d, db) = temp_db();
        tokens_store(&db, "b1", 3, &[1, 2, -3], &[4]).unwrap();
        tokens_store(&db, "b2", 5, &[7], &[8, 9]).unwrap();
        assert_eq!(tokens_read(&db, "b1").unwrap(), Some((vec![1, 2, -3], vec![4])));
        prune(&db, 4).unwrap();
        assert_eq!(tokens_read(&db, "b1").unwrap(), None);
        assert!(tokens_read(&db, "b2").unwrap().is_some());
    }

    #[test]
    fn the_outbox_retries_until_delivered() {
        let (_d, db) = temp_db();
        outbox_put(&db, "b1", 1, "tok").unwrap();
        outbox_put(&db, "b1", 1, "tok").unwrap();
        assert_eq!(outbox_pending(&db, 10).unwrap().len(), 1);
        outbox_done(&db, "b1", false).unwrap();
        assert_eq!(outbox_pending(&db, 10).unwrap().len(), 1);
        outbox_done(&db, "b1", true).unwrap();
        assert!(outbox_pending(&db, 10).unwrap().is_empty());
    }

    #[test]
    fn a_ledger_answer_ends_delivery_only_when_settled() {
        assert!(accepted(&json!({"accepted": true})));
        assert!(accepted(&json!({"duplicate": true})));
        assert!(accepted(&json!({"final": true, "error": "bad signature"})));
        assert!(!accepted(&json!({"error": "busy"})));
    }

    #[test]
    fn node_ops_are_only_the_metering_codes_node_namespace() {
        assert!(is_node_op("metering", "node.tokens"));
        assert!(!is_node_op("metering", "metering.submit"));
        assert!(!is_node_op("logs", "node.tokens"));
    }

    #[test]
    fn proof_buffer_proves_full_chunks_and_a_long_enough_tail() {
        with_test_context(|| {
        let ctx = context().unwrap();
        let width = 64;
        let mut buf = ProofBuffer::default();
        let rows: Vec<f32> = (0..(40 * width)).map(|i| ((i * 7919) % 1000) as f32 - 500.0).collect();
        buf.push_rows(&rows[..30 * width], width, 0);
        buf.push_rows(&rows[30 * width..], width, 100);
        let (root, chunks) = buf.finish(&ctx);
        // 32 rows prove one chunk; the 8-row tail has 512 >= 128 values.
        assert_eq!(chunks, Some(2));
        assert!(root.is_some());
        let leaves = buf.take_chunks();
        let (positions, _) = toploc::decode_leaf(&leaves[0]).unwrap();
        assert_eq!(positions.len(), 32);
        assert_eq!(&positions[29..], &[29, 100, 101]);
        });
    }

    #[test]
    fn model_digest_changes_with_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.gguf");
        let b = dir.path().join("b.gguf");
        std::fs::write(&a, b"GGUF-a").unwrap();
        std::fs::write(&b, b"GGUF-b").unwrap();
        let (da, db) = (model_digest(a.to_str().unwrap()), model_digest(b.to_str().unwrap()));
        assert_eq!(da.len(), 64);
        assert_ne!(da, db);
        assert_eq!(model_digest("/nonexistent/x.gguf"), "");
    }
}
