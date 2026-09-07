use super::*;

/// Tokens `/api/generate` will produce before stopping on its own.
pub const DEFAULT_MAX_TOKENS: u32 = 512;
/// A peer that cannot answer its inference health endpoint is not eligible for
/// a new pipeline. UDP discovery is deliberately lightweight and may still
/// hear an isolated or old node, so it is not sufficient proof of a usable hop.
pub(super) const PIPELINE_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Characters per token, used only to size up a prompt before sending it.
/// Deliberately pessimistic — the real tokenizer lives in the first peer's
/// llama.cpp context, and under-estimating here would let through the very
/// request this guard exists to stop.
pub(super) const CHARS_PER_TOKEN: usize = 3;

/// Prazo do `/v1/session/rewind`. Curto de propósito: o evento final do stream
/// espera por ele (ver `generate_stream_inner`), e do outro lado é só apagar
/// uma sequência de KV.
pub(super) const REWIND_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// Prefixos de conversas que continuam vivos entre turnos. A entrada só
    /// guarda uma sessão KV já existente, nunca pesos ou estado oculto.
    pub prefix_cache: Arc<crate::prefix_cache::PrefixCache>,
    /// Índice em disco dos snapshots automáticos de prefixo (`--slot-save-path`
    /// + `slot_persist_prefix_cache`). `None` quando o recurso está desligado.
    /// Ver `crate::prefix_cache::PrefixDiskIndex` e `build_prefix_disk_index`.
    pub prefix_disk: Option<Arc<crate::prefix_cache::PrefixDiskIndex>>,
    /// Clientes HTTP de saída, **um por peer**, com o certificado do destino
    /// fixado pelo node_id (`peer_client::PeerClients`).
    ///
    /// Aqui havia também um `http` compartilhado e sem pinning. Ele saiu de
    /// propósito: com o cluster em TLS ele não tem como verificar o certificado
    /// de ninguém, e todo ponto de chamada que o pegasse por hábito falhava
    /// calado - foi assim que a tela de implantar parou de enxergar as fatias
    /// dos peers. Para destino que não é peer (loopback, Ollama) use
    /// `peer_http.plain()`, que diz o que está fazendo.
    pub peer_http: Arc<crate::peer_client::PeerClients>,
    /// Mutable node configuration driven by the web UI.
    pub runtime: Arc<NodeRuntime>,
    /// Cycle policy used before a deployment exists. A loaded deployment's
    /// `NodeConfig` takes precedence, so this is also the CLI fallback for a
    /// gateway-only process.
    pub cycle_config: crate::cycle::CycleConfig,
    /// Durable audit trail for requests entering this gateway.
    pub audit: Arc<crate::reqlog::AuditLog>,
    /// Single durable store for local operational state.
    pub database: Arc<crate::db::PomDb>,
    /// HTTP port this node serves the API and UI on.
    pub gateway_port: u16,
    /// Setup-level configuration (role, connection key, bootstrap nodes).
    pub auth: AuthState,
    /// Long-lived identity used to sign administrator-only profile commands.
    pub admin_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// In-memory browser sessions. It is intentionally not persisted beside
    /// the configuration and is bound to the current admin access token.
    pub(crate) admin_sessions: crate::auth::AdminSessionState,
    /// In-flight slice transfers and the offers waiting for an answer.
    pub transfers: crate::transfer::Transfers,
    /// Frames this node dispatched into a pipeline and is still waiting on.
    ///
    /// Only used by the direct-reply relay (`relay/v2-direct`), where the tail
    /// answers this process instead of the answer bubbling back through every
    /// hop — see [`crate::direct`].
    pub reply_inbox: crate::direct::ReplyInbox,
    /// Dispatches a hop-to-hop call over the libp2p swarm instead of plain
    /// HTTP, as a NAT-tunnel fallback — see `relay::send_frame_or_tunnel`.
    /// `None` whenever this node's own libp2p discovery never came up
    /// (disabled or failed to start); every prefill/decode call site treats
    /// that exactly like "the fallback wasn't available this time."
    pub tunnel_tx: Option<crate::infer_tunnel::TunnelOutboundSender>,
    /// Certificado deste nó e a política de texto claro, quando o binário
    /// conseguiu derivá-los da identidade. `None` só nos servidores de teste,
    /// que sobem em `127.0.0.1:0` e não têm par de chaves a apresentar.
    pub tls_server: Option<crate::serve::TlsServer>,
    /// Captura, transporte e persistência dos logs de processo do cluster.
    pub cluster_logs: Arc<crate::cluster_logs::ClusterLogs>,
    /// Retrato curto do cluster, isolado por gateway e por instância de teste.
    pub(crate) cluster_cache: Arc<Mutex<ClusterSnapshotCache>>,
    /// Provider de modelos remotos e seus jobs de download.
    pub(crate) model_service: Arc<crate::model_provider_backend::ModelService>,
}

#[derive(Serialize)]
pub(super) struct AdminAccessStatus {
    pub(super) admin_exposure: crate::config::AdminExposure,
    pub(super) requires_token: bool,
    pub(super) gateway_port: u16,
}

pub(super) async fn admin_access_status(State(state): State<GatewayState>) -> impl IntoResponse {
    let cfg = state.auth.read().await;
    Json(AdminAccessStatus {
        admin_exposure: cfg.admin_exposure,
        requires_token: cfg.admin_exposure.requires_token(),
        gateway_port: cfg.gateway_port,
    })
}

/// Routes used by inference or by the public compatibility API. This list is
/// the existing pipeline boundary from `20dfe12`; the UI gate must never be
/// applied to it, even when the node is in localhost-only mode.
pub(super) fn is_pipeline_or_public_path(path: &str) -> bool {
    path == "/api/access"
        || path == "/api/generate"
        || path == "/generate"
        || path == "/peers"
        || path == "/health"
        || path == "/handshake"
        || path == crate::tls_policy::TLS_POLICY_PATH
        || path == crate::cluster_logs_policy::CLUSTER_LOGS_POLICY_PATH
        || path.starts_with("/api/internal/")
        || path.starts_with("/api/transfer/")
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

pub(super) async fn require_admin_for_surface(
    State(state): State<GatewayState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    if !is_admin_surface_path(&path) {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    if is_public_ui_static_path(&path) {
        let network_exposure = state.auth.read().await.admin_exposure.requires_token();
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
    let mut response = next.run(Request::from_parts(parts, body)).await;
    if let Some(session_id) = auth.session_id_to_set {
        for path in crate::auth::ADMIN_SESSION_COOKIE_PATHS {
            let value = crate::auth::session_cookie_header(&session_id, secure, path);
            if let Ok(value) = HeaderValue::from_str(&value) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
    }
    response
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
pub(super) async fn ready_for_pipeline(
    http: &crate::peer_client::PeerClients,
    peers: Vec<PeerInfo>,
    timeout: Duration,
) -> Vec<PeerInfo> {
    futures::future::join_all(peers.into_iter().map(|peer| {
        // Um cliente por peer: a sonda tem que falar a mesma língua que a
        // inferência vai falar com aquele nó, senão ela mede - ou recusa - um
        // caminho que a requisição de verdade não percorre.
        let cliente = http.for_peer(&peer.node_id);
        async move {
            let Some(base) = peer.inference_url() else {
                return None;
            };
            let Ok(cliente) = cliente else {
                return None;
            };
            let url = format!("{base}/health");
            match tokio::time::timeout(timeout, cliente.get(url).send()).await {
                Ok(Ok(response)) if response.status().is_success() => Some(peer),
                _ => None,
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

impl GatewayState {
    /// Model label the pipeline planner matches on right now.
    pub async fn active_model(&self) -> String {
        self.runtime.model_label(&self.default_model).await
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

    /// Endereços por onde um peer alcança **este** nó, na ordem de preferência.
    ///
    /// Mesmo insumo que o nó já anuncia de si (`self_dial_host` primeiro, os
    /// candidatos enumerados depois): é o que a cauda de um pipeline usa para
    /// devolver o resultado direto, e errar aqui é a diferença entre uma
    /// resposta e um prazo estourado.
    pub fn self_reply_hosts(&self) -> Vec<String> {
        let mut hosts = vec![self_dial_host(
            self.network.effective_host(),
            crate::pkarr_node::detect_local_ip(),
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
            // Não é `127.0.0.1`: esta entrada viaja como está dentro de
            // `next_hops` para o repasse em anel, e um hop de outra máquina
            // discando "127.0.0.1" alcança a si mesmo, não a nós. O modo hub
            // discando a própria máquina pelo IP real em vez do loopback
            // continua funcionando (o servidor de inferência liga em
            // `0.0.0.0`), então não há troca a fazer aqui.
            //
            // E não é o IP da LAN quando existe `advertise_addr`: entre sites,
            // um `192.168.x.y` é tão inalcançável para o outro lado quanto o
            // loopback. O sintoma era mudo e assimétrico - com este nó de
            // cabeça o pipeline ia, porque o gateway falava consigo mesmo pela
            // LAN; com ele de sucessor, o hop remoto recebia um endereço
            // privado e a requisição morria sem chegar a lugar nenhum.
            // `effective_host` já vem com colchetes quando é IPv6. Pinado
            // pelo operador vence; sem pin, o endereço público atual da
            // interface vence o endereço antigo observado por `identify`.
            // O observado continua sendo fallback para NAT.
            let self_host = self_dial_host(
                self.network.effective_host(),
                crate::pkarr_node::detect_local_ip(),
            );
            peers.push(PeerInfo {
                deploy_id: local
                    .as_ref()
                    .map(|assignment| assignment.deploy_id.clone())
                    .unwrap_or_default(),
                node_id: self.network.node_id.clone(),
                tls: self.network.tls_enabled(),
                pubkey: self.network.pubkey.clone(),
                addr: format!("{}:{}", self_host, self.network.bind_addr.port()),
                addr_candidates: self.network.self_addr_candidates(),
                model: local.as_ref().map(|l| l.model.clone()).unwrap_or_default(),
                start_layer: local.as_ref().map(|l| l.start_layer).unwrap_or(0),
                end_layer: local.as_ref().map(|l| l.end_layer).unwrap_or(0),
                total_layers: local.as_ref().map(|l| l.total_layers).unwrap_or(0),
                memory_mb: local.as_ref().map(|l| l.memory_used_mb).unwrap_or(0.0),
                last_seen: chrono::Utc::now(),
                ollama_port: self.network.ollama_port,
                inference_port: self.network.inference_port,
                // Keep in sync with network.rs's `self_announce` and
                // infer_server.rs's health/announce builder, and with the
                // capability constants in routing/pipeline.rs.
                inference_frame_capabilities: {
                    let mut caps = vec![
                        crate::pipeline::CAP_FRAME_BATCH.into(),
                        crate::pipeline::CAP_FRAME_DTYPE_F8.into(),
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
                    if self.network.libp2p_tunnel_ready() {
                        caps.push("transport/libp2p-tunnel-v1".into());
                    }
                    caps
                },
                gateway_port: Some(self.gateway_port),
                role: self.network.role_label(),
                hostname: self.network.hostname.clone(),
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
                libp2p_peer_id: self.network.libp2p_peer_id.clone(),
                tp: self.network.tp_identity(),
                runtime_capacity: local.and_then(|_| self.network.runtime_capacity()),
            });
        }
        peers
    }

    /// Capacidade deste processo. Depois que o `/health` local confirma o
    /// motor, usa o valor efetivo que o llama.cpp reportou; antes disso cai no
    /// alvo configurado para manter os testes e a validação local honestos.
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

    /// Menor capacidade entre os peers vivos que podem atender o seletor.
    ///
    /// A geração atravessa todos os estágios do pipeline, então anunciar o
    /// maior peer seria uma mentira quando outro ainda está em 8192. Um peer
    /// sem anúncio de capacidade é um nó antigo ou ainda inicializando; nesse
    /// caso a configuração local continua sendo o fallback conservador só
    /// quando não há peer correspondente. Se há peer vivo sem anúncio, a
    /// requisição fica pendente: aceitar com a capacidade local poderia
    /// esconder um estágio antigo no pipeline.
    pub(super) async fn runtime_capacity_for_model(
        &self,
        model: Option<&str>,
    ) -> (Option<RuntimeCapacity>, bool) {
        let Some(model) = model else {
            return (self.local_runtime_capacity().await, false);
        };
        let peers = self.all_peers().await;
        let (capacity, pending) = runtime_capacity_from_peers(&peers, model);
        if capacity.is_some() || pending {
            return (capacity, pending);
        }
        (self.local_runtime_capacity().await, false)
    }
}

/// Host que os outros nós devem discar para chegar a este.
///
/// A ordem importa e é a razão do defeito de 2026-08-13: o endereço anunciado
/// vem primeiro porque é o único que vale fora da LAN. Cair para o IP local é
/// correto num cluster de uma rede só, e o loopback é o último recurso, bom
/// apenas para um nó sozinho na própria máquina.
pub(super) fn self_dial_host(advertised: Option<String>, local_ip: Option<String>) -> String {
    advertised
        .or(local_ip)
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// Reduz uma fotografia de peers para a capacidade do modelo selecionado.
/// Manter essa função sobre um slice permite que `/v1/models` use exatamente
/// o mesmo snapshot para descobrir IDs e publicar seus limites.
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
    if capacities.len() != matching.len() {
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
