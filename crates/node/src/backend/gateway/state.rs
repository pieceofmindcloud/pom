use super::*;
use axum::body::Body;
use axum::http::Method;

/// Tokens `/api/generate` will produce before stopping on its own.
pub const DEFAULT_MAX_TOKENS: u32 = 512;
/// A peer that cannot answer its inference health endpoint is not eligible for
/// a new pipeline. UDP discovery is deliberately lightweight and may still
/// hear an isolated or old node, so it is not sufficient proof of a usable hop.
/// This is the ceiling **of the admission probe**, short on purpose.
///
/// A live peer on the LAN answers `/health` in a few milliseconds - the inference's own
/// decode calls measure 12-18 ms round trip. Whoever did not
/// answer in 100 ms is not "slow": either it swallowed the SYN, or it is off, and in
/// both cases it will not serve this request. Before this ceiling, what ruled
/// was `peer_client::CONNECT_TIMEOUT` of 2 s, and since `join_all` only finishes with
/// the slowest, **one** unreachable announcer cost 2.005 s on every
/// inference of the cluster - measured in production, constant across engines, prompt
/// sizes and cold/warm.
///
/// It used to be 300 ms. The ceiling applies per address, and a peer's addresses are
/// tried in sequence: two dead addresses of a peer that was not even serving the
/// model cost 609 ms of TTFT on a request whose hops answered in
/// 6 ms (2026-10-05). A live peer farther away than this ceiling still gets in
/// through the tunnel (`TUNNEL_HEALTH_TIMEOUT`).
pub(crate) const ADMISSION_PROBE_TIMEOUT: Duration = Duration::from_millis(100);

/// How long a peer that failed the probe stays out without being probed again.
///
/// The defense does not weaken: the peer in quarantine is **excluded** from the plan, which is
/// exactly what the probe wanted. What the quarantine removes is the cost of
/// rediscovering on every request that the dead one is still dead. The return is
/// automatic - once the period passes, the next request pays one probe and the peer
/// re-enters if it answers -, so no state needs to be cleaned by hand.
/// It is the single failure-memory window (`peer_client::FAILURE_MEMORY`).
pub(super) const ADMISSION_FAILURE_COOLDOWN: Duration = crate::peer_client::FAILURE_MEMORY;

/// Who failed the admission probe, and until when it stays out.
///
/// Only keeps the negative. The healthy peer is still probed on every
/// request - the freshness of the admission is the point of the probe and costs milliseconds.
#[derive(Default)]
pub struct AdmissionHealth {
    quarantine: tokio::sync::RwLock<std::collections::HashMap<String, Instant>>,
    /// Speed, queue and RTTs that each peer's `/health` published at the last
    /// admission probe. It is the input of the delivery-time planner
    /// (`fastest::plan_fastest`) and of the Dashboard.
    perf: std::sync::RwLock<std::collections::HashMap<String, (Instant, crate::fastest::PeerPerf)>>,
    /// The last peer admitted by a probe that finished, so a request whose probe missed the
    /// budget can still plan with it for `ADMISSION_MEMORY`.
    admitted: std::sync::RwLock<std::collections::HashMap<String, (Instant, PeerInfo)>>,
}

/// How old a remembered admission may be to stand in for a probe that missed the budget.
///
/// A late probe is not a refusal: it keeps running, and if the peer is really gone it ends
/// refused and erases the memory (`forget`). With 15 s, a peer whose `/health` sometimes took
/// longer than the budget was left out of the plan on the first request after a quiet minute,
/// and a deployment still served failed with "No valid pipeline" (2026-10-19). The window now
/// covers the gaps between requests; it stays bounded so a probe that never ends cannot keep a
/// peer in the plan forever.
pub(crate) const ADMISSION_MEMORY: Duration = Duration::from_secs(5 * 60);

/// The whole admission of a request. The planner waits no longer than this for the probes;
/// a probe that has not finished by then keeps running and refreshes the memory.
pub(crate) const ADMISSION_BUDGET: Duration = Duration::from_millis(250);

/// How long a request waits for a peer it has no remembered admission of. Right after a boot or
/// a join every connection is cold (mutual TLS, iroh, maybe a relay), the first probe missed
/// `ADMISSION_BUDGET`, the peer was left out and the first prompt failed with "No valid
/// pipeline" while the second one worked. It covers a direct miss followed by the tunnel
/// (`TUNNEL_HEALTH_TIMEOUT`); a peer that really is gone pays it once and then sits in
/// quarantine.
pub(crate) const FIRST_ADMISSION_BUDGET: Duration = Duration::from_millis(3_500);

/// How often `spawn_admission_warmup` looks for peers with no fresh admission.
const ADMISSION_WARMUP_INTERVAL: Duration = Duration::from_secs(5);

/// The warm-up probes again a peer whose admission is older than this, so the memory a request
/// falls back on never reaches `ADMISSION_MEMORY` while the peer keeps answering.
const ADMISSION_WARMUP_REFRESH: Duration = Duration::from_secs(150);

impl AdmissionHealth {
    fn remember(&self, peer: PeerInfo) {
        if let Ok(mut map) = self.admitted.write() {
            map.insert(peer.node_id.clone(), (Instant::now(), peer));
        }
    }

    fn forget(&self, node_id: &str) {
        if let Ok(mut map) = self.admitted.write() {
            map.remove(node_id);
        }
    }

    fn remembered(&self, node_id: &str, max_age: Duration) -> Option<PeerInfo> {
        self.admitted
            .read()
            .ok()?
            .get(node_id)
            .filter(|(at, _)| at.elapsed() <= max_age)
            .map(|(_, peer)| peer.clone())
    }
}

impl AdmissionHealth {
    async fn quarantined(&self, node_id: &str) -> bool {
        self.quarantine
            .read()
            .await
            .get(node_id)
            .is_some_and(|until| *until > Instant::now())
    }

    async fn note_failure(&self, node_id: &str) {
        self.quarantine.write().await.insert(
            node_id.to_string(),
            Instant::now() + ADMISSION_FAILURE_COOLDOWN,
        );
    }

    async fn note_success(&self, node_id: &str) {
        self.quarantine.write().await.remove(node_id);
    }

    pub(crate) fn note_perf(&self, node_id: &str, perf: crate::fastest::PeerPerf) {
        if let Ok(mut map) = self.perf.write() {
            map.insert(node_id.to_string(), (Instant::now(), perf));
        }
    }

    pub(crate) fn perf_snapshot(
        &self,
    ) -> std::collections::HashMap<String, crate::fastest::PeerPerf> {
        self.perf
            .read()
            .map(|map| {
                map.iter()
                    .map(|(id, (_, perf))| (id.clone(), perf.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `true` when `node_id` was never probed or the last probe is older than
    /// `max_age`. The planner probes on every request; whoever only reads (the Dashboard)
    /// uses this to avoid showing what a peer published minutes ago.
    pub(crate) fn perf_older_than(&self, node_id: &str, max_age: Duration) -> bool {
        self.perf
            .read()
            .map(|map| {
                map.get(node_id)
                    .is_none_or(|(at, _)| at.elapsed() > max_age)
            })
            .unwrap_or(true)
    }
}

/// Characters per token, used only to size up a prompt before sending it.
/// Deliberately pessimistic — the real tokenizer lives in the first peer's
/// llama.cpp context, and under-estimating here would let through the very
/// request this guard exists to stop.
pub(super) const CHARS_PER_TOKEN: usize = 3;

/// Deadline of the session lifecycle on the peer (`/v1/session/{rewind,release,
/// rollback}`). On the other side it only erases or checks a KV sequence, but
/// the handler needs the engine lock, and on a peer busy with another session's
/// prefill chunk that waits for the whole chunk (~10 s per 512 tokens on slow
/// hardware). The old 3 s deadline expired during that wait: the rewind failed
/// and the conversation lost its retained prefix, and the release left the
/// session stuck in the KV. A dead peer is caught earlier by the connection's
/// lifetime. The stream's final event waits for the rewind (see
/// `generate_stream_inner`), so on a busy peer it arrives later instead of
/// arriving without the prefix.
pub(super) const SESSION_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(60);

// ── State ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ClusterSnapshot {
    pub(crate) fingerprint: String,
    pub(crate) body: serde_json::Value,
    pub(crate) at: Instant,
    pub(crate) version: u64,
    pub(crate) etag: String,
}

#[derive(Default)]
pub(crate) struct ClusterSnapshotCache {
    pub(super) snapshot: Option<ClusterSnapshot>,
    pub(super) next_version: u64,
}

/// Live publisher of this node's pkarr record.
///
/// The record is the "key -> where to dial now" pointer: the endpoint's public
/// direct addresses and the URLs of the relays it is reachable through. It is rebuilt from the live `Network` state on every
/// round and only republished when it changes, or every `PKARR_REFRESH` so it
/// does not vanish from the DHT. It is always published, with or without a
/// deployment: an idle admin is exactly who a new node needs to find by the
/// trust document's key.
pub(crate) struct PkarrPublishHandle {
    node: Arc<crate::pkarr_node::PkarrNode>,
    gateway_port: Option<u16>,
    inference_port: Option<u16>,
    network: Arc<crate::network::Network>,
    geo_label: std::sync::Mutex<String>,
    last: tokio::sync::Mutex<Option<(crate::pkarr_node::PkarrRecord, Instant)>>,
}

/// Republish even without changes: the Mainline DHT forgets old records.
pub(crate) const PKARR_REFRESH: Duration = Duration::from_secs(30 * 60);
/// How often the publisher checks whether the record changed.
pub(crate) const PKARR_CHECK_INTERVAL: Duration = Duration::from_secs(15);

impl PkarrPublishHandle {
    pub(crate) fn new(
        node: Arc<crate::pkarr_node::PkarrNode>,
        gateway_port: Option<u16>,
        inference_port: Option<u16>,
        network: Arc<crate::network::Network>,
        geo_label: String,
    ) -> Self {
        Self {
            node,
            gateway_port,
            inference_port,
            network,
            geo_label: std::sync::Mutex::new(geo_label),
            last: tokio::sync::Mutex::new(None),
        }
    }

    pub(crate) fn node(&self) -> &Arc<crate::pkarr_node::PkarrNode> {
        &self.node
    }

    /// The record this node should have published right now.
    pub(crate) fn desired_record(&self) -> crate::pkarr_node::PkarrRecord {
        let (addrs, relays) = record_addrs(&self.network.endpoint_addrs());
        let geo_label = self.geo_label.lock().expect("geo label lock").clone();
        crate::pkarr_node::PkarrRecord {
            addrs,
            relays,
            gateway_port: self.gateway_port,
            inference_port: self.inference_port,
            // Only a public relay is one in the DHT: that record is how
            // other networks find a relay by its key. Read live, because
            // the option changes from the screen.
            is_relay: self.network.relay_gate().everyone(),
            geo_label: (!geo_label.trim().is_empty()).then_some(geo_label),
        }
    }

    /// Changes the geo label and republishes immediately.
    pub(crate) async fn set_geo_label(&self, geo_label: &str) -> anyhow::Result<()> {
        *self.geo_label.lock().expect("geo label lock") = geo_label.to_string();
        self.publish_if_changed().await.map(|_| ())
    }

    /// Publishes when the record changed or expired. Returns whether it
    /// published.
    pub(crate) async fn publish_if_changed(&self) -> anyhow::Result<bool> {
        // A node off the network (discovery off, or taken off by its
        // operator) must not be findable by key either.
        if self.network.is_detached() {
            return Ok(false);
        }
        let desired = self.desired_record();
        let mut last = self.last.lock().await;
        if last
            .as_ref()
            .is_some_and(|(record, at)| *record == desired && at.elapsed() < PKARR_REFRESH)
        {
            return Ok(false);
        }
        self.node.publish(&desired).await?;
        *last = Some((desired, Instant::now()));
        Ok(true)
    }

    /// Republish loop, for the whole life of the process.
    ///
    /// With no reachable DHT (a network without internet access), every attempt
    /// fails: the wait between attempts doubles up to `PKARR_REFRESH` and only
    /// the 1st, 2nd, 4th, 8th... consecutive failure becomes a `warn` (see
    /// `PublishBackoff`). To turn it off for good, use `--no-pkarr`.
    pub(crate) fn spawn_republisher(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PKARR_CHECK_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut backoff = PublishBackoff::default();
            loop {
                ticker.tick().await;
                if !backoff.ready(Instant::now()) {
                    continue;
                }
                match self.publish_if_changed().await {
                    Ok(_) => {
                        if let Some(failures) = backoff.succeeded() {
                            tracing::info!(
                                "pkarr: publishing recovered after {failures} consecutive failure(s)"
                            );
                        }
                    }
                    Err(error) => {
                        let (failures, wait, loud) = backoff.failed(Instant::now());
                        if loud {
                            tracing::warn!(
                                "pkarr: publish failed (consecutive failure #{failures}; next in {} s): {error:#}",
                                wait.as_secs()
                            );
                        } else {
                            tracing::debug!(
                                "pkarr: publish failed (consecutive failure #{failures}): {error:#}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// Resolve a public directory candidate from its announced hex key.
    pub(crate) async fn resolve_public_peer(
        &self,
        pubkey: &str,
    ) -> Option<crate::pkarr_node::ResolvedPeer> {
        self.node.resolve_public_peer(pubkey).await
    }
}

/// Wait between failed pkarr publications: doubles from
/// `PKARR_CHECK_INTERVAL` up to `PKARR_REFRESH`, and only warns at the 1st,
/// 2nd, 4th, 8th... consecutive failure. Without it, a network without internet
/// access wrote a warning every 15 s forever.
#[derive(Debug, Default)]
pub(crate) struct PublishBackoff {
    failures: u32,
    next_try: Option<Instant>,
}

impl PublishBackoff {
    pub(crate) fn ready(&self, now: Instant) -> bool {
        self.next_try.is_none_or(|next| now >= next)
    }

    /// Records a failure: returns how many in a row, the wait until the next
    /// attempt and whether this one deserves a `warn`.
    pub(crate) fn failed(&mut self, now: Instant) -> (u32, Duration, bool) {
        self.failures = self.failures.saturating_add(1);
        let wait = PKARR_CHECK_INTERVAL
            .saturating_mul(1u32 << self.failures.saturating_sub(1).min(16))
            .min(PKARR_REFRESH);
        self.next_try = Some(now + wait);
        (self.failures, wait, self.failures.is_power_of_two())
    }

    /// Records a success; returns how many consecutive failures preceded it,
    /// if any.
    pub(crate) fn succeeded(&mut self) -> Option<u32> {
        let previous = std::mem::take(&mut self.failures);
        self.next_try = None;
        (previous > 0).then_some(previous)
    }
}

/// Splits the announced endpoint addresses into direct ones and relay URLs;
/// anything else stays out of the record.
///
/// The DHT is public: a private address (LAN, CGNAT, Tailscale, loopback) never
/// goes to it, even when it is the operator's pin. That path stays in the
/// `Announce`, which only admitted members read.
pub(crate) fn record_addrs(addrs: &[String]) -> (Vec<String>, Vec<String>) {
    let mut direct = Vec::new();
    let mut relays = Vec::new();
    for addr in addrs {
        if let Ok(socket) = addr.parse::<std::net::SocketAddr>() {
            let ip = socket.ip();
            if !ip.is_unspecified()
                && !crate::addrs::is_private_or_local(&ip.to_string())
                && !direct.contains(addr)
            {
                direct.push(addr.clone());
            }
        } else if crate::p2p::relay_url(addr).is_some() && !relays.contains(addr) {
            relays.push(addr.clone());
        }
    }
    (direct, relays)
}

impl ClusterSnapshotCache {
    pub(crate) fn get(
        &self,
        fingerprint: &str,
        now: Instant,
        ttl: Duration,
    ) -> Option<ClusterSnapshot> {
        let snapshot = self.snapshot.as_ref()?;
        let age = now.checked_duration_since(snapshot.at)?;
        (snapshot.fingerprint == fingerprint && age < ttl).then(|| snapshot.clone())
    }

    pub(crate) fn store(
        &mut self,
        fingerprint: impl Into<String>,
        body: serde_json::Value,
        etag: impl Into<String>,
        at: Instant,
    ) -> ClusterSnapshot {
        let fingerprint = fingerprint.into();
        let version = self
            .snapshot
            .as_ref()
            .filter(|previous| previous.body == body)
            .map(|previous| previous.version)
            .unwrap_or_else(|| {
                self.next_version = self.next_version.saturating_add(1);
                self.next_version
            });
        let snapshot = ClusterSnapshot {
            fingerprint,
            body,
            at,
            version,
            etag: etag.into(),
        };
        self.snapshot = Some(snapshot.clone());
        snapshot
    }
}

#[derive(Clone)]
pub struct GatewayState {
    pub network: Arc<Network>,
    pub ollama: Arc<OllamaClient>,
    pub default_model: String,
    /// Current assignment of the local node.
    pub local_assignment: Arc<tokio::sync::RwLock<Option<LayerAssignment>>>,
    /// Tracks in-flight inference requests per peer (for pipeline load balancing).
    pub loads: Arc<LoadTracker>,
    /// Prefixes of conversations that stay alive between turns. The entry only
    /// holds an already existing KV session, never weights or hidden state.
    pub prefix_cache: Arc<crate::prefix_cache::PrefixCache>,
    /// Disk index of the automatic prefix snapshots (`--slot-save-path`
    /// + `slot_persist_prefix_cache`). `None` when the feature is off.
    /// See `crate::prefix_cache::PrefixDiskIndex` and `build_prefix_disk_index`.
    pub prefix_disk: Option<Arc<crate::prefix_cache::PrefixDiskIndex>>,
    /// Adaptive profile per route, updated only after a completed
    /// prefill; choosing the chunk does not wait on I/O or on a network probe.
    pub(crate) adaptive_chunk_tuner: Arc<crate::gateway::AdaptiveChunkTuner>,
    /// Outbound HTTP clients, **one per peer**, with the destination's certificate
    /// pinned by node_id (`peer_client::PeerClients`).
    ///
    /// There used to be a shared `http` without pinning here as well. It left on
    /// purpose: with the cluster on TLS it has no way to verify anyone's
    /// certificate, and every call site that picked it up out of habit failed
    /// silently - that is how the deploy screen stopped seeing the peers'
    /// slices. For a destination that is not a peer (loopback, Ollama) use
    /// `peer_http.plain()`, which says what it is doing.
    pub peer_http: Arc<crate::peer_client::PeerClients>,
    /// Who failed the pipeline's admission probe, and until when it stays out.
    /// See `AdmissionHealth` and `ready_for_pipeline`.
    pub admission_health: Arc<AdmissionHealth>,
    /// Mutable node configuration driven by the web UI.
    pub runtime: Arc<NodeRuntime>,
    /// Durable audit trail for requests entering this gateway.
    pub audit: Arc<crate::reqlog::AuditLog>,
    /// Manager of the independent plugins (license, process host per
    /// plugin, own update). See `crate::plugins`.
    pub(crate) plugins: Arc<crate::plugins::PluginManager>,
    /// Detection/installation of the release of the `pom` binary itself. A product
    /// boundary separate from the plugins on purpose - see `crate::pom_core_update`.
    pub(crate) pom_core_update: Arc<crate::pom_core_update::PomCoreUpdater>,
    /// Single durable store for local operational state.
    pub database: Arc<crate::db::PomDb>,
    /// HTTP port this node serves the API and UI on.
    pub gateway_port: u16,
    /// Setup-level configuration (profile, connection key, bootstrap nodes).
    pub auth: AuthState,
    /// Durable anchor and grant chain for administrator capabilities. It is
    /// deliberately separate from `Config.profile`.
    pub(crate) admin_trust: Arc<crate::admin_trust::AdminTrust>,
    /// The p2p identity is the only signing authority for this node.
    pub(crate) authority_signing_key: Arc<ed25519_dalek::SigningKey>,
    pub(crate) authority_principal: crate::admin_trust::AuthorityPrincipal,
    /// In-memory browser sessions. It is intentionally not persisted beside
    /// the configuration and is bound to the current admin password hash.
    pub(crate) admin_sessions: crate::auth::AdminSessionState,
    /// In-flight slice transfers and the offers waiting for an answer.
    pub transfers: crate::transfer::Transfers,
    /// Frames this node dispatched into a pipeline and is still waiting on.
    ///
    /// Only used by the direct-reply relay (`relay/v2-direct`), where the tail
    /// answers this process instead of the answer bubbling back through every
    /// hop — see [`crate::direct`].
    pub reply_inbox: crate::direct::ReplyInbox,
    /// Dispatches a hop-to-hop call over the p2p endpoint instead of plain
    /// HTTP, as a NAT-tunnel fallback — see `relay::send_frame_or_tunnel`.
    /// `None` whenever this node's own p2p discovery never came up
    /// (disabled or failed to start); every prefill/decode call site treats
    /// that exactly like "the fallback wasn't available this time."
    pub tunnel_tx: Option<crate::infer_tunnel::TunnelOutboundSender>,
    /// This node's certificate and the cleartext policy, when the binary
    /// managed to derive them from the identity. `None` only on test servers,
    /// which come up on `127.0.0.1:0` and have no key pair to present.
    pub tls_server: Option<crate::serve::TlsServer>,
    /// The gateway's HTTP/3 certificate, independent of the TLS policy over TCP.
    pub quic_server: Option<crate::serve::QuicServerConfig>,
    /// Dedicated UDP port announced for the gateway's HTTP/3 endpoint.
    pub quic_port: Option<u16>,
    /// Capture, transport and persistence of the cluster's process logs.
    pub cluster_logs: Arc<crate::cluster_logs::ClusterLogs>,
    /// Global channel of the network, validated and relayed by the principal.
    pub network_chat: Arc<crate::network_chat::NetworkChat>,
    /// Short snapshot of the cluster, isolated per gateway and per test instance.
    pub(crate) cluster_cache: Arc<Mutex<ClusterSnapshotCache>>,
    /// Provider of remote models and their download jobs.
    pub(crate) model_service: Arc<crate::model_provider_backend::ModelService>,
    /// Best-effort pkarr republisher used when network metadata changes.
    pub(crate) pkarr_publisher: Option<Arc<PkarrPublishHandle>>,
}

impl GatewayState {
    pub(crate) fn signed_control_headers_for(
        signing_key: &ed25519_dalek::SigningKey,
        principal: crate::admin_trust::AuthorityPrincipal,
        trust: &crate::admin_trust::AdminTrust,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> anyhow::Result<reqwest::header::HeaderMap> {
        let envelope = crate::admin_trust::sign_control_envelope(
            signing_key,
            principal,
            &trust.document(),
            method,
            path,
            body,
        )?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            crate::auth::INTERNAL_NODE_REQUEST_HEADER,
            reqwest::header::HeaderValue::from_static("1"),
        );
        headers.insert(
            crate::auth::CONTROL_SIGNATURE_HEADER,
            reqwest::header::HeaderValue::from_str(&serde_json::to_string(&envelope)?)?,
        );
        Ok(headers)
    }

    /// Build the authenticated headers for a node-to-node control request.
    /// Public networks use only the signed envelope; private networks may
    /// still carry the shared secret as an additional transport guard.
    pub(crate) fn signed_control_headers(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> anyhow::Result<reqwest::header::HeaderMap> {
        Self::signed_control_headers_for(
            &self.authority_signing_key,
            self.authority_principal.clone(),
            &self.admin_trust,
            method,
            path,
            body,
        )
    }

    /// Add the chain signature to a peer control request. The shared key is
    /// intentionally only an optional private-network transport guard; the
    /// receiver authorizes the operation from the signed envelope.
    pub(crate) async fn authorize_control_request(
        &self,
        request: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let mut request = request.headers(self.signed_control_headers(method, path, body)?);
        let config = self.auth.read().await;
        if let Some(connection_key) = crate::auth::internal_node_token(&config) {
            request = request.bearer_auth(connection_key);
        }
        Ok(request)
    }

    pub(crate) async fn control_get_request(
        &self,
        request: reqwest::RequestBuilder,
        path: &str,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        self.authorize_control_request(request, "GET", path, &[])
            .await
    }

    pub(crate) async fn control_json_request<T: serde::Serialize>(
        &self,
        request: reqwest::RequestBuilder,
        path: &str,
        body: &T,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let bytes = serde_json::to_vec(body)?;
        Ok(self
            .authorize_control_request(request, "POST", path, &bytes)
            .await?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes))
    }
    /// Gateway addresses of `peer`, the one that answered first. See
    /// `Network::gateway_bases`.
    pub(crate) fn gateway_bases(&self, peer: &crate::network::PeerInfo) -> Vec<String> {
        self.network.gateway_bases(peer)
    }

    /// The first of `gateway_bases`: the address of `peer`'s gateway for
    /// whoever can only use one (a download, the return address of an offer).
    pub(crate) fn gateway_base(&self, peer: &crate::network::PeerInfo) -> Option<String> {
        self.gateway_bases(peer).into_iter().next()
    }

    /// Signed control GET to `peer`'s gateway, over all the paths
    /// it announces (addresses and, last, the p2p tunnel - see
    /// `peer_client::send_to_gateway_over`). `path` may carry a query string; the
    /// signature covers only the path, as in `control_get_request`.
    pub(crate) async fn control_get(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        self.control_call(peer, reqwest::Method::GET, path, None, timeout)
            .await
    }

    /// Signed JSON control POST to `peer`'s gateway, over the same
    /// paths as `control_get`.
    pub(crate) async fn control_post_json<T: serde::Serialize>(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        body: &T,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        let bytes = serde_json::to_vec(body)?;
        self.control_call(peer, reqwest::Method::POST, path, Some(bytes), timeout)
            .await
    }

    /// Signed control POST to `peer`'s gateway along the route this node
    /// already measured to it (`route_plan`, gateway plane): only the pinned
    /// address, or only the p2p tunnel when that is what reaches the peer -
    /// a machine behind NAT, for example. Without a measured route, the
    /// address that last answered. For a call with a deadline of
    /// milliseconds, where walking every announced address would spend it on
    /// the ones that do not answer. The tunnel stays as the fallback.
    /// A signed POST of opaque bytes (an image component op's body).
    pub(crate) async fn control_post_bytes(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        body: Vec<u8>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        self.control_call(peer, reqwest::Method::POST, path, Some(body), timeout)
            .await
    }

    pub(crate) async fn control_post_json_routed<T: serde::Serialize>(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        body: &T,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        use crate::route_plan::{book, Plane, RouteKind};
        let bases = match book().active(&peer.node_id, Plane::Gateway) {
            Some(route) if route.kind == RouteKind::Tunnel => Vec::new(),
            Some(route) => vec![route.base_url.trim_end_matches('/').to_string()],
            None => self.gateway_bases(peer).into_iter().take(1).collect(),
        };
        let bytes = serde_json::to_vec(body)?;
        self.control_call_over(
            peer,
            &bases,
            reqwest::Method::POST,
            path,
            Some(bytes),
            timeout,
        )
        .await
    }

    /// Signed control GET to `peer`'s gateway along the measured route, as
    /// [`Self::control_post_json_routed`]: a large download (a plugin
    /// engine's session file) goes over the address that reaches the peer
    /// fastest, not the first announced one that answers or the tunnel.
    pub(crate) async fn control_get_routed(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        use crate::route_plan::{book, Plane, RouteKind};
        let bases = match book().active(&peer.node_id, Plane::Gateway) {
            Some(route) if route.kind == RouteKind::Tunnel => Vec::new(),
            Some(route) => vec![route.base_url.trim_end_matches('/').to_string()],
            None => self.gateway_bases(peer).into_iter().take(1).collect(),
        };
        self.control_call_over(peer, &bases, reqwest::Method::GET, path, None, timeout)
            .await
    }

    /// [`Self::control_post_json_routed`] with a body of raw bytes (a frame of
    /// a plugin engine's stage link, `plugin_stage_link`).
    pub(crate) async fn control_post_bytes_routed(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        body: Vec<u8>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        use crate::route_plan::{book, Plane, RouteKind};
        let bases = match book().active(&peer.node_id, Plane::Gateway) {
            Some(route) if route.kind == RouteKind::Tunnel => Vec::new(),
            Some(route) => vec![route.base_url.trim_end_matches('/').to_string()],
            None => self.gateway_bases(peer).into_iter().take(1).collect(),
        };
        self.control_call_over(
            peer,
            &bases,
            reqwest::Method::POST,
            path,
            Some(body),
            timeout,
        )
        .await
    }

    /// `control_get` only over the tunnel, for whoever already tried the peer's addresses
    /// with its own criteria (see `send_to_gateway_over`).
    pub(crate) async fn control_get_via_tunnel(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        self.control_call_over(peer, &[], reqwest::Method::GET, path, None, timeout)
            .await
    }

    /// POST without a body (signed over an empty body), like `/api/ui/unload`.
    pub(crate) async fn control_post_empty(
        &self,
        peer: &crate::network::PeerInfo,
        path: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        self.control_call(peer, reqwest::Method::POST, path, None, timeout)
            .await
    }

    async fn control_call(
        &self,
        peer: &crate::network::PeerInfo,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        let bases = self.control_bases(peer);
        self.control_call_over(peer, &bases, method, path, body, timeout)
            .await
    }

    /// Where a control call to `peer` goes. This node's own entry is reached on
    /// loopback, in cleartext (`serve::plain_allowed`), the way the tunnel
    /// reaches it: through its announced addresses the call depended on the
    /// machine reaching itself on its LAN or public IP, which macOS refuses
    /// without the Local Network permission, and the deployment survey then
    /// left the node out of its own deployment screen. Only here, never in
    /// `gateway_bases`, whose first entry is handed to other machines.
    fn control_bases(&self, peer: &crate::network::PeerInfo) -> Vec<String> {
        match self.network.gateway_port {
            Some(port) if peer.node_id == self.network.node_id => {
                vec![format!("http://127.0.0.1:{port}")]
            }
            _ => self.gateway_bases(peer),
        }
    }

    async fn control_call_over(
        &self,
        peer: &crate::network::PeerInfo,
        bases: &[String],
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        let client = self.peer_http.for_peer(&peer.node_id)?;
        let signed_path = path.split('?').next().unwrap_or(path);
        let bytes = body.as_deref().unwrap_or_default();
        let mut headers = self.signed_control_headers(method.as_str(), signed_path, bytes)?;
        if let Some(connection_key) = {
            let config = self.auth.read().await;
            crate::auth::internal_node_token(&config)
        } {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {connection_key}"))?,
            );
        }
        if body.is_some() {
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
        }
        crate::peer_client::send_to_gateway_over(peer, bases, |base| {
            let request = client
                .request(method.clone(), format!("{base}{path}"))
                .timeout(timeout)
                .headers(headers.clone());
            match &body {
                Some(bytes) => request.body(bytes.clone()),
                None => request,
            }
        })
        .await
    }
}

#[derive(Serialize)]
pub(super) struct AdminAccessStatus {
    pub(super) admin_exposure: crate::config::AdminExposure,
    pub(super) requires_password: bool,
    /// The password in force is the first-access default: the interface goes in without
    /// asking and asks to change it.
    pub(super) default_password: bool,
    /// Identifies this run of the process (`boot_id`). The interface warns
    /// about the default password once per start, and this is how it tells a
    /// restart from a page reload.
    pub(super) boot_id: &'static str,
    pub(super) gateway_port: u16,
}

/// Random per process, never persisted: a restart is a new id. It says nothing
/// about the node, which matters because `/api/access` is read before login.
fn boot_id() -> &'static str {
    static BOOT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT_ID.get_or_init(|| Uuid::new_v4().simple().to_string())
}

pub(super) async fn admin_access_status(State(state): State<GatewayState>) -> impl IntoResponse {
    let cfg = state.auth.read().await;
    Json(AdminAccessStatus {
        admin_exposure: cfg.admin_exposure,
        requires_password: cfg.admin_exposure.requires_password(),
        default_password: cfg.admin_exposure.requires_password() && cfg.admin_password_default,
        boot_id: boot_id(),
        gateway_port: cfg.gateway_port,
    })
}

/// Routes used by inference or by the public compatibility API. This list is
/// the existing pipeline boundary from `20dfe12`; the UI gate must never be
/// applied to it, even when the node is in localhost-only mode.
///
/// `/api/transfer/*` is not on it: a peer reaches those routes signed, as a
/// node-to-node request that `caller_may_ask` checks, and this machine's own
/// screen through the browser session. Outside the surface, a request with
/// only the connection key - which every member holds - pulled model files
/// with no check at all.
pub(super) fn is_pipeline_or_public_path(path: &str) -> bool {
    path == "/api/access"
        || path == "/api/generate"
        || path == "/generate"
        || path == "/peers"
        || path == "/health"
        || path.starts_with("/api/public/")
        || path == "/handshake"
        || path == crate::cluster_logs_policy::CLUSTER_LOGS_POLICY_PATH
        || path == crate::network_chat_policy::NETWORK_CHAT_POLICY_PATH
        || path == crate::webui::NETWORK_SETTINGS_POLICY_PATH
        || path == crate::webui::MEMBER_LEFT_PATH
        || path.starts_with("/api/internal/")
        || path.starts_with("/api/plugin-host/")
        || path.starts_with("/v1/")
}

/// Administrative paths are protected at the server boundary. Unknown paths
/// stay restricted by default, so adding a control route cannot silently make
/// it public. This also covers the static bundle and the SPA entry point.
pub(crate) fn is_admin_surface_path(path: &str) -> bool {
    !is_pipeline_or_public_path(path)
}

/// The network-exposed UI bundle is a login screen, not an administrative
/// data endpoint. It must be reachable before a browser has a session, while
/// localhost-only mode still runs it through `AdminAuth` so a remote peer gets
/// the autonomous refusal page instead of any application code.
pub(super) fn is_public_ui_static_path(path: &str) -> bool {
    path == "/"
        || path == "/admin-ui"
        || path.starts_with("/admin-ui/")
        || (!path.starts_with("/api/")
            && !path.starts_with("/v1/")
            && path != "/admin"
            && !path.starts_with("/admin/")
            && path != "/peers"
            && path != "/health"
            && path != "/handshake")
}

/// Whether `peer` is a visitor. The role is granted at admission and an
/// announce is only what a node says about itself, so a machine admitted as
/// one stays one whatever it announces: on every node its membership token
/// says so (`crate::membership`), and on the admins their admission list
/// as well. Everywhere, a node that announces itself as a visitor is one.
pub(crate) fn peer_is_visitor(state: &GatewayState, peer: &PeerInfo) -> bool {
    use crate::config::EnrollmentRole;
    peer.profile == crate::profile::NodeProfile::Visitor
        || crate::membership::peer_role(peer, &state.admin_trust) == Some(EnrollmentRole::Visitor)
        || state
            .network_chat
            .admission_of(&peer.node_id)
            .is_some_and(|record| record.role == EnrollmentRole::Visitor)
        || state
            .network_chat
            .admitted_role(&peer.node_id, None)
            == Some(EnrollmentRole::Visitor)
}

/// What a node-to-node request may reach, by the caller's identity.
///
/// - A route that governs the network (`/admin`, settings, admissions) takes
///   a request **signed by the principal**. A secondary admin is refused.
/// - A deploy route (deploy, load, unload, stop) takes a request signed by
///   any admin of the trust chain, principal or secondary.
/// - A route of a serving node (catalog, planning, transfers) takes a signed
///   request from an admin or from a worker whose membership token says so
///   (`peer_role_is_attested`) - never on the strength of its announce.
/// - A route of this machine's own operator (`LocalOnly`) is refused to
///   every other node, signed or not.
/// - Everything else - inference, the node's own description, the internal
///   sync routes - stays as it was.
///
/// The connection key alone opens none of the first four: every member
/// holds it.
pub(crate) async fn caller_may_ask(
    state: &GatewayState,
    scope: crate::webui::RouteScope,
    issuer: Option<&crate::admin_trust::AuthorityPrincipal>,
) -> Result<(), &'static str> {
    use crate::webui::RouteScope;
    if scope == RouteScope::LocalOnly {
        return Err("local_operator_only");
    }
    if !matches!(
        scope,
        RouteScope::PrincipalOnly | RouteScope::AnyAdmin | RouteScope::ServingMember
    ) {
        return Ok(());
    }
    let Some(issuer) = issuer else {
        return Err("signature_required");
    };
    if issuer == &state.authority_principal || issuer_is_principal(state, issuer) {
        return Ok(());
    }
    if scope == RouteScope::PrincipalOnly {
        return Err("principal_required");
    }
    if crate::admin_trust::control_issuer_is_admin(&state.admin_trust.document(), issuer) {
        return Ok(());
    }
    if scope == RouteScope::AnyAdmin {
        return Err("admin_required");
    }
    let peers = state.network.alive_peers().await;
    match peers.iter().find(|peer| peer.node_id == issuer.node_id) {
        Some(peer) if peer_is_visitor(state, peer) => Err("visitor_caller"),
        Some(peer) if peer_role_is_attested(state, peer).await => Ok(()),
        Some(_) => Err("role_unattested"),
        None => Err("unknown_caller"),
    }
}

/// Whether an admin vouched that `peer` was admitted as a worker, rather
/// than taking the peer's word for it: the membership token it announces
/// says so, checked against the chain alone. On an admin its own admission
/// list does too. A legacy network open to anyone with the anchor has no
/// admissions, and keeps the announce.
async fn peer_role_is_attested(state: &GatewayState, peer: &PeerInfo) -> bool {
    use crate::config::EnrollmentRole;
    crate::membership::peer_role(peer, &state.admin_trust) == Some(EnrollmentRole::Worker)
        || state
            .network_chat
            .admitted_role(&peer.node_id, state.admin_trust.revoked_node_at(&peer.node_id))
            == Some(EnrollmentRole::Worker)
        || state.auth.read().await.network.open_admission()
}

/// Whether `issuer` is the principal of this node's trust chain.
pub(crate) fn issuer_is_principal(
    state: &GatewayState,
    issuer: &crate::admin_trust::AuthorityPrincipal,
) -> bool {
    let document = state.admin_trust.document();
    issuer == &document.primary && crate::admin_trust::control_issuer_is_admin(&document, issuer)
}

pub(crate) async fn require_admin_for_surface(
    State(state): State<GatewayState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let (mut parts, body) = request.into_parts();
    // Only a request that says it comes from a node is judged as one. The
    // browser sends the connection key with no marker, and probing it logged
    // a refused node-to-node call on every poll of the interface.
    let claims_node = parts
        .headers
        .contains_key(crate::auth::CONTROL_SIGNATURE_HEADER)
        || parts
            .headers
            .contains_key(crate::auth::INTERNAL_NODE_REQUEST_HEADER);
    let internal_node = if claims_node {
        crate::auth::InternalNodeAuth::from_request_parts(&mut parts, &state)
            .await
            .ok()
    } else {
        None
    };
    let internal_node_request = internal_node.is_some();
    let request = Request::from_parts(parts, body);
    let profile = state.auth.read().await.profile;
    let scope = crate::webui::route_scope(&method, &path);
    // An inference node holds and serves no model, whoever asks. This comes
    // before everything else on purpose: the profile gate below lets a
    // node-to-node request through, and an admin's request is one.
    if profile == crate::profile::NodeProfile::Visitor && crate::webui::visitor_refuses(&path) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "this machine was admitted for inference only and does not hold or serve models",
                "reason": "visitor_profile",
                "path": path,
            })),
        )
            .into_response();
    }
    // What another node may ask of this one depends on who it is. The shared
    // connection key says "a member"; it does not say which, and every member
    // holds it, inference nodes included.
    if let Some(auth) = &internal_node {
        // Taking a machine out of the network is an admin's act: any admin
        // removes a worker or a visitor, and only the principal removes an
        // admin. The route is open to every profile because leaving is
        // abdication, so the scope cannot say it: a leave forwarded by
        // another node has to carry that signature.
        // An unsigned leave is refused too: the connection key alone would
        // let any member, a visitor included, detach this machine or have
        // an admin forward the removal of another.
        let verdict = match auth.principal() {
            None if path == "/api/ui/leave" => Err("principal_required"),
            Some(issuer)
                if path == "/api/ui/leave"
                    && issuer != &state.authority_principal
                    && !issuer_is_principal(&state, issuer) =>
            {
                let document = state.admin_trust.document();
                if !crate::admin_trust::control_issuer_is_admin(&document, issuer) {
                    Err("admin_required")
                } else if crate::admin_trust::control_issuer_is_admin(
                    &document,
                    &state.authority_principal,
                ) {
                    Err("principal_required")
                } else {
                    Ok(())
                }
            }
            issuer => caller_may_ask(&state, scope, issuer).await,
        };
        if let Err(reason) = verdict {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "the calling node is not allowed to ask this",
                    "reason": reason,
                    "path": path,
                })),
            )
                .into_response();
        }
    }
    // The `admin` profile alone grants nothing: the trust chain decides, and
    // it tells the principal from a secondary admin, who only deploys.
    let local_authority = state
        .admin_trust
        .effective_authority(&state.authority_principal, profile);
    let authority_access = match scope {
        crate::webui::RouteScope::PrincipalOnly => {
            local_authority == Some(crate::admin_trust::EffectiveAuthority::Principal)
        }
        crate::webui::RouteScope::AnyAdmin => local_authority.is_some(),
        _ => true,
    };
    let profile_access = crate::webui::profile_can_access(profile, scope) && authority_access;
    if !profile_access && !internal_node_request {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "profile not authorized for this route",
                "path": path,
                "profile": profile.to_string(),
            })),
        )
            .into_response();
    }
    if !is_admin_surface_path(&path) {
        return next.run(request).await;
    }

    // A signed peer request already proved its transport/control identity. It
    // must not be forced through the browser-only session gate, especially on
    // a public network where there is intentionally no shared secret.
    if internal_node_request {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    if is_public_ui_static_path(&path) {
        let network_exposure = state.auth.read().await.admin_exposure.requires_password();
        if network_exposure {
            return next.run(Request::from_parts(parts, body)).await;
        }
    }
    let auth = match crate::auth::AdminAuth::from_request_parts(&mut parts, &state).await {
        Ok(auth) => auth,
        Err(rejection) => return rejection.into_response(),
    };
    let secure = parts
        .extensions
        .get::<crate::serve::TransportSecurity>()
        .is_some_and(|transport| transport.0);
    let cookie_name = crate::auth::admin_session_cookie_name(&parts.headers, secure);
    let mut response = next.run(Request::from_parts(parts, body)).await;
    if let Some(session_id) = auth.session_id_to_set {
        for path in crate::auth::ADMIN_SESSION_COOKIE_PATHS {
            let value = crate::auth::session_cookie_header(&cookie_name, &session_id, secure, path);
            if let Ok(value) = HeaderValue::from_str(&value) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
    }
    response
}

/// Check the body hash covered by a signed control envelope before any handler
/// can mutate state. The extractor still validates the header metadata so
/// direct unit-test routers get a useful refusal; this layer closes the gap
/// that only exists while axum has not handed the body to `Json` yet.
pub(crate) async fn verify_control_request(
    State(state): State<GatewayState>,
    request: Request,
    next: Next,
) -> Response {
    if !request
        .headers()
        .contains_key(crate::auth::CONTROL_SIGNATURE_HEADER)
    {
        return next.run(request).await;
    }
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, crate::plugin_session::CONTROL_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "signed control exceeds the limit" })),
            )
                .into_response()
        }
    };
    let raw = parts
        .headers
        .get(crate::auth::CONTROL_SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok());
    let Some(raw) = raw else {
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    let envelope = match serde_json::from_str::<crate::admin_trust::ControlEnvelope>(raw) {
        Ok(envelope) => envelope,
        Err(_) => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "invalid control signature" })),
            )
                .into_response()
        }
    };
    if crate::auth::validate_signed_control_metadata(&state, &parts.headers, &method, &path)
        .await
        .is_err()
        || crate::admin_trust::verify_control_envelope(
            &envelope,
            &state.admin_trust.document(),
            method.as_str(),
            &path,
            &bytes,
        )
        .is_err()
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "invalid control signature" })),
        )
            .into_response();
    }
    if !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        if let Err(error) = state.admin_trust.consume_control_replay(&envelope) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": format!("control command refused: {error:#}") })),
            )
                .into_response();
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
mod cluster_cache_tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn a_known_state_change_bypasses_the_ttl_and_advances_the_version() {
        let mut cache = ClusterSnapshotCache::default();
        let at = Instant::now();
        let first = cache.store("state-a", json!({ "peers": [] }), "etag-a", at);

        assert!(cache
            .get(
                "state-a",
                at + Duration::from_millis(50),
                Duration::from_secs(2)
            )
            .is_some());
        assert!(cache
            .get(
                "state-b",
                at + Duration::from_millis(50),
                Duration::from_secs(2)
            )
            .is_none());

        let second = cache.store("state-b", json!({ "peers": [{ "id": "b" }] }), "etag-b", at);
        assert!(second.version > first.version);
    }

    #[test]
    fn an_unchanged_snapshot_expires_after_the_short_ttl() {
        let mut cache = ClusterSnapshotCache::default();
        let at = Instant::now();
        cache.store("state-a", json!({ "peers": [] }), "etag-a", at);

        assert!(cache
            .get(
                "state-a",
                at + Duration::from_secs(2),
                Duration::from_secs(2)
            )
            .is_none());
    }
}

impl axum::extract::FromRef<GatewayState> for AuthState {
    fn from_ref(state: &GatewayState) -> Self {
        state.auth.clone()
    }
}

impl axum::extract::FromRef<GatewayState> for crate::auth::AdminSessionState {
    fn from_ref(state: &GatewayState) -> Self {
        state.admin_sessions.clone()
    }
}

impl axum::extract::FromRef<GatewayState> for Arc<crate::db::PomDb> {
    fn from_ref(state: &GatewayState) -> Self {
        state.database.clone()
    }
}

/// The connection key gives the operator authority over which remote identities
/// this node trusts. A blocked peer may keep broadcasting UDP announcements,
/// but it must not enter the dashboard, deployment survey, or inference plan.
pub(super) fn peers_visible_to_cluster(peers: Vec<PeerInfo>, blocked: &[String]) -> Vec<PeerInfo> {
    peers
        .into_iter()
        .filter(|peer| !blocked.contains(&peer.node_id))
        .collect()
}

/// Keep only peers whose actual inference endpoint can answer now. This is a
/// per-request admission check: a stale UDP announcer must not poison the
/// layer count or leave a request waiting for a hop that no longer serves HTTP.
fn refresh_peer_from_health(peer: PeerInfo, body: &serde_json::Value) -> Option<PeerInfo> {
    if body
        .get("model_loaded")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }

    let mut current = peer;
    if let Some(model) = body
        .get("model_label")
        .and_then(serde_json::Value::as_str)
        .filter(|model| !model.is_empty())
    {
        current.model = model.to_string();
    }
    if let Some(deploy_id) = body.get("deploy_id").and_then(serde_json::Value::as_str) {
        current.deploy_id = deploy_id.to_string();
    }

    // The P2P announcement and the inference process are separate update
    // streams. Once /health proves the current process is loaded, its range is
    // the freshest executable topology available to this gateway.
    let start = body
        .get("start_layer")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let end = body
        .get("end_layer")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let total = body
        .get("total_layers")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    if let (Some(start), Some(end), Some(total)) = (start, end, total) {
        if total == 0 || start > end || end >= total {
            return None;
        }
        current.start_layer = start;
        current.end_layer = end;
        current.total_layers = total;
    }

    Some(current)
}

/// Deadline of the `/health` probe through the tunnel: a circuit through the relay on the
/// internet does not fit in the direct probe's ceiling (`ADMISSION_PROBE_TIMEOUT`).
const TUNNEL_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// The admission probe follows the pinned route (`route_plan`): a tunnel route goes
/// straight to the tunnel, an HTTP/QUIC route puts the measured address in front. Without this, the
/// probe, which runs on every request, would pay for the dead address before the tunnel.
fn bases_by_route_plan(node_id: &str, bases: Vec<String>) -> Vec<String> {
    use crate::route_plan::{book, Plane, RouteKind};
    match book().active(node_id, Plane::Inference) {
        Some(route) if route.kind == RouteKind::Tunnel => Vec::new(),
        Some(route) if !route.base_url.is_empty() => {
            let mut ordered = vec![route.base_url.clone()];
            ordered.extend(bases.into_iter().filter(|base| *base != route.base_url));
            ordered
        }
        _ => bases,
    }
}

/// Admission of the peers that can take part in a pipeline. Only the peers that announce a
/// model, or are idle, are probed and waited for: a peer with nothing loaded cannot serve, and
/// it costs nothing here. Each probe runs on its own task, and the request waits at most
/// `ADMISSION_BUDGET` for them. A probe that misses the budget keeps running and refreshes the
/// memory, so the next request has its verdict; in the meantime a verdict younger than
/// `ADMISSION_MEMORY` stands in for it.
pub(crate) async fn ready_for_pipeline(
    http: &crate::peer_client::PeerClients,
    health: &Arc<AdmissionHealth>,
    network: &Arc<crate::network::Network>,
    peers: Vec<PeerInfo>,
    timeout: Duration,
) -> Vec<PeerInfo> {
    let started = Instant::now();
    let mut probes = Vec::new();
    for peer in peers {
        if !admission_candidate(&peer) {
            continue;
        }
        // A peer never admitted (a boot, a join, a new peer) gets the longer wait: with the
        // short one its cold first probe always missed and the request had nobody to plan with.
        let deadline = if health.remembered(&peer.node_id, ADMISSION_MEMORY).is_some() {
            started + ADMISSION_BUDGET
        } else {
            started + FIRST_ADMISSION_BUDGET
        };
        let node_id = peer.node_id.clone();
        let last_seen = peer.last_seen;
        probes.push((
            node_id,
            last_seen,
            deadline,
            spawn_admission_probe(http, health, network, peer, timeout),
        ));
    }
    let probed = probes.len();
    let mut admitted = Vec::with_capacity(probed);
    for (node_id, last_seen, deadline, handle) in probes {
        let short = &node_id[..node_id.len().min(12)];
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, handle).await {
            Ok(Ok((Some(peer), via, took))) => {
                tracing::info!(
                    "admission: peer {short} {via} in {:.1} ms",
                    took.as_secs_f64() * 1000.0
                );
                admitted.push(peer);
            }
            Ok(Ok((None, via, took))) => {
                tracing::info!(
                    "admission: peer {short} refused ({via}) in {:.1} ms",
                    took.as_secs_f64() * 1000.0
                );
            }
            Ok(Err(_)) => {
                tracing::warn!("admission: peer {short} probe ended in a panic");
            }
            Err(_) => match health.remembered(&node_id, ADMISSION_MEMORY) {
                Some(mut peer) => {
                    // The memory is the last `/health` verdict, up to
                    // `ADMISSION_MEMORY` old; liveness is the announcement
                    // this request was handed. Keeping the stored
                    // `last_seen` made the planner drop the peer as dead
                    // (`PEER_TIMEOUT_SECS`) and report a gap at its layers.
                    peer.last_seen = last_seen;
                    tracing::info!("admission: peer {short} still probing; using its last admission");
                    admitted.push(peer);
                }
                None => tracing::info!("admission: peer {short} still probing; left out"),
            },
        }
    }
    tracing::info!(
        "admission: {} of {} probed peers in {:.1} ms",
        admitted.len(),
        probed,
        started.elapsed().as_secs_f64() * 1000.0
    );
    admitted
}

/// Only the peers that announce a model, or are idle, can take part in a pipeline; a peer with
/// nothing loaded is never probed.
fn admission_candidate(peer: &PeerInfo) -> bool {
    peer.idle || !peer.model.is_empty()
}

/// Runs one peer's admission on its own task and records the verdict in the memory, whether or
/// not anybody is still waiting for it.
fn spawn_admission_probe(
    http: &crate::peer_client::PeerClients,
    health: &Arc<AdmissionHealth>,
    network: &Arc<crate::network::Network>,
    peer: PeerInfo,
    timeout: Duration,
) -> tokio::task::JoinHandle<(Option<PeerInfo>, &'static str, Duration)> {
    // One client per peer: the probe has to speak the same language that the
    // inference will speak with that node, otherwise it measures - or refuses - a
    // path that the real request does not take.
    let client = http.for_peer(&peer.node_id);
    // All the inference addresses, the one that answered first. Only the
    // first announced - an unreachable IPv6, for example - left the
    // peer in quarantine and out of the plan ("Gap at layer 0") even though
    // reachable through another address or through the tunnel.
    let bases = bases_by_route_plan(&peer.node_id, network.inference_bases(&peer));
    let (health, network) = (health.clone(), network.clone());
    tokio::spawn(async move {
        let probe_started = Instant::now();
        let node_id = peer.node_id.clone();
        let (admitted, via) = admit_peer(&health, &network, peer, client, bases, timeout).await;
        match &admitted {
            Some(peer) => health.remember(peer.clone()),
            None => health.forget(&node_id),
        }
        (admitted, via, probe_started.elapsed())
    })
}

/// Keeps an admission in memory for every peer that can serve, so the first prompt after a
/// boot, a join or a quiet stretch finds its peers already admitted instead of waiting for
/// cold connections. Every `ADMISSION_WARMUP_INTERVAL` it probes the candidates with no
/// admission younger than `ADMISSION_WARMUP_REFRESH`, and waits for those probes before
/// looking again, so one peer never has two warm-up probes in flight. A peer in quarantine
/// answers at once without a network call (`admit_peer`). `/health` is not inference
/// activity, so the probes never keep an `unload_if_idle` peer loaded.
pub(crate) fn spawn_admission_warmup(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(ADMISSION_WARMUP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let stale: Vec<PeerInfo> = state
                .all_peers()
                .await
                .into_iter()
                .filter(|peer| {
                    admission_candidate(peer)
                        && state
                            .admission_health
                            .remembered(&peer.node_id, ADMISSION_WARMUP_REFRESH)
                            .is_none()
                })
                .collect();
            let probes: Vec<_> = stale
                .into_iter()
                .map(|peer| {
                    spawn_admission_probe(
                        &state.peer_http,
                        &state.admission_health,
                        &state.network,
                        peer,
                        ADMISSION_PROBE_TIMEOUT,
                    )
                })
                .collect();
            futures::future::join_all(probes).await;
        }
    });
}

/// One peer's admission. Returns the peer when it may take part, and the path that decided it,
/// for the log. The tunnel is tried only for a peer that announces a model, and not when every
/// direct address refused the connection: a refused address is a server that is not there.
/// This node's own `/health`, in cleartext on loopback (`serve::plain_allowed`).
/// The handler never waits for the engine (`infer_server::handle_health`), so the
/// probe's deadline only bounds a server that is not listening yet.
async fn local_health(port: u16, timeout: Duration) -> Option<serde_json::Value> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap_or_default()
    });
    let response = tokio::time::timeout(
        timeout,
        client.get(format!("http://127.0.0.1:{port}/health")).send(),
    )
    .await
    .ok()?
    .ok()?;
    if !response.status().is_success() {
        return None;
    }
    tokio::time::timeout(timeout, response.json::<serde_json::Value>())
        .await
        .ok()?
        .ok()
}

async fn admit_peer(
    health: &AdmissionHealth,
    network: &crate::network::Network,
    peer: PeerInfo,
    client: anyhow::Result<reqwest::Client>,
    bases: Vec<String>,
    timeout: Duration,
) -> (Option<PeerInfo>, &'static str) {
    // Idle peers keep their assignment announced while `/health` has no loaded model.
    // They are valid plan members and are woken by the reload event before inference starts.
    if peer.idle {
        return (Some(peer), "idle");
    }
    // This machine's own entry: its inference server is read on loopback, the
    // way the tunnel reaches it (`infer_tunnel::forward_loopback`). Dialing its
    // own public address went through QUIC and the NAT, and on a head busy with
    // a long prompt that missed the budget and left its layers out of the plan.
    if peer.node_id == network.node_id {
        if let Some(port) = network.inference_port {
            let body = local_health(port, timeout).await;
            return match body {
                Some(body) => {
                    health.note_perf(&peer.node_id, crate::fastest::PeerPerf::from_health(&body));
                    match refresh_peer_from_health(peer, &body) {
                        Some(peer) => (Some(peer), "local"),
                        None => (None, "not-loaded"),
                    }
                }
                None => (None, "local-miss"),
            };
        }
    }
    if bases.is_empty() && crate::peer_client::TunnelPeer::from_peer(&peer).is_none() {
        return (None, "no-address");
    }
    let Ok(client) = client else {
        return (None, "no-client");
    };
    // Whoever failed recently is not probed again at full price: it
    // stays out of the plan, and the request does not pay for it.
    if health.quarantined(&peer.node_id).await {
        return (None, "quarantined");
    }
    // Every address at once, the first good answer wins. Each probe reports whether its
    // address refused the connection, which is what decides about the tunnel below.
    let mut in_flight = futures::stream::FuturesUnordered::new();
    for base in &bases {
        let client = &client;
        let peer_id = peer.node_id.as_str();
        in_flight.push(Box::pin(async move {
            let url = format!("{base}/health");
            let response = match tokio::time::timeout(timeout, client.get(url).send()).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(error.is_connect()),
                Err(_) => return Err(false),
            };
            if !response.status().is_success()
                || crate::peer_client::answered_by_other_node(response.headers(), peer_id).is_some()
            {
                return Err(false);
            }
            match tokio::time::timeout(timeout, response.json::<serde_json::Value>()).await {
                Ok(Ok(value)) => Ok((base, value)),
                _ => Err(false),
            }
        }));
    }
    let mut body = None;
    let mut via = "direct-miss";
    let mut all_refused = !bases.is_empty();
    while let Some(outcome) = futures::StreamExt::next(&mut in_flight).await {
        match outcome {
            Ok((base, value)) => {
                network.routes.mark_success(&peer.node_id, base);
                body = Some(value);
                via = "direct";
                break;
            }
            Err(refused) => all_refused &= refused,
        }
    }
    drop(in_flight);
    if body.is_none() && all_refused {
        return (None, "refused-direct");
    }
    // No address answered: the p2p tunnel. Only a peer that announces a model can serve,
    // so only it is worth the tunnel's round trip.
    if body.is_none() && !peer.model.is_empty() {
        if let Some(tunnel) = crate::peer_client::TunnelPeer::from_peer(&peer) {
            body = crate::infer_tunnel::health_via_tunnel(&tunnel, TUNNEL_HEALTH_TIMEOUT).await;
            via = if body.is_some() {
                "tunnel"
            } else {
                "tunnel-miss"
            };
        }
    }
    let Some(body) = body else {
        health.note_failure(&peer.node_id).await;
        return (None, via);
    };
    let node_id = peer.node_id.clone();
    health.note_perf(&node_id, crate::fastest::PeerPerf::from_health(&body));
    let Some(peer) = refresh_peer_from_health(peer, &body) else {
        health.note_failure(&node_id).await;
        return (None, "not-loaded");
    };
    health.note_success(&peer.node_id).await;
    (Some(peer), via)
}

impl GatewayState {
    /// Model label the pipeline planner matches on right now.
    pub async fn active_model(&self) -> String {
        self.runtime.model_label(&self.default_model).await
    }

    /// The model the panel shows: the one this node serves, or, on a node
    /// that serves nothing (an admin that only coordinates), the model the
    /// network serves. Without it, the Dashboard of such a node had no active
    /// model, so no routes, RTT or measured speeds.
    pub async fn displayed_model(&self) -> String {
        let own = self.active_model().await;
        if !own.is_empty() {
            return own;
        }
        network_model(&self.all_peers().await)
    }

    /// Where this node writes slices, keeps received files, and the only
    /// directory a peer may pull from. Read from the live configuration, so a
    /// change on the screen takes effect without a restart.
    pub async fn shard_dir(&self) -> std::path::PathBuf {
        self.auth.read().await.shard_dir_path()
    }

    /// Directories of whole models this node scans.
    pub async fn model_dirs(&self) -> Vec<std::path::PathBuf> {
        self.auth.read().await.model_dir_paths()
    }

    /// Largest prompt this node's inference server can take, in tokens.
    ///
    /// llama.cpp aborts the *whole process* on
    /// `GGML_ASSERT(n_tokens_all <= cparams.n_batch)`, so an oversized prompt
    /// does not fail one request — it kills the peer and every request in
    /// flight on it. Coding harnesses send multi-thousand-token system prompts,
    /// which makes that trivially reachable from the public API. Refusing early
    /// keeps the node up; the caller gets a message naming the flags to raise.
    pub async fn prompt_token_budget(&self) -> Option<u32> {
        Some(prompt_budget(&self.runtime.config().await?))
    }

    /// Addresses through which a peer reaches **this** node, in order of preference.
    ///
    /// Same input the node already announces about itself (`self_dial_host` first, the
    /// enumerated candidates after): it is what a pipeline's tail uses to
    /// return the result directly, and getting it wrong here is the difference between a
    /// reply and a blown deadline.
    pub fn self_reply_hosts(&self) -> Vec<String> {
        let mut hosts = vec![self_dial_host(
            self.network.effective_host(),
            crate::addrs::outbound_local_ip(),
        )];
        hosts.extend(self.network.self_addr_candidates());
        hosts
    }

    /// Alive peers plus this node, so the local assignment participates in
    /// planning exactly like a remote one.
    pub async fn all_peers(&self) -> Vec<PeerInfo> {
        let blocked = self.auth.read().await.blocked_nodes.clone();
        let mut peers = peers_visible_to_cluster(self.network.alive_peers().await, &blocked);
        // This machine is always part of the cluster, model or no model. Hiding
        // it when there is no assignment made the whole machine list disappear
        // on a node configured through the web UI.
        {
            let local = self.local_assignment.read().await.clone();
            // It is not `127.0.0.1`: this entry travels as is inside
            // `next_hops` for the ring handoff, and a hop on another machine
            // dialing "127.0.0.1" reaches itself, not us. The hub mode
            // dialing its own machine by the real IP instead of the loopback
            // keeps working (the inference server binds to
            // `0.0.0.0`), so there is no swap to make here.
            //
            // And it is not the LAN IP when `advertise_addr` exists: between sites,
            // a `192.168.x.y` is as unreachable for the other side as the
            // loopback. The symptom was silent and asymmetric - with this node as
            // head the pipeline went through, because the gateway talked to itself over the
            // LAN; with it as successor, the remote hop received a private
            // address and the request died without getting anywhere.
            // `effective_host` already comes with brackets when it is IPv6. Pinned
            // by the operator wins; without a pin, the interface's current public address
            // wins over the old address observed by `identify`.
            // The observed one remains a fallback for NAT.
            let self_host = self_dial_host(
                self.network.effective_host(),
                crate::addrs::outbound_local_ip(),
            );
            peers.push(PeerInfo {
                deploy_id: local
                    .as_ref()
                    .map(|assignment| assignment.deploy_id.clone())
                    .unwrap_or_default(),
                node_id: self.network.node_id.clone(),
                tls: self.network.tls_enabled(),
                pubkey: self.network.pubkey.clone(),
                cluster_id: self.network.cluster_id(),
                trust_version: self.network.trust_version(),
                pom_version: crate::pom_core_update::release_version().to_string(),
                pom_build: crate::build_commit(),
                membership: self.network.membership(),
                relay: self.network.relay_gate().offer(),
                anon_tunnel: None,
                addr: format!("{}:{}", self_host, self.network.bind_addr.port()),
                addr_candidates: self.network.self_addr_candidates(),
                model: local.as_ref().map(|l| l.model.clone()).unwrap_or_default(),
                start_layer: local.as_ref().map(|l| l.start_layer).unwrap_or(0),
                end_layer: local.as_ref().map(|l| l.end_layer).unwrap_or(0),
                total_layers: local.as_ref().map(|l| l.total_layers).unwrap_or(0),
                memory_mb: local.as_ref().map(|l| l.memory_used_mb).unwrap_or(0.0),
                last_seen: chrono::Utc::now(),
                inference_port: self.network.inference_port,
                inference_quic_port: self.network.inference_quic_port,
                // Keep in sync with network.rs's `self_announce` and
                // infer_server.rs's health/announce builder, and with the
                // capability constants in routing/pipeline.rs.
                inference_frame_capabilities: if let Some(caps) =
                    self.network.plugin_engine_capabilities()
                {
                    caps
                } else {
                    let mut caps = vec![
                        crate::pipeline::CAP_FRAME_BATCH.into(),
                        crate::pipeline::CAP_FRAME_DTYPE_F8.into(),
                        crate::pipeline::CAP_FRAME_DTYPE_I4G64.into(),
                        crate::pipeline::CAP_FRAME_DTYPE_I4G128.into(),
                        crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into(),
                        crate::pipeline::CAP_LINK_STREAM.into(),
                        "relay/v1-chain".into(),
                        "relay/v2-direct".into(),
                        "speculate/v1-lookup".into(),
                        "speculate/v1-mtp-sliced".into(),
                    ];
                    if self.network.mtp_ready() {
                        caps.push("speculate/v1-mtp".into());
                    }
                    if self.network.batching_ready() {
                        caps.push(crate::pipeline::CAP_FRAME_BATCH.into());
                    }
                    if self.network.slot_persist_ready() {
                        caps.push(crate::pipeline::CAP_SLOT_PERSIST.into());
                    }
                    if self.network.p2p_tunnel_ready() {
                        caps.push("transport/iroh-tunnel-v1".into());
                        caps.push(crate::pipeline::CAP_P2P_TUNNEL_STREAM.into());
                    }
                    caps
                },
                gateway_port: Some(self.gateway_port),
                gateway_quic_port: self.network.gateway_quic_port,
                profile: self.network.profile(),
                hostname: self.network.hostname(),
                capabilities: local
                    .as_ref()
                    .map(|l| {
                        vec![routing_contract::ModelCapability {
                            model_id: l.model.clone(),
                            start_layer: l.start_layer,
                            end_layer: l.end_layer,
                            total_layers: Some(l.total_layers),
                            supports_inference: self.network.inference_port.is_some(),
                        }]
                    })
                    .unwrap_or_default(),
                connection_verified: false,
                disk_total_mb: 0,
                disk_free_mb: 0,
                api_keys_active: 0,
                endpoint_id: self.network.endpoint_id.clone(),
                endpoint_addrs: self.network.endpoint_addrs(),
                tp: self.network.tp_identity(),
                runtime_capacity: local.and_then(|_| self.network.runtime_capacity()),
                idle: self.network.is_idle(),
            });
        }
        peers
    }

    /// Capacity of this process. After the local `/health` confirms the
    /// engine, it uses the effective value that llama.cpp reported; before that it falls back
    /// to the configured target to keep tests and local validation honest.
    pub(super) async fn local_runtime_capacity(&self) -> Option<RuntimeCapacity> {
        if let Some(capacity) = self.network.runtime_capacity() {
            return Some(capacity);
        }
        let runtime = self.runtime.config().await?;
        Some(RuntimeCapacity {
            n_ctx: runtime.n_ctx,
            max_new_tokens: runtime.max_new_tokens,
        })
    }

    /// Lowest capacity among the peers of the planned pipeline that can serve
    /// the selector. Idle peers of that pipeline are woken by the generation path
    /// before the first hop.
    ///
    /// Generation crosses all the stages of the pipeline, so announcing the
    /// largest peer would be a lie when another is still at 8192. A peer
    /// without a capacity announcement is an old node or one still initializing; in that
    /// case the local configuration remains the conservative fallback only
    /// when there is no corresponding peer. Only a non-idle peer of the pipeline
    /// without an announcement leaves the request pending: accepting with the local capacity
    /// could hide an old stage in the pipeline.
    pub(super) async fn runtime_capacity_for_model(
        &self,
        model: Option<&str>,
        preferred_head: Option<&str>,
    ) -> (Option<RuntimeCapacity>, bool) {
        let Some(model) = model else {
            return (self.local_runtime_capacity().await, false);
        };
        let peers = self.all_peers().await;
        let Some(planned) = self
            .planned_peers_for_model(&peers, model, preferred_head)
            .await
        else {
            return (None, false);
        };
        let (capacity, pending) = runtime_capacity_from_peers(&planned, model);
        if capacity.is_some() || pending {
            return (capacity, pending);
        }
        (self.local_runtime_capacity().await, false)
    }

    /// The capacity gate belongs to the route the planner will actually use,
    /// not to every live announcer carrying the same model label. Alternative
    /// routes and unrelated deployments must not make a valid request look
    /// unready.
    pub(super) async fn planned_peers_for_model(
        &self,
        peers: &[PeerInfo],
        model: &str,
        preferred_head: Option<&str>,
    ) -> Option<Vec<PeerInfo>> {
        let (model_name, deploy_id) = crate::deployment::split_selector(model);
        let candidates = crate::pipeline::model_pipeline_candidates(peers, model_name, deploy_id);
        let total_layers = candidates
            .iter()
            .map(|peer| peer.total_layers)
            .max()
            .filter(|total| *total > 0)?;
        let rtts = self
            .network
            .rtts
            .fresh_snapshot()
            .into_iter()
            .map(|measurement| (measurement.target_node_id, measurement.rtt_ms))
            .collect::<std::collections::HashMap<_, _>>();
        let plan = crate::pipeline::plan_pipeline_with_rtt_from_head(
            peers,
            model_name,
            deploy_id,
            total_layers,
            &self.loads,
            &rtts,
            preferred_head,
        )
        .await?;
        let planned_ids = plan
            .steps
            .iter()
            .map(|step| step.node_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        Some(
            peers
                .iter()
                .filter(|peer| planned_ids.contains(peer.node_id.as_str()))
                .cloned()
                .collect(),
        )
    }
}

/// Host that the other nodes must dial to reach this one.
///
/// The order matters and is the reason for the 2026-08-13 defect: the announced address
/// comes first because it is the only one that counts outside the LAN. Falling back to
/// the local IP is correct in a single-network cluster, and the loopback is the last resort, good
/// only for a node alone on its own machine.
pub(super) fn self_dial_host(advertised: Option<String>, local_ip: Option<String>) -> String {
    advertised
        .or(local_ip)
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// Reduces a snapshot of peers to the capacity of the selected model.
/// Keeping this function over a slice lets `/v1/models` use exactly
/// the same snapshot to discover IDs and publish its limits.
pub(super) fn runtime_capacity_from_peers(
    peers: &[PeerInfo],
    model: &str,
) -> (Option<RuntimeCapacity>, bool) {
    let (label, deploy_id) = crate::deployment::split_selector(model);
    let matching: Vec<&PeerInfo> = peers
        .iter()
        .filter(|peer| {
            peer.is_alive()
                && peer.model == label
                && deploy_id.is_none_or(|id| peer.deploy_id == id)
        })
        .collect();
    let capacities: Vec<RuntimeCapacity> = matching
        .iter()
        .filter_map(|peer| peer.runtime_capacity)
        .collect();
    if matching.is_empty() {
        return (None, false);
    }
    if matching
        .iter()
        .any(|peer| peer.runtime_capacity.is_none() && !peer.idle)
    {
        return (None, true);
    }
    (
        capacities.into_iter().reduce(|a, b| RuntimeCapacity {
            n_ctx: a.n_ctx.min(b.n_ctx),
            max_new_tokens: a.max_new_tokens.min(b.max_new_tokens),
        }),
        false,
    )
}

#[cfg(test)]
mod pkarr_record_tests {
    use super::{record_addrs, PublishBackoff, PKARR_CHECK_INTERVAL, PKARR_REFRESH};
    use std::time::Instant;

    /// Consecutive failures double the wait up to the cap and only powers of
    /// two warn; a success resets everything.
    #[test]
    fn publish_failures_back_off_and_stay_quiet() {
        let mut backoff = PublishBackoff::default();
        let now = Instant::now();
        assert!(backoff.ready(now));

        let (n, wait, loud) = backoff.failed(now);
        assert_eq!((n, wait, loud), (1, PKARR_CHECK_INTERVAL, true));
        assert!(!backoff.ready(now));
        assert!(backoff.ready(now + wait));

        let (_, wait, loud) = backoff.failed(now);
        assert_eq!((wait, loud), (PKARR_CHECK_INTERVAL * 2, true));
        let (_, _, loud) = backoff.failed(now);
        assert!(!loud, "3rd failure does not warn");
        for _ in 0..20 {
            backoff.failed(now);
        }
        let (_, wait, _) = backoff.failed(now);
        assert_eq!(wait, PKARR_REFRESH, "the wait is capped");

        assert_eq!(backoff.succeeded(), Some(24));
        assert!(backoff.ready(now));
        assert_eq!(backoff.succeeded(), None);
    }

    /// A relay URL passes as a relay, a private address does not go to the
    /// DHT, and a direct public address passes as is.
    #[test]
    fn the_record_keeps_public_addrs_and_relay_urls() {
        let (direct, relays) = record_addrs(&[
            "203.0.113.7:52777".into(),
            "192.168.0.10:52777".into(),
            "100.122.132.97:52777".into(),
            "[2001:db8::1]:52777".into(),
            "https://relay.example.org/".into(),
            "not an address".into(),
        ]);
        assert_eq!(
            direct,
            vec![
                "203.0.113.7:52777".to_string(),
                "[2001:db8::1]:52777".to_string(),
            ]
        );
        assert_eq!(relays, vec!["https://relay.example.org/".to_string()]);
    }
}

/// The model most live serving peers run (ties by name), or empty.
pub(crate) fn network_model(peers: &[crate::network::PeerInfo]) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for peer in peers {
        if peer.is_alive()
            && !peer.model.is_empty()
            && (peer.runtime_capacity.is_some() || peer.idle)
        {
            *counts.entry(peer.model.as_str()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(left.0)))
        .map(|(model, _)| model.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod network_model_tests {
    use super::network_model;

    #[test]
    fn a_node_serving_nothing_shows_the_model_the_network_serves() {
        let serving = |id: &str, model: &str| {
            let mut peer = crate::gateway::testkit::peer(id, id, 8080);
            peer.model = model.to_string();
            peer.idle = true;
            peer
        };
        let mut idle_other = crate::gateway::testkit::peer("x", "x", 8080);
        idle_other.model.clear();
        let peers = vec![
            serving("a", "Ornith"),
            serving("b", "Ornith"),
            serving("c", "Qwen"),
            idle_other,
        ];
        assert_eq!(network_model(&peers), "Ornith");
        assert_eq!(network_model(&[]), "");
    }
}
