//! Registration of the tensor parallelism callback in llama.cpp (C path).
//!
//! The llama.cpp fork inserts a reduce op (GGML_OP_CUSTOM) after the
//! row-parallel projections (wo and ffn_down). The op calls llama_tp_reduce_fun,
//! which copies the partial and invokes this callback to sum the group's partials.
//!
//! The callback runs on a blocking thread during llama_decode. The workers
//! POST their own partial to the primary (rank 0), whose `/v1/tp/reduce`
//! endpoint accumulates via `ReduceBarrier`. The primary itself contributes
//! directly to the local barrier, without dialing its own public URL. The
//! partial is f32 (the precision of the GEMM).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::collective::ReduceBarrier;
use crate::relay::ByteTransport;

/// Headers of the collective. The body is the partial in raw f32; fixed-size
/// metadata goes in a header, not mixed into the vector.
pub const TP_BARRIER_HEADER: &str = "x-pom-tp-barrier";
pub const TP_RANK_HEADER: &str = "x-pom-tp-rank";

struct TpState {
    rank: u32,
    size: u32,
    /// URL of the collective's primary (rank 0). The workers POST here; rank
    /// 0 uses the local barrier and does not dial its own URL.
    rank0_url: String,
    /// Alternative candidates for the primary, chosen by whoever deployed.
    /// The first is `rank0_url`; the others cover NAT, LAN and containers on
    /// the same host when only one of them is reachable from this rank.
    rank0_urls: Vec<String>,
    /// UDP addresses announced by the primary, so the tunnel can rebuild the
    /// TCP endpoint addresses without depending on this rank's discovery table.
    rank0_addr_candidates: Vec<String>,
    /// endpoint id and tunnel capability of the primary, for networks with no
    /// direct HTTPS route between the machines.
    rank0_peer_id: Option<String>,
    rank0_supports_tunnel: bool,
    /// Client **pinned to the primary's `node_id`**, built once at registration.
    ///
    /// The collective used `reqwest::Client::new()`, which validates the
    /// certificate through the public chain; the cluster's is self-signed and
    /// verified by `SHA256(pubkey) == node_id`. With TLS on, every POST died in
    /// 13 ms (measured on ksia, 18/08) and the all-reduce never happened.
    client: reqwest::Client,
    /// Live configuration used to obtain the internal credential. The receiver
    /// also accepts active API keys, but the nodes use the connection_key.
    auth: Option<crate::auth::AuthState>,
    /// Runtime that hosts the inference server. The llama.cpp callback runs
    /// on a `spawn_blocking` thread, so `Handle::current()` does not exist
    /// there; the handle has to be captured when the TP group is registered.
    runtime: Option<tokio::runtime::Handle>,
    /// Reduce counter. In lockstep execution, the k-th reduce of all the
    /// ranks corresponds to the same projection, so the id matches across processes.
    counter: AtomicU64,
    /// Local barrier belonging to the server that registered this TP state.
    /// Keeping it in the state, and not in a single process global, prevents a
    /// reconfiguration from swapping the barrier of an old decode still draining.
    local_barrier: std::sync::RwLock<Option<(ReduceBarrier, u32, u64)>>,
}

/// State of this process's collective. `None` = this node is not in a TP group.
///
/// It used to be a `OnceLock`, which **cannot be cleared**: a process that
/// once served a TP group kept declaring itself TP forever. Deploying a whole
/// model on top of a group inherited that state, and the "no reduce in this
/// decode" check went as far as failing legitimate requests of a model that
/// never had any collective at all (18/08). Swapping the group also required
/// restarting the process, which was a limitation of the type, not the design.
static TP_STATE: std::sync::RwLock<Option<Arc<TpState>>> = std::sync::RwLock::new(None);

static NEXT_LOCAL_BARRIER_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(crate) struct LocalBarrierHandle {
    state: Arc<TpState>,
    token: u64,
}

pub(crate) fn set_local_barrier(barrier: ReduceBarrier, size: u32) -> Option<LocalBarrierHandle> {
    let state = tp_state()?;
    let token = NEXT_LOCAL_BARRIER_TOKEN.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut slot) = state.local_barrier.write() {
        *slot = Some((barrier, size, token));
    } else {
        return None;
    }
    Some(LocalBarrierHandle { state, token })
}

pub(crate) fn clear_local_barrier(handle: LocalBarrierHandle) {
    if let Ok(mut slot) = handle.state.local_barrier.write() {
        if slot
            .as_ref()
            .is_some_and(|(_, _, current)| *current == handle.token)
        {
            *slot = None;
        }
    }
}

fn local_barrier_for(state: &Arc<TpState>) -> Option<(ReduceBarrier, u32)> {
    if state.rank != 0 {
        return None;
    }
    state.local_barrier.read().ok().and_then(|slot| {
        slot.as_ref()
            .map(|(barrier, size, _)| (barrier.clone(), *size))
    })
}

fn tp_state() -> Option<Arc<TpState>> {
    TP_STATE.read().ok().and_then(|s| s.clone())
}

/// Last failure of the collective, for the decode to turn into a request error.
///
/// An all-reduce that fails **does not degrade the quality of the reply: it
/// produces wrong numbers**. The rank is left with its own partial - half the
/// heads and half the FFN - and the generation comes out syntactically plausible
/// and semantically garbage (`oI, TESTE` followed by `ape ilib ion perc`, 18/08).
/// That is why the failure is recorded here and fails the request, instead of
/// becoming text.
static TP_FAILURE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Consumes the collective's pending failure, if there is one.
///
/// Called by the decode: while this returns `Some`, the reply in progress is
/// contaminated and cannot be delivered.
pub fn take_failure() -> Option<String> {
    TP_FAILURE.lock().ok().and_then(|mut f| f.take())
}

/// Decode in progress: `(id, next reduce sequence)`.
///
/// The `barrier_id` **has to be the same on all ranks** for the partials to
/// meet. It used to be a per-process counter, which only matches while the
/// processes are perfectly aligned since startup: a worker that restarts
/// zeroes the counter and the group never meets again - an error that gives no
/// error, it just hangs. Now rank 0 generates the decode's id, sends it along
/// with the batch, and both sides number that decode's reduces from zero.
///
/// Global because the `llama_context` is not safe for concurrent use and already
/// lives behind a mutex: there is at most one decode per process at a time.
static DECODE: std::sync::Mutex<Option<(String, u64)>> = std::sync::Mutex::new(None);

#[derive(Debug, Clone, serde::Serialize)]
pub struct TpDecodeTelemetry {
    pub enabled: bool,
    pub rank: u32,
    pub size: u32,
    pub expected_reduces: u64,
    pub reduces: u64,
    pub wait_ms: u64,
    pub reduce_p50_ms: u64,
    pub reduce_p95_ms: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub local_calls: u64,
    pub direct_calls: u64,
    pub tunnel_calls: u64,
    pub fallback_calls: u64,
    pub broadcast_calls: u64,
    pub broadcast_bytes: u64,
    #[serde(skip)]
    samples_ms: Vec<u64>,
}

static DECODE_TELEMETRY: std::sync::Mutex<Option<TpDecodeTelemetry>> = std::sync::Mutex::new(None);

fn percentile(samples: &[u64], percent: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) * percent + 50) / 100;
    sorted[index]
}

fn telemetry_snapshot(current: &TpDecodeTelemetry) -> TpDecodeTelemetry {
    let mut snapshot = current.clone();
    snapshot.reduce_p50_ms = percentile(&snapshot.samples_ms, 50);
    snapshot.reduce_p95_ms = percentile(&snapshot.samples_ms, 95);
    snapshot.samples_ms.clear();
    snapshot
}

/// Is this process running a decode as a **replica** (rank != 0)?
///
/// A replica has no opinion: it exists to contribute its half to each
/// reduce. Whoever samples the token, decides whether the generation is over
/// and drives the session's lifecycle is rank 0 - the replica follows.
///
/// Ignoring this cost the 400 of 18/08: sampling is stochastic (temperature,
/// top_p, top_k, with an independent RNG in each process), the worker sampled EOS
/// while rank 0 sampled a normal token, considered the session finished and
/// deleted it. The next step reached it and got "unknown session".
pub fn is_replica() -> bool {
    REPLICA.load(Ordering::Relaxed)
}

static REPLICA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Opens a decode as a replica: same id as rank 0, and with no right to decide.
pub fn begin_replica_decode(id: &str) {
    REPLICA.store(true, Ordering::Relaxed);
    begin_decode_inner(id);
}

/// Opens a decode: the following reduces belong to `id` and restart from zero.
///
/// Called by the `/v1/prefill` and `/v1/step` handlers on **both** sides -
/// rank 0 with the id it drew, the worker with the id it received in the header.
pub fn begin_decode(id: &str) {
    REPLICA.store(false, Ordering::Relaxed);
    begin_decode_inner(id);
}

fn begin_decode_inner(id: &str) {
    if let Ok(mut d) = DECODE.lock() {
        *d = Some((id.to_string(), 0));
    }
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        *telemetry = tp_state().map(|state| TpDecodeTelemetry {
            enabled: true,
            rank: state.rank,
            size: state.size,
            expected_reduces: 0,
            reduces: 0,
            wait_ms: 0,
            reduce_p50_ms: 0,
            reduce_p95_ms: 0,
            request_bytes: 0,
            response_bytes: 0,
            local_calls: 0,
            direct_calls: 0,
            tunnel_calls: 0,
            fallback_calls: 0,
            broadcast_calls: 0,
            broadcast_bytes: 0,
            samples_ms: Vec::new(),
        });
    }
}

pub fn set_expected_reduces(expected: u64) {
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        if let Some(current) = telemetry.as_mut() {
            current.expected_reduces = expected;
        }
    }
}

pub fn record_broadcast_scheduled(calls: u64, bytes: u64) {
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        if let Some(current) = telemetry.as_mut() {
            current.broadcast_calls += calls;
            current.broadcast_bytes += calls * bytes;
        }
    }
}

pub fn record_reduce(
    _elements: usize,
    request_bytes: u64,
    response_bytes: u64,
    elapsed: std::time::Duration,
    transport: ByteTransport,
    fallback: bool,
) {
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        if let Some(current) = telemetry.as_mut() {
            let elapsed_ms = elapsed.as_millis() as u64;
            current.reduces += 1;
            current.wait_ms += elapsed_ms;
            current.request_bytes += request_bytes;
            current.response_bytes += response_bytes;
            current.samples_ms.push(elapsed_ms);
            match transport {
                ByteTransport::Local => current.local_calls += 1,
                ByteTransport::Direct => current.direct_calls += 1,
                ByteTransport::Tunnel => current.tunnel_calls += 1,
            }
            if fallback {
                current.fallback_calls += 1;
            }
        }
    }
}

pub fn take_decode_telemetry() -> Option<TpDecodeTelemetry> {
    DECODE_TELEMETRY.lock().ok().and_then(|mut telemetry| {
        let snapshot = telemetry.as_ref().map(telemetry_snapshot);
        *telemetry = None;
        snapshot
    })
}

/// How many reduces the decode in progress has done. Zero = the op did not enter the graph.
pub fn reduces_in_current_decode() -> u64 {
    DECODE
        .lock()
        .ok()
        .and_then(|d| d.as_ref().map(|(_, n)| *n))
        .unwrap_or(0)
}

/// Consumes the next `barrier_id` of this decode.
fn next_barrier_id() -> Option<String> {
    let mut d = DECODE.lock().ok()?;
    let (id, seq) = d.as_mut()?;
    let current = *seq;
    *seq += 1;
    Some(format!("{id}#{current}"))
}

/// Is the collective on in this process?
pub fn is_active() -> bool {
    tp_state().is_some()
}

/// Wires the bridge the `llama-engine` uses to query this module from inside
/// the decode (`llm.rs` -> `llama_engine::tp_probe`). Called once at `node`
/// startup; `tp.rs` stays in `node` because the collective talks to the primary
/// over HTTP/p2p tunnel.
pub fn install_probe() {
    llama_engine::tp_probe::install(llama_engine::tp_probe::TpProbe {
        is_active,
        reduce_count,
        reduces_in_current_decode,
        take_failure,
        set_expected_reduces,
    });
}

/// How many reduces this process has already executed.
///
/// The decode compares before and after: with the collective on, a decode that
/// fired **no** reduce ran the graph at half width and with no sum, and the
/// reply is false. That was the state of 18/08 - 30 tok/s of meaningless text -,
/// and it had no symptom other than the text.
pub fn reduce_count() -> u64 {
    tp_state()
        .map(|s| s.counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

fn note_failure(reason: String) {
    tracing::error!("TP collective failed: {reason}");
    if let Ok(mut f) = TP_FAILURE.lock() {
        // The first failure is the one that explains; the later ones follow from it.
        if f.is_none() {
            *f = Some(reason);
        }
    }
}

/// Turns on this process's TP collective and registers the callback in llama.cpp.
///
/// Called by apply when the node is primary/worker of a group (size > 1).
#[cfg(test)]
pub fn register_tp_reduce(
    rank: u32,
    size: u32,
    rank0_url: String,
    rank0_node_id: Option<String>,
    rank0_peer_id: Option<String>,
    rank0_supports_tunnel: bool,
    auth: Option<crate::auth::AuthState>,
) {
    register_tp_reduce_with_urls(
        rank,
        size,
        rank0_url.clone(),
        vec![rank0_url],
        Vec::new(),
        rank0_node_id,
        rank0_peer_id,
        rank0_supports_tunnel,
        auth,
    );
}

/// Turns on the TP collective with all the known addresses of the primary.
pub fn register_tp_reduce_with_urls(
    rank: u32,
    size: u32,
    rank0_url: String,
    mut rank0_urls: Vec<String>,
    rank0_addr_candidates: Vec<String>,
    rank0_node_id: Option<String>,
    rank0_peer_id: Option<String>,
    rank0_supports_tunnel: bool,
    auth: Option<crate::auth::AuthState>,
) {
    let runtime = tokio::runtime::Handle::try_current().ok();
    if runtime.is_none() {
        note_failure(
            "TP collective registered outside a Tokio runtime; the callback will not be able to wait for the barrier"
                .to_string(),
        );
    }
    if rank0_urls.is_empty() {
        rank0_urls.push(rank0_url.clone());
    } else if !rank0_urls.iter().any(|url| url == &rank0_url) {
        rank0_urls.insert(0, rank0_url.clone());
    }
    // Client of the collective: pinned when the cluster is on TLS, plain when
    // it is not. Failing here is better than finding out mid first decode.
    let client = match (crate::peer_client::tls_enabled(), rank0_node_id.as_deref()) {
        (true, Some(id)) => match crate::tls::client_config(id) {
            Ok(cfg) => reqwest::Client::builder()
                .use_preconfigured_tls(cfg)
                .build()
                .unwrap_or_else(|e| {
                    note_failure(format!("TLS client of the collective did not start: {e}"));
                    reqwest::Client::new()
                }),
            Err(e) => {
                note_failure(format!("certificate of the primary {id} did not pin: {e}"));
                reqwest::Client::new()
            }
        },
        (true, None) => {
            // Group built by a version that did not propagate the id: the POST
            // will be refused by the certificate, and it is better to say so now.
            note_failure(
                "cluster on TLS and the TP group did not bring the node_id of the primary: redo the deployment of the group"
                    .to_string(),
            );
            reqwest::Client::new()
        }
        (false, _) => reqwest::Client::new(),
    };
    let state = Arc::new(TpState {
        rank,
        size,
        rank0_url,
        rank0_urls,
        rank0_addr_candidates,
        rank0_peer_id,
        rank0_supports_tunnel,
        client,
        auth,
        runtime,
        counter: AtomicU64::new(0),
        local_barrier: std::sync::RwLock::new(None),
    });
    if let Ok(mut slot) = TP_STATE.write() {
        *slot = Some(state);
    }
    unsafe {
        llama_engine::llama_cpp_sys_2::llama_set_tp_reduce_callback(
            Some(tp_reduce_cb),
            std::ptr::null_mut(),
        );
    }
}

/// Turns the collective off: the callback becomes None and the fork stops inserting the op.
pub fn unregister_tp_reduce() {
    unsafe {
        llama_engine::llama_cpp_sys_2::llama_set_tp_reduce_callback(None, std::ptr::null_mut());
    }
    // Clearing the state is half of the shutdown: without this `is_active()` stays
    // true and the node behaves as a member of a group that no longer exists.
    if let Ok(mut slot) = TP_STATE.write() {
        *slot = None;
    }
    if let Ok(mut d) = DECODE.lock() {
        *d = None;
    }
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        *telemetry = None;
    }
    REPLICA.store(false, Ordering::Relaxed);
    // A failure of the old group must not fail the first request of the new
    // model, which has nothing to do with it.
    let _ = take_failure();
}

/// The reduce op's callback: reads the partial (f32), contributes to the local
/// barrier when it is the primary or POSTs to it when it is a worker, and
/// returns the sum in the same buffer.
unsafe extern "C" fn tp_reduce_cb(data: *mut c_void, n_bytes: usize, _userdata: *mut c_void) {
    if data.is_null() || n_bytes == 0 || n_bytes % 4 != 0 {
        return;
    }
    let Some(state) = tp_state() else {
        return;
    };
    let state = &state;
    let n = n_bytes / 4;
    let partial = unsafe { std::slice::from_raw_parts(data as *const f32, n).to_vec() };
    let seq = state.counter.fetch_add(1, Ordering::Relaxed);
    let Some(barrier_id) = next_barrier_id() else {
        // Reduce outside a known decode: there is no id that matches the other
        // ranks, so the sum would be by chance. Better to fail naming that.
        note_failure("reduce outside a known decode (begin_decode was not called)".into());
        return;
    };
    if seq == 0 {
        // The first invocation is the proof that the fork's op is in the graph.
        // Without this line, "the callback is not called" and "the callback fails"
        // have exactly the same symptom: wrong text, fast, with no log.
        if state.rank == 0 {
            tracing::info!(
                "TP collective active: first reduce of rank 0 ({} values) -> local barrier",
                n
            );
        } else {
            tracing::info!(
                "TP collective active: first reduce of rank {} ({} values) -> {}",
                state.rank,
                n,
                state.rank0_url
            );
        }
    }

    let Some(runtime) = state.runtime.as_ref() else {
        note_failure("TP collective without a Tokio runtime to wait for the reduce".to_string());
        return;
    };
    let sum = runtime.block_on(post_reduce(
        state,
        &barrier_id,
        partial,
        crate::infer_tunnel::tunnel_sender(),
    ));
    match sum {
        Ok(sum) if sum.len() == n => unsafe {
            std::ptr::copy_nonoverlapping(sum.as_ptr(), data as *mut f32, n);
        },
        // A sum of the wrong size is as fatal as a missing sum: copying
        // partially would leave half the vector reduced and half not.
        Ok(sum) => note_failure(format!(
            "collective sum came with {} values, expected {n}",
            sum.len()
        )),
        // The buffer still holds this rank's partial. Copying nothing and carrying
        // on was the defect: the partial passed for a sum and the reply came out wrong.
        Err(e) => note_failure(e),
    }
}

/// Header that marks a frame as a TP replica and carries the decode's id.
///
/// Present = "you are a worker running the decode that rank 0 is running
/// right now". Absent = normal frame, coming from the gateway.
pub const TP_DECODE_HEADER: &str = "x-pom-tp-decode";

/// Makes the workers run **the same decode**, with the same batch.
///
/// This is the half of tensor parallelism that was missing. The gateway only
/// talks to rank 0 (the worker is excluded from the pipeline on purpose), so
/// without this broadcast rank 0 posted the partial and waited at the barrier
/// for a contribution nobody was going to send: the decode hung. TP requires
/// all ranks to walk the same graph over the same tokens.
///
/// The forwarded body is **the original frame, byte for byte** - the worker
/// runs the same handler and produces the same sequence of reduces. Its reply
/// is discarded: the one that samples the token is rank 0, and with the output
/// head replicated both reach the same logits after the sums.
///
/// Fires in parallel and **does not wait**: the one that waits is the barrier,
/// at the right moment. Waiting here would serialize what must be simultaneous.
pub fn broadcast_decode(
    members: &[crate::network::TpMember],
    route: &str,
    decode_id: &str,
    body: Vec<u8>,
) {
    broadcast_to(members, route, decode_id, body, None)
}

fn broadcast_target(member_url: &str, route: &str) -> (String, String) {
    (
        member_url.trim_end_matches('/').to_string(),
        route.to_string(),
    )
}

/// The same, stating the `content-type` when the body is not a binary frame.
///
/// axum's `Json` extractor **requires** the header: without it the replica
/// returns 415 and the call vanishes, which is the worst kind of failure for a
/// lifecycle route - the session would be left hanging and the defect would
/// only show up on the next request, far from the cause.
pub fn broadcast_to(
    members: &[crate::network::TpMember],
    route: &str,
    decode_id: &str,
    body: Vec<u8>,
    content_type: Option<&'static str>,
) {
    let replicas = members.len().saturating_sub(1) as u64;
    record_broadcast_scheduled(replicas, body.len() as u64);
    for member in members.iter().skip(1) {
        // The base and the path must stay separate: the tunnel uses only
        // the path to forward to the worker's loopback. Concatenating the route
        // and passing `path=""` makes the tunnel reach `/`, which answers 404.
        let (url, path) = broadcast_target(&member.url, route);
        let url_candidates = if member.url_candidates.is_empty() {
            vec![url.clone()]
        } else {
            member.url_candidates.clone()
        };
        let client = match peer_client_for(&member.node_id) {
            Some(c) => c,
            None => continue,
        };
        let body = body.clone();
        let ct = content_type;
        let id = decode_id.to_string();
        let who = member.node_id.clone();
        let auth = tp_state().and_then(|state| state.auth.clone());
        let peer_id = member
            .endpoint_id
            .as_deref()
            .and_then(|id| id.parse().ok());
        let supports_tunnel = member.supports_tunnel;
        let addr_candidates = member.addr_candidates.clone();
        let tunnel = crate::infer_tunnel::tunnel_sender();
        let auth_state = auth.clone();
        tokio::spawn(async move {
            let internal_token = match auth_state {
                Some(auth) => {
                    let config = auth.read().await;
                    crate::auth::internal_node_token(&config)
                }
                None => None,
            };
            let mut headers = vec![(TP_DECODE_HEADER.to_string(), id.clone())];
            if let Some(ct) = ct {
                headers.push(("content-type".to_string(), ct.to_string()));
            }
            if let Some(token) = internal_token {
                headers.push((
                    crate::auth::INTERNAL_NODE_REQUEST_HEADER.to_string(),
                    "1".to_string(),
                ));
                headers.push((
                    reqwest::header::AUTHORIZATION.as_str().to_string(),
                    format!("Bearer {token}"),
                ));
            }
            let result = crate::relay::send_bytes_or_tunnel_with_candidates(
                &client,
                &url,
                &url_candidates,
                &path,
                body,
                headers,
                &addr_candidates,
                tunnel,
                peer_id,
                supports_tunnel,
                &who,
            )
            .await;
            if let Err(e) = result {
                note_failure(format!(
                    "could not put rank {} in decode {id}: {e}",
                    &who[..12.min(who.len())]
                ));
            }
        });
    }
}

/// Client pinned to the member, or `None` (with the failure recorded) if it cannot be built.
fn peer_client_for(node_id: &str) -> Option<reqwest::Client> {
    if !crate::peer_client::tls_enabled() {
        return Some(reqwest::Client::new());
    }
    match crate::tls::client_config(node_id) {
        Ok(cfg) => reqwest::Client::builder()
            .use_preconfigured_tls(cfg)
            .build()
            .ok()
            .or_else(|| {
                note_failure(format!("TLS client for {node_id} did not start"));
                None
            }),
        Err(e) => {
            note_failure(format!("certificate of {node_id} did not pin: {e}"));
            None
        }
    }
}

async fn post_reduce(
    state: &Arc<TpState>,
    barrier_id: &str,
    data: Vec<f32>,
    tunnel: Option<&crate::infer_tunnel::TunnelOutboundSender>,
) -> Result<Vec<f32>, String> {
    if let Some((barrier, size)) = local_barrier_for(state) {
        let expected = data.len();
        let request_bytes = (expected * std::mem::size_of::<f32>()) as u64;
        let started = std::time::Instant::now();
        let receiver = barrier
            .contribute(barrier_id, state.rank, size, data)
            .map_err(|error| {
                format!("local barrier of the primary refused the partial: {error}")
            })?;
        let sum = receiver
            .await
            .map_err(|_| "local barrier of the primary was closed".to_string())?;
        record_reduce(
            expected,
            request_bytes,
            (sum.len() * std::mem::size_of::<f32>()) as u64,
            started.elapsed(),
            ByteTransport::Local,
            false,
        );
        return Ok(sum);
    }

    // Raw f32, little-endian. JSON cost ~12 bytes per 4-byte float: a prefill
    // of 100 tokens gave ~5 MB per reduce and blew the body limit (413),
    // 64 times per decode.
    let mut body = Vec::with_capacity(data.len() * 4);
    for v in &data {
        body.extend_from_slice(&v.to_le_bytes());
    }
    let expected = data.len();
    let request_bytes = body.len() as u64;
    let started = std::time::Instant::now();

    let internal_token = match &state.auth {
        Some(auth) => {
            let config = auth.read().await;
            crate::auth::internal_node_token(&config)
        }
        None => None,
    };
    let headers = vec![
        (TP_BARRIER_HEADER.to_string(), barrier_id.to_string()),
        (TP_RANK_HEADER.to_string(), state.rank.to_string()),
        (
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        ),
    ]
    .into_iter()
    .chain(internal_token.as_ref().map(|_| {
        (
            crate::auth::INTERNAL_NODE_REQUEST_HEADER.to_string(),
            "1".to_string(),
        )
    }))
    .chain(internal_token.as_ref().map(|token| {
        (
            reqwest::header::AUTHORIZATION.as_str().to_string(),
            format!("Bearer {token}"),
        )
    }))
    .collect();
    let response = crate::relay::send_bytes_or_tunnel_with_candidates(
        &state.client,
        &state.rank0_url,
        &state.rank0_urls,
        "/v1/tp/reduce",
        body,
        headers,
        &state.rank0_addr_candidates,
        tunnel,
        state
            .rank0_peer_id
            .as_deref()
            .and_then(|id| id.parse().ok()),
        state.rank0_supports_tunnel,
        "primary TP",
    )
    .await
    .map_err(|e| e.to_string())?;
    record_reduce(
        expected,
        request_bytes,
        response.body.len() as u64,
        started.elapsed(),
        response.transport,
        response.fallback,
    );
    let bytes = response.body;
    if bytes.len() != expected * 4 {
        return Err(format!(
            "sum came with {} bytes, expected {}",
            bytes.len(),
            expected * 4
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_keeps_the_inference_route_when_using_the_tunnel() {
        let (base_url, path) = broadcast_target("https://[2001:db8::2]:8187", "/v1/prefill");

        assert_eq!(base_url, "https://[2001:db8::2]:8187");
        assert_eq!(path, "/v1/prefill");
    }

    /// The `barrier_id` is a function of the decode, not of each process's clock.
    ///
    /// Two ranks that open the **same** decode number the reduces alike, so
    /// the partials meet. While it was a per-process counter, one worker
    /// restarting was enough for the group never to meet again - and that gave
    /// no error, it just hung.
    #[test]
    fn the_barrier_id_comes_from_the_decode_so_every_rank_agrees() {
        let _guard = GLOBAL_STATE.lock();
        cleanup();
        begin_decode("sessao-x:chunk-0");
        let rank0: Vec<String> = (0..3).map(|_| next_barrier_id().unwrap()).collect();

        // Another process, same decode: the numbering restarts from zero.
        begin_decode("sessao-x:chunk-0");
        let rank1: Vec<String> = (0..3).map(|_| next_barrier_id().unwrap()).collect();

        assert_eq!(rank0, rank1, "the ranks must agree on the ids");
        assert_eq!(rank0[0], "sessao-x:chunk-0#0");
        assert_eq!(rank0[2], "sessao-x:chunk-0#2");

        // The next decode does not reuse the previous one's id.
        begin_decode("sessao-x:chunk-1");
        assert_eq!(next_barrier_id().unwrap(), "sessao-x:chunk-1#0");
    }

    /// Turning the group off turns the node off: deploying a whole model on top
    /// of a TP group must leave no residue.
    ///
    /// `TP_STATE` was a `OnceLock`, which cannot be cleared. A process that
    /// once served TP kept declaring itself TP forever: the next deployment,
    /// of a whole model with no collective at all, warned "reduced 1
    /// times, expected 64" - and the zero-reduces check went as far as failing
    /// the request (18/08).
    #[test]
    fn turning_the_group_off_leaves_no_residue() {
        let _guard = GLOBAL_STATE.lock();
        cleanup();
        register_tp_reduce(
            1,
            2,
            "https://127.0.0.1:9/".into(),
            Some("um-node-id-qualquer".into()),
            None,
            false,
            None,
        );
        begin_replica_decode("decode-do-grupo");
        let _ = next_barrier_id();
        note_failure("failure of the old group".into());
        assert!(is_active(), "with a group on, the node is TP");

        unregister_tp_reduce();

        assert!(!is_active(), "without a group, the node is not TP");
        assert_eq!(reduce_count(), 0);
        assert!(!is_replica(), "the replica mark does not survive");
        assert!(
            next_barrier_id().is_none(),
            "there is no open decode after turning off"
        );
        assert!(
            take_failure().is_none(),
            "a failure of the old group does not bring down the request of the new model"
        );
    }

    /// A replica decides nothing; rank 0 decides. And opening a normal decode
    /// removes the mark, otherwise the process would carry "I am a replica" forever.
    #[test]
    fn a_replica_has_no_say_and_the_mark_does_not_leak() {
        let _guard = GLOBAL_STATE.lock();
        cleanup();
        begin_decode("normal");
        assert!(!is_replica(), "rank 0 decides for itself");

        begin_replica_decode("do-rank-0");
        assert!(
            is_replica(),
            "whoever received the id of rank 0 is a replica"
        );
        assert_eq!(next_barrier_id().unwrap(), "do-rank-0#0");

        begin_decode("outro");
        assert!(!is_replica(), "an own decode after a replica is own again");
    }

    /// The collective's failure is stored, consumed once, and the **first**
    /// wins: the following ones are its consequence.
    ///
    /// It is what turns "wrong reply" into "request with an error". An
    /// all-reduce that fails does not degrade quality - it produces false
    /// numbers, and the text comes out plausible and meaningless.
    #[test]
    fn the_first_failure_wins_and_is_consumed_once() {
        let _guard = GLOBAL_STATE.lock();
        cleanup();
        let _ = take_failure(); // clears residue from another test

        note_failure("certificate of the primary did not pin".into());
        note_failure("next error, a consequence of the first".into());

        let reason = take_failure().expect("the failure must survive until the decode");
        assert!(
            reason.contains("certificate"),
            "the first failure is the one that explains: {reason}"
        );
        assert!(
            take_failure().is_none(),
            "consumed once: the next request does not inherit the failure of the previous one"
        );
    }

    #[test]
    fn telemetry_keeps_collective_counts_and_bytes_separate() {
        let _guard = GLOBAL_STATE.lock().unwrap();
        cleanup();
        register_tp_reduce(0, 2, "https://127.0.0.1:9/".into(), None, None, false, None);
        begin_decode("telemetry");
        set_expected_reduces(64);
        record_broadcast_scheduled(2, 128);
        record_reduce(
            64,
            128,
            256,
            std::time::Duration::from_millis(20),
            ByteTransport::Direct,
            false,
        );
        record_reduce(
            64,
            128,
            256,
            std::time::Duration::from_millis(30),
            ByteTransport::Tunnel,
            true,
        );

        let telemetry = take_decode_telemetry().expect("TP telemetry");

        assert_eq!(telemetry.reduces, 2);
        assert_eq!(telemetry.expected_reduces, 64);
        assert_eq!(telemetry.request_bytes, 256);
        assert_eq!(telemetry.response_bytes, 512);
        assert_eq!(telemetry.direct_calls, 1);
        assert_eq!(telemetry.tunnel_calls, 1);
        assert_eq!(telemetry.fallback_calls, 1);
        assert_eq!(telemetry.broadcast_calls, 2);
        assert_eq!(telemetry.broadcast_bytes, 256);
        assert_eq!(telemetry.reduce_p50_ms, 30);
        assert_eq!(telemetry.reduce_p95_ms, 30);
    }

    #[tokio::test]
    async fn primary_contributes_to_the_local_barrier_without_dialing_itself() {
        let _guard = GLOBAL_STATE.lock().unwrap();
        cleanup();
        let barrier = ReduceBarrier::new();
        register_tp_reduce(
            0,
            2,
            "https://[2001:db8::1]:8181".into(),
            Some("primary-node".into()),
            Some("primary-endpoint".into()),
            true,
            None,
        );
        let barrier_handle = set_local_barrier(barrier.clone(), 2).expect("primary state");

        let state = tp_state().expect("primary state");
        let worker = barrier
            .contribute("local-primary", 1, 2, vec![3.0])
            .expect("worker contribution");
        let primary = post_reduce(&state, "local-primary", vec![2.0], None);
        let (worker_sum, primary_sum) = tokio::join!(worker, primary);

        assert_eq!(worker_sum.expect("worker sum"), vec![5.0]);
        assert_eq!(primary_sum.expect("primary sum"), vec![5.0]);
        clear_local_barrier(barrier_handle);
        unregister_tp_reduce();
    }

    #[tokio::test]
    async fn callback_can_block_on_tokio_from_a_llama_blocking_thread() {
        let _guard = GLOBAL_STATE.lock().unwrap();
        cleanup();
        register_tp_reduce(
            0,
            1,
            "http://127.0.0.1:9/".into(),
            Some("primary-node".into()),
            None,
            false,
            None,
        );
        let barrier_handle = set_local_barrier(ReduceBarrier::new(), 1).expect("barrier");
        begin_decode("blocking-thread");

        let values = std::thread::spawn(|| {
            let mut values = vec![2.0f32, 3.0];
            unsafe {
                tp_reduce_cb(
                    values.as_mut_ptr().cast(),
                    values.len() * std::mem::size_of::<f32>(),
                    std::ptr::null_mut(),
                );
            }
            values
        })
        .join()
        .expect("the callback cannot panic");

        assert_eq!(values, [2.0, 3.0]);
        clear_local_barrier(barrier_handle);
        unregister_tp_reduce();
    }

    use crate::collective::ReduceBarrier;
    use axum::{routing::post, Router};
    use tokio::sync::mpsc;

    /// Lock that serializes the tests that touch the collective's GLOBAL state
    /// (`TP_STATE`, `DECODE`, `REPLICA`, `TP_FAILURE`).
    ///
    /// Without this they trample each other in parallel and fail by chance - the
    /// same defect that haunted `transfer::offer_latency_tests` the whole day
    /// of 18/08, and that was only explained after hours. Every new test that
    /// touches those statics takes this lock.
    static GLOBAL_STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Leaves the process with no group, so a test does not inherit the previous one's.
    fn cleanup() {
        unregister_tp_reduce();
    }

    /// The two production sides talking to each other: the real client
    /// (`post_reduce`) against the real handler (`infer_server::handle_tp_reduce`).
    ///
    /// The previous version of this test built a JSON server of its own and a
    /// JSON client of its own - it exercised neither of the two real sides, and
    /// kept passing green after production switched the body to raw f32.
    /// A test that cannot fail when the contract changes is not a safety
    /// net, it is decoration.
    #[tokio::test]
    async fn three_ranks_reduce_over_the_real_wire() {
        let app = Router::new()
            .route("/v1/tp/reduce", post(crate::infer_server::handle_tp_reduce))
            .with_state((ReduceBarrier::new(), 3u32));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let url = format!("http://{addr}");
        let partials = vec![vec![1.0f32, 2.0], vec![3.0, 4.0], vec![5.0, 6.0]];
        let expected = vec![9.0f32, 12.0];

        let handles: Vec<_> = partials
            .into_iter()
            .enumerate()
            .map(|(rank, data)| {
                let state = Arc::new(TpState {
                    rank: rank as u32,
                    size: 3,
                    rank0_url: url.clone(),
                    rank0_urls: vec![url.clone()],
                    rank0_addr_candidates: Vec::new(),
                    rank0_peer_id: None,
                    rank0_supports_tunnel: false,
                    client: reqwest::Client::new(),
                    auth: None,
                    runtime: None,
                    counter: AtomicU64::new(0),
                    local_barrier: std::sync::RwLock::new(None),
                });
                tokio::spawn(async move { post_reduce(&state, "b", data, None).await })
            })
            .collect();

        for h in handles {
            assert_eq!(h.await.unwrap().expect("reduce"), expected);
        }
    }

    #[tokio::test]
    async fn reduce_uses_the_tunnel_when_the_primary_http_endpoint_is_unreachable() {
        let peer_id = crate::p2p::random_endpoint_id();
        let (tunnel, mut received) = mpsc::channel::<crate::infer_tunnel::TunnelOutbound>(1);
        let receiver = tokio::spawn(async move {
            let outbound = received.recv().await.expect("tunnel request");
            assert_eq!(outbound.peer_id, peer_id);
            assert_eq!(outbound.request.path, "/v1/tp/reduce");
            assert_eq!(
                &outbound.request.headers[..2],
                &[
                    (TP_BARRIER_HEADER.to_string(), "b".to_string()),
                    (TP_RANK_HEADER.to_string(), "1".to_string()),
                ]
            );
            let values: Vec<f32> = outbound
                .request
                .body
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            assert_eq!(values, [1.0, 2.0]);
            outbound
                .reply
                .send(Ok(crate::infer_tunnel::TunnelResponse {
                    status: 200,
                    body: [3.0f32, 4.0]
                        .into_iter()
                        .flat_map(f32::to_le_bytes)
                        .collect(),
                }))
                .expect("reply");
        });

        let state = Arc::new(TpState {
            rank: 1,
            size: 2,
            rank0_url: "http://127.0.0.1:1".into(),
            rank0_urls: vec!["http://127.0.0.1:1".into()],
            rank0_addr_candidates: Vec::new(),
            rank0_peer_id: Some(peer_id.to_string()),
            rank0_supports_tunnel: true,
            client: reqwest::Client::new(),
            auth: None,
            runtime: None,
            counter: AtomicU64::new(0),
            local_barrier: std::sync::RwLock::new(None),
        });
        let result = post_reduce(&state, "b", vec![1.0, 2.0], Some(&tunnel))
            .await
            .expect("tunnel reduce");

        assert_eq!(result, [3.0, 4.0]);
        receiver.await.expect("receiver");
    }
}
