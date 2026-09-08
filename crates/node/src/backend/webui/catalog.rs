use super::*;
// ── Node view ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct NodeInfo {
    pub node_id: String,
    pub short_id: String,
    pub hostname: String,
    /// Commit this binary was built from, so "which build is that peer running?"
    /// is answerable without a shell on the machine.
    pub build_commit: String,
    /// Este nó aceita atualizar-se pela interface. A tela só desenha o botão
    /// quando isto é verdade, para nunca oferecer o que vai dar 403.
    pub dev_update: bool,
    pub profile: crate::profile::NodeProfile,
    pub role: String,
    pub connection_key: String,
    pub p2p_port: u16,
    /// Endereços UDP anunciados pelo nó, necessários para o plano reconstruir
    /// as multiaddresses TCP do túnel no processo que executar o hop.
    pub addr_candidates: Vec<String>,
    /// Identidade e capabilities do túnel libp2p anunciadas por este nó.
    pub libp2p_peer_id: Option<String>,
    pub inference_frame_capabilities: Vec<String>,
    pub gateway_port: u16,
    pub admin_exposure: AdminExposure,
    pub admin_access_token_configured: bool,
    pub admin_access_token_masked: Option<String>,
    pub inference_port: Option<u16>,
    pub config: Option<NodeConfig>,
    pub status: RuntimeStatus,
    pub system: SystemLoad,
    pub peer_count: usize,
    pub load: u32,
    pub active_model: String,
    pub active_deployment: String,
    /// True while the operator has taken this node off the network.
    pub detached: bool,
    /// Raw operator-pinned address (`Config.advertise_addr`), for pre-filling
    /// the edit form — `None` means "nothing pinned," not "unknown."
    pub advertise_addr: Option<String>,
    /// What this node actually puts in `Announce.addr` right now
    /// (`Network::announce_addr()`, full `host:port`): the pin if set, else
    /// a peer-observed public address, else the `0.0.0.0:<port>` "ask the
    /// receiver" sentinel. Always present — lets the UI show the fix taking
    /// effect without a second round trip.
    pub effective_address: String,
    /// Se o cluster fala TLS entre peers agora.
    pub tls_required: bool,
    /// Se **este** nó pode mudar isso - só o principal pode. A tela usa para
    /// decidir entre interruptor e leitura; a checagem que vale é a do
    /// `save_tls`, no servidor.
    pub tls_authority: bool,
    /// Se este nó recolhe e envia logs de processo do cluster.
    pub cluster_logs_enabled: bool,
    /// Só o principal pode mudar a política de logs.
    pub cluster_logs_authority: bool,
    pub thermal: ThermalPolicy,
}

pub(super) async fn node_info(State(state): State<GatewayState>) -> Json<NodeInfo> {
    let system = sysmon::sample().await;
    let config = state.runtime.config().await;
    let active_deployment = config
        .as_ref()
        .map(|cfg| cfg.deployment_name.clone())
        .unwrap_or_default();
    let (
        profile,
        role,
        conn_key,
        advertise_addr,
        tls_required,
        tls_authority,
        cluster_logs_authority,
        thermal,
        admin_exposure,
        admin_access_token_configured,
        admin_access_token_masked,
    ) = {
        let cfg = state.auth.read().await;
        (
            cfg.profile,
            cfg.role.to_string(),
            cfg.connection_key.clone(),
            cfg.advertise_addr.clone(),
            cfg.tls_required,
            cfg.role == crate::config::Role::Principal,
            cfg.role == crate::config::Role::Principal,
            cfg.thermal,
            cfg.admin_exposure,
            cfg.admin_access_token
                .as_deref()
                .is_some_and(|token| !token.is_empty()),
            cfg.admin_access_token.as_deref().map(mask_access_token),
        )
    };
    Json(NodeInfo {
        short_id: state.network.node_id[..state.network.node_id.len().min(12)].to_string(),
        node_id: state.network.node_id.clone(),
        // Not `system.hostname`: that's the raw OS hostname, and ignores
        // `node_name` from pom.json. `network.hostname` is what
        // `with_identity` (main.rs) actually announces to peers — the same
        // value `/admin/nodes` already shows correctly on the Network tab.
        hostname: state.network.hostname.clone(),
        build_commit: crate::BUILD_COMMIT.to_string(),
        dev_update: crate::devupdate::enabled(),
        profile,
        role,
        connection_key: conn_key,
        p2p_port: state.network.bind_addr.port(),
        addr_candidates: state.network.self_addr_candidates(),
        libp2p_peer_id: state.network.libp2p_peer_id.clone(),
        inference_frame_capabilities: {
            let mut capabilities = vec![
                crate::pipeline::CAP_FRAME_BATCH.to_string(),
                crate::pipeline::CAP_FRAME_DTYPE_F8.to_string(),
                crate::pipeline::CAP_LINK_STREAM.to_string(),
                "relay/v1-chain".to_string(),
                "relay/v2-direct".to_string(),
                "speculate/v1-lookup".to_string(),
                "speculate/v1-mtp-sliced".to_string(),
            ];
            if state.network.mtp_ready() {
                capabilities.push("speculate/v1-mtp".to_string());
            }
            if state.network.batching_ready() {
                capabilities.push(crate::pipeline::CAP_FRAME_BATCH.to_string());
            }
            if state.network.libp2p_tunnel_ready() {
                capabilities.push(crate::pipeline::CAP_LIBP2P_TUNNEL.to_string());
            }
            capabilities
        },
        gateway_port: state.gateway_port,
        admin_exposure,
        admin_access_token_configured,
        admin_access_token_masked,
        inference_port: state.network.inference_port,
        config,
        status: state.runtime.status().await,
        peer_count: if profile.allows(crate::profile::ProfileAction::ViewNetworkPeers) {
            state.network.alive_peers().await.len()
        } else {
            0
        },
        load: state.loads.load(&state.network.node_id).await,
        active_model: state.active_model().await,
        active_deployment,
        detached: state.network.is_detached(),
        advertise_addr,
        effective_address: state.network.announce_addr(),
        tls_required,
        tls_authority,
        cluster_logs_enabled: state.network.cluster_logs_enabled(),
        cluster_logs_authority,
        thermal,
        system,
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct ClusterLogsQuery {
    pub(super) deploy_id: Option<String>,
    pub(super) limit: Option<usize>,
}

pub(super) async fn cluster_logs(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<ClusterLogsQuery>,
) -> Response {
    if state.auth.read().await.role != crate::config::Role::Principal {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "os logs do cluster só estão disponíveis no nó principal." })),
        )
            .into_response();
    }
    match state
        .cluster_logs
        .list(query.deploy_id.as_deref(), query.limit)
        .await
    {
        Ok(logs) => Json(logs).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não deu para ler os logs: {error}") })),
        )
            .into_response(),
    }
}

pub(super) fn mask_access_token(token: &str) -> String {
    let suffix = token.get(token.len().saturating_sub(4)..).unwrap_or("");
    format!("••••••••{suffix}")
}

#[derive(Debug, Serialize)]
pub struct AdminAccessInfo {
    pub admin_exposure: AdminExposure,
    pub gateway_port: u16,
    /// Só a superfície já autenticada recebe o token persistido. O endpoint
    /// público `/api/access` informa apenas se a catraca está ligada.
    pub access_token: Option<String>,
}

pub(super) async fn admin_access_info(State(state): State<GatewayState>) -> Json<AdminAccessInfo> {
    let cfg = state.auth.read().await;
    Json(AdminAccessInfo {
        admin_exposure: cfg.admin_exposure,
        gateway_port: cfg.gateway_port,
        access_token: cfg
            .admin_access_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .map(str::to_string),
    })
}

#[derive(Debug, Deserialize)]
pub struct SaveAdminAccessRequest {
    pub admin_exposure: AdminExposure,
    pub gateway_port: u32,
    #[serde(default)]
    pub regenerate_token: bool,
}

/// Persiste a fronteira da interface administrativa. As rotas da pipeline não
/// são alteradas: o listener do gateway continua dual-stack e o servidor de
/// inferência segue ouvindo em IPv4/IPv6 para o fallback do túnel.
pub(super) async fn save_admin_access(
    State(state): State<GatewayState>,
    Json(req): Json<SaveAdminAccessRequest>,
) -> Response {
    let port = match u16::try_from(req.gateway_port) {
        Ok(port) => port,
        Err(_) => return bad_request("a porta da interface deve estar entre 1 e 65535"),
    };
    if let Err(error) = crate::config::validate_gateway_port(port) {
        return bad_request(error.to_string());
    }
    if let Err(error) =
        crate::gateway::ensure_gateway_port_available(port, Some(state.gateway_port)).await
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response();
    }

    let (restart, access_token, invalidate_sessions) = {
        let mut cfg = state.auth.write().await;
        let mut next = cfg.clone();
        next.admin_exposure = req.admin_exposure;
        next.gateway_port = port;

        if req.admin_exposure.requires_token() {
            let should_regenerate = req.regenerate_token
                || next.admin_access_token.as_deref().is_none_or(str::is_empty);
            if should_regenerate {
                next.admin_access_token = Some(new_admin_access_token());
            }
        } else {
            // Leaving the network invalidates the old credential immediately.
            next.admin_access_token = None;
        }

        if let Err(error) = next.save() {
            return bad_request(format!(
                "não deu para gravar {}: {error}",
                crate::brand::config_filename()
            ));
        }
        let restart = port != state.gateway_port;
        let access_token = next.admin_access_token.clone();
        let invalidate_sessions = cfg.admin_exposure != next.admin_exposure
            || cfg.admin_access_token != next.admin_access_token;
        *cfg = next;
        (restart, access_token, invalidate_sessions)
    };

    if invalidate_sessions {
        state.admin_sessions.lock().await.invalidate_all();
    }

    info!(
        exposure = ?req.admin_exposure,
        gateway_port = port,
        restart,
        regenerated = req.regenerate_token,
        "preferência de acesso administrativo atualizada pela interface"
    );

    if restart {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            let error = crate::respawn::respawn_self();
            tracing::error!("respawn falhou, o nó segue no ar: {error}");
        });
    }

    Json(json!({
        "saved": true,
        "restart": restart,
        "admin_exposure": req.admin_exposure,
        "gateway_port": port,
        "access_token": access_token,
    }))
    .into_response()
}

pub(super) fn new_admin_access_token() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    hex::encode(Sha256::digest(raw.as_bytes()))
}

// ── Model catalog ────────────────────────────────────────────────────────────

/// Directories one scan covered: the whole-model libraries and the slice folder.
pub(super) type CatalogRoots = (Vec<std::path::PathBuf>, std::path::PathBuf);

pub(super) struct CachedCatalog {
    pub(super) at: Instant,
    /// Directories the cached scan covered. Point the configuration somewhere
    /// else and the old result stops being an answer to the new question.
    pub(super) roots: CatalogRoots,
    pub(super) catalog: Arc<Catalog>,
    /// Roots currently being scanned. The separate `enriching` marker stays set
    /// after this slot is released, until the header task publishes.
    pub(super) refreshing: Option<CatalogRoots>,
    /// Roots of the one metadata task currently running. A new inventory must
    /// reuse or reconcile this task instead of starting another one.
    pub(super) enriching: Option<CatalogRoots>,
    /// Latest roots requested while another scan is in flight.
    pub(super) pending_refresh: Option<CatalogRoots>,
}

pub(super) fn catalog_cache() -> &'static Mutex<Option<CachedCatalog>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<CachedCatalog>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

pub(super) fn normalize_catalog_roots(
    model_dirs: Vec<std::path::PathBuf>,
    shard_dir: std::path::PathBuf,
) -> CatalogRoots {
    let mut model_dirs: Vec<_> = model_dirs
        .into_iter()
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect();
    model_dirs.sort_unstable();
    model_dirs.dedup();
    let shard_dir = shard_dir.canonicalize().unwrap_or(shard_dir);
    (model_dirs, shard_dir)
}

/// Claim the sole background scan slot. A changed configuration is remembered
/// and scanned after the current walk, rather than starting another walk now.
pub(super) fn claim_refresh(cached: &mut CachedCatalog, roots: CatalogRoots) -> bool {
    if let Some(in_flight) = &cached.refreshing {
        if in_flight != &roots {
            cached.pending_refresh = Some(roots);
        }
        return false;
    }
    cached.refreshing = Some(roots);
    cached.pending_refresh = None;
    true
}

pub(super) fn claim_enrichment(cached: &mut CachedCatalog, roots: CatalogRoots) -> bool {
    if cached.enriching.is_some() {
        return false;
    }
    cached.enriching = Some(roots);
    true
}

pub(super) fn pending_metadata(catalog: &Catalog) -> Catalog {
    let mut pending = catalog.clone();
    pending
        .models
        .retain(|model| !catalog::metadata_is_current(model));
    catalog::rebuild_shard_sets(&mut pending);
    pending.metadata_complete = false;
    pending
}

/// Copy completed metadata into a newer inventory by filesystem identity.
/// Inventory refreshes necessarily create a new `Arc`, so pointer equality is
/// not a valid publication condition.
pub(super) fn reconcile_enriched_catalog(current: &mut Catalog, enriched: &Catalog) -> usize {
    let mut applied = 0;
    for source in &enriched.models {
        let Some(identity) = source.metadata_identity.as_ref() else {
            continue;
        };
        let Some(target) = current
            .models
            .iter_mut()
            .find(|target| catalog::current_metadata_identity(target).as_ref() == Some(identity))
        else {
            continue;
        };
        *target = source.clone();
        applied += 1;
    }
    catalog::rebuild_shard_sets(current);
    current.metadata_complete = current.models.iter().all(catalog::metadata_is_current);
    applied
}

/// Inventory the directories without opening GGUF headers.
pub(super) async fn scan_catalog(roots: &CatalogRoots) -> Catalog {
    let (dirs, shards) = (roots.0.clone(), roots.1.clone());
    tokio::task::spawn_blocking(move || catalog::discover_fast(&dirs, &shards))
        .await
        .unwrap_or_else(|e| {
            warn!("varredura de modelos falhou: {e}");
            Catalog {
                models: Vec::new(),
                shard_sets: Vec::new(),
                scanned: Vec::new(),
                metadata_complete: false,
            }
        })
}

/// Read GGUF headers off the reactor and return a complete catalog.
pub(super) async fn enrich_catalog(partial: Arc<Catalog>) -> Arc<Catalog> {
    let fallback = partial.clone();
    let input = (*partial).clone();
    match tokio::task::spawn_blocking(move || catalog::enrich(input)).await {
        Ok(catalog) => Arc::new(catalog),
        Err(e) => {
            warn!("enriquecimento do catálogo falhou: {e}");
            let mut catalog = (*fallback).clone();
            catalog.metadata_complete = true;
            for model in &mut catalog.models {
                model.metadata_identity = catalog::current_metadata_identity(model);
                model.metadata_error = Some(format!(
                    "a tarefa de enriquecimento do catálogo falhou: {e}"
                ));
            }
            Arc::new(catalog)
        }
    }
}

/// Publish header metadata, reconciling it with whichever inventory is current.
pub(super) fn spawn_enrichment(database: Arc<crate::db::PomDb>, roots: CatalogRoots, partial: Arc<Catalog>) {
    tokio::spawn(async move {
        let started = Instant::now();
        info!(
            "iniciando enriquecimento do catálogo - {} modelos, {} conjuntos de fatias",
            partial.models.len(),
            partial.shard_sets.len()
        );
        let enriched = enrich_catalog(partial.clone()).await;
        let next = {
            let mut guard = catalog_cache().lock().await;
            let Some(current) = guard.as_mut() else {
                warn!(
                    "enriquecimento do catálogo concluído em {:.1}s, mas descartado: cache ausente",
                    started.elapsed().as_secs_f32()
                );
                return;
            };
            if current.enriching.as_ref() != Some(&roots) {
                warn!(
                    "enriquecimento do catálogo concluído em {:.1}s, mas descartado: tarefa substituída",
                    started.elapsed().as_secs_f32()
                );
                return;
            }

            let applied =
                reconcile_enriched_catalog(Arc::make_mut(&mut current.catalog), &enriched);
            current.enriching = None;
            let pending = pending_metadata(&current.catalog);
            let failed = enriched
                .models
                .iter()
                .filter(|model| model.metadata_error.is_some())
                .count();
            let pending_count = pending.models.len();
            let next = if current.refreshing.is_none() && !pending.models.is_empty() {
                let roots = current.roots.clone();
                claim_enrichment(current, roots.clone()).then_some((roots, Arc::new(pending)))
            } else {
                None
            };
            info!(
                "metadados do catálogo concluídos em {:.1}s - {} aplicados, {} falhas, {} pendentes",
                started.elapsed().as_secs_f32(),
                applied,
                failed,
                pending_count
            );
            let snapshot = current.catalog.clone();
            (next, snapshot)
        };
        let (next, snapshot) = next;
        if snapshot.metadata_complete || !snapshot.models.is_empty() {
            let database_for_record = database.clone();
            match tokio::task::spawn_blocking(move || database_for_record.record_catalog(&snapshot))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    warn!("não foi possível guardar o histórico do catálogo: {error:#}");
                }
                Err(error) => {
                    warn!("tarefa do histórico do catálogo falhou: {error}");
                }
            }
        }
        if let Some((next_roots, next_partial)) = next {
            spawn_enrichment(database, next_roots, next_partial);
        }
    });
}

/// Rescan in the background and publish the result. Metadata already read for
/// unchanged files is copied into the new inventory before a new task is
/// considered.
pub(super) fn spawn_refresh(database: Arc<crate::db::PomDb>, roots: CatalogRoots) {
    tokio::spawn(async move {
        let scanned = scan_catalog(&roots).await;
        let (next_refresh, next_enrichment, snapshot) = {
            let mut guard = catalog_cache().lock().await;
            let Some(current) = guard.as_mut() else {
                warn!("varredura do catálogo concluída, mas descartada: cache ausente");
                return;
            };
            if current.refreshing.as_ref() != Some(&roots) {
                warn!("varredura do catálogo descartada: tarefa substituída");
                return;
            }

            let mut catalog = scanned;
            reconcile_enriched_catalog(&mut catalog, &current.catalog);
            current.at = Instant::now();
            current.roots = roots.clone();
            current.catalog = Arc::new(catalog);
            current.refreshing = None;

            let next_refresh = current.pending_refresh.take().map(|roots| {
                current.refreshing = Some(roots.clone());
                roots
            });
            let next_enrichment = if next_refresh.is_none() && current.enriching.is_none() {
                let pending = pending_metadata(&current.catalog);
                if pending.models.is_empty() {
                    Arc::make_mut(&mut current.catalog).metadata_complete = true;
                    None
                } else {
                    let roots = current.roots.clone();
                    claim_enrichment(current, roots.clone()).then_some((roots, Arc::new(pending)))
                }
            } else {
                None
            };
            (next_refresh, next_enrichment, current.catalog.clone())
        };
        if snapshot.metadata_complete || !snapshot.models.is_empty() {
            let database_for_record = database.clone();
            match tokio::task::spawn_blocking(move || database_for_record.record_catalog(&snapshot))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    warn!("não foi possível guardar o histórico do catálogo: {error:#}");
                }
                Err(error) => {
                    warn!("tarefa do histórico do catálogo falhou: {error}");
                }
            }
        }
        if let Some(next_roots) = next_refresh {
            spawn_refresh(database.clone(), next_roots);
        }
        if let Some((next_roots, next_partial)) = next_enrichment {
            spawn_enrichment(database, next_roots, next_partial);
        }
    });
}

/// Ask for a fresh walk without throwing away what we can already answer with —
/// after slicing, after the directories change. A slice that exists must not
/// need a restart to show up.
pub(crate) async fn invalidate_catalog(database: Arc<crate::db::PomDb>) {
    let roots = {
        let mut guard = catalog_cache().lock().await;
        let Some(cached) = guard.as_mut() else {
            return;
        };
        let roots = cached.roots.clone();
        claim_refresh(cached, roots.clone()).then_some(roots)
    };
    if let Some(roots) = roots {
        spawn_refresh(database, roots);
    }
}

/// The catalog of this machine, served from cache while it is refreshed.
///
/// Scanning a slow volume is not cheap - the captain's SD card full of models
/// takes about two minutes the first time - and the screen polls this. Handing
/// back the previous answer and refreshing behind it keeps the list on screen
/// instead of blanking it every time the cache turns stale. The first request
/// installs an empty placeholder and shares the one background scan with all
/// concurrent requests, including F5 and model selection.
pub(super) async fn local_catalog(state: &GatewayState) -> Arc<Catalog> {
    let roots = normalize_catalog_roots(state.model_dirs().await, state.shard_dir().await);
    let (catalog, start_refresh) = {
        let mut guard = catalog_cache().lock().await;
        match guard.as_mut() {
            Some(cached) => {
                let stale = cached.roots == roots && cached.at.elapsed() >= CATALOG_TTL;
                let roots_changed = cached.roots != roots;
                let start_refresh =
                    (stale || roots_changed) && claim_refresh(cached, roots.clone());
                (cached.catalog.clone(), start_refresh)
            }
            None => {
                let catalog = Arc::new(Catalog {
                    models: Vec::new(),
                    shard_sets: Vec::new(),
                    scanned: Vec::new(),
                    metadata_complete: false,
                });
                let mut cached = CachedCatalog {
                    at: Instant::now(),
                    roots: roots.clone(),
                    catalog: catalog.clone(),
                    refreshing: None,
                    enriching: None,
                    pending_refresh: None,
                };
                let start_refresh = claim_refresh(&mut cached, roots.clone());
                *guard = Some(cached);
                (catalog, start_refresh)
            }
        }
    };
    if start_refresh {
        spawn_refresh(state.database.clone(), roots);
    }
    catalog
}

/// Acha, no catálogo desta máquina, o arquivo que uma faixa de uma implantação
/// deveria carregar - quando não há caminho vivo nem `part_sha256` para
/// resolver (registro antigo, ou nó que já estava fora do ar num `redeploy`).
///
/// Modelo inteiro: casa pelo nome de catálogo, como `deploy::parse_whole`.
/// Fatia: casa o grupo/rótulo da rodada e a faixa exata de camadas, como
/// `find_release_candidate`.
pub(super) async fn resolve_deployment_slice_file(
    state: &GatewayState,
    model_label: &str,
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    peer_count: u32,
) -> Option<String> {
    let catalog = local_catalog(state).await;
    let whole = peer_count <= 1 && start_layer == 0 && end_layer + 1 >= total_layers.max(1);
    if whole {
        return catalog
            .models
            .iter()
            .find(|m| !m.is_shard() && m.name == model_label)
            .map(|m| m.path.clone());
    }
    catalog
        .shard_sets
        .iter()
        .filter(|s| s.group == model_label || s.model_label == model_label)
        .flat_map(|s| &s.shards)
        .find(|shard| shard.shard_range == Some((start_layer, end_layer)))
        .map(|shard| shard.path.clone())
}

/// Schedule the inventory once at boot, so the first screen does not pay for
/// the walk or wait for GGUF header enrichment.
///
/// Measured on the captain's machine with the slices on an external volume: the
/// very first `/api/ui/models` took **11,9 s**, and the deploy screen's own
/// `/api/ui/cluster/models` took 13,2 s cold against 0,15 s once that scan had
/// happened - the fan-out to eleven peers was never the cost, the local walk
/// was. Nothing else changes: the same cache, the same TTL, the same
/// stale-while-revalidate. What moves is *when* the walk happens, from "the
/// first person to open a screen" to "while the node is still starting up".
///
/// Deliberately not awaited. A slow volume must not hold the gateway's port
/// closed, and a request that lands mid-walk receives the placeholder from the
/// same scan instead of starting another one.
pub fn warm_catalog(state: GatewayState) {
    tokio::spawn(async move {
        let started = Instant::now();
        let catalog = local_catalog(&state).await;
        info!(
            "catálogo inicial em {:.1}s - {} modelos, {} conjuntos de fatias; cabeçalhos em background",
            started.elapsed().as_secs_f32(),
            catalog.models.len(),
            catalog.shard_sets.len()
        );
    });
}

// ── Model directories ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct PathEntry {
    pub(super) path: String,
    pub(super) exists: bool,
    /// "ollama" | "lmstudio" | "folder" — what the scan will make of it.
    pub(super) kind: &'static str,
    pub(super) used_bytes: u64,
    pub(super) max_bytes: Option<u64>,
    pub(super) usage_percent: Option<f64>,
}

pub(super) fn path_entry(dir: &std::path::Path, max_bytes: Option<u64>) -> PathEntry {
    let used_bytes = crate::storage::directory_bytes(dir).unwrap_or(0);
    let usage_percent = max_bytes.map(|max| {
        crate::storage::StorageUsage {
            used_bytes,
            max_bytes: max,
        }
        .percent()
    });
    PathEntry {
        path: dir.to_string_lossy().into_owned(),
        exists: dir.is_dir(),
        kind: match catalog::detect_source(dir) {
            catalog::ModelSource::Ollama => "ollama",
            catalog::ModelSource::LmStudio => "lmstudio",
            _ => "folder",
        },
        used_bytes,
        max_bytes,
        usage_percent,
    }
}

/// Where this node looks for models and where it writes slices.
pub(super) async fn get_paths(State(state): State<GatewayState>) -> impl IntoResponse {
    let cfg = state.auth.read().await;
    let shard = cfg.shard_dir_path();
    let shard_max_bytes = cfg.shard_max_bytes();
    Json(json!({
        "model_dirs": cfg.model_dir_paths().iter().map(|d| path_entry(d, None)).collect::<Vec<_>>(),
        "shard_dir": path_entry(&shard, Some(shard_max_bytes)),
        "shard_max_gb": cfg.shard_max_gb(),
        "using_defaults": cfg.model_dirs_are_default(),
        "detected": catalog::default_model_dirs()
            .iter().map(|d| path_entry(d, None)).collect::<Vec<_>>(),
        "default_shard_dir": crate::config::default_shard_dir().to_string_lossy(),
    }))
}

pub(super) async fn get_thermal(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Json<ThermalPolicy> {
    Json(state.auth.read().await.thermal)
}

pub(super) async fn save_thermal(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(policy): Json<ThermalPolicy>,
) -> Response {
    if let Err(error) = policy.validate() {
        return bad_request(error.to_string());
    }
    if let Err(error) = state.database.save_thermal_policy(policy) {
        tracing::error!("não foi possível guardar a política térmica: {error:#}");
        return bad_request("não foi possível guardar a política térmica");
    }
    {
        let mut cfg = state.auth.write().await;
        cfg.thermal = policy;
    }
    info!(
        limit_c = policy.limit_c,
        hysteresis_c = policy.hysteresis_c,
        cooldown_secs = policy.cooldown_secs,
        "política térmica atualizada pela interface"
    );
    Json(policy).into_response()
}

/// The KV cache representation is an installation setting, not a deployment
/// or machine override. The deploy screen reads this separately so it cannot
/// accidentally put a global storage choice into one node's launch payload.
pub(super) async fn get_cache_types(State(state): State<GatewayState>) -> impl IntoResponse {
    let cfg = state.auth.read().await;
    Json(json!({
        "cache_type_k": cfg.cache_type_k,
        "cache_type_v": cfg.cache_type_v,
        "choices": KvCacheType::ALL.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct SaveCacheTypesRequest {
    pub cache_type_k: KvCacheType,
    pub cache_type_v: KvCacheType,
}

pub(super) async fn save_cache_types(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveCacheTypesRequest>,
) -> Response {
    {
        let mut cfg = state.auth.write().await;
        cfg.cache_type_k = req.cache_type_k;
        cfg.cache_type_v = req.cache_type_v;
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar pom.json: {e}"));
        }
    }
    info!(
        cache_type_k = req.cache_type_k.as_str(),
        cache_type_v = req.cache_type_v.as_str(),
        "tipos do cache KV atualizados pela interface"
    );
    get_cache_types(State(state)).await.into_response()
}

#[derive(Debug, Deserialize)]
pub struct SavePathsRequest {
    pub model_dirs: Vec<String>,
    /// Empty means "back to `<project_root>/models`".
    #[serde(default)]
    pub shard_dir: String,
    #[serde(default)]
    pub shard_max_gb: Option<f64>,
}

/// Refuse anything that is not a directory sitting on this machine right now.
///
/// A typo here is not a small thing: the node would advertise a library it does
/// not have, and the mistake would only surface much later, as a peer that
/// cannot find the model it was told to slice.
pub(super) fn check_dir(raw: &str, what: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{what}: caminho vazio."));
    }
    let path = std::path::Path::new(trimmed);
    if !path.exists() {
        return Err(format!("{what}: “{trimmed}” não existe nesta máquina."));
    }
    if !path.is_dir() {
        return Err(format!(
            "{what}: “{trimmed}” é um arquivo, e aqui se informa a pasta que o contém."
        ));
    }
    Ok(trimmed.to_string())
}

pub(super) fn reject_nested_shard_dir(model_dirs: &[String], shard_dir: &str) -> Result<(), String> {
    let shard = std::path::Path::new(shard_dir)
        .canonicalize()
        .map_err(|e| format!("Pasta de modelos fatiados: não deu para resolver o caminho: {e}"))?;
    for model in model_dirs {
        let model = std::path::Path::new(model).canonicalize().map_err(|e| {
            format!("Pasta de modelos inteiros: não deu para resolver {model}: {e}")
        })?;
        if shard == model || shard.starts_with(&model) {
            return Err(
                "a pasta de modelos fatiados não pode ser a mesma ou ficar dentro de uma pasta de modelos inteiros".to_string(),
            );
        }
    }
    Ok(())
}

pub(super) async fn save_paths(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SavePathsRequest>,
) -> Response {
    let mut model_dirs = Vec::new();
    for raw in &req.model_dirs {
        match check_dir(raw, "Pasta de modelos inteiros") {
            Ok(p) => {
                if !model_dirs.contains(&p) {
                    model_dirs.push(p);
                }
            }
            Err(e) => return bad_request(e),
        }
    }

    let shard_dir = if req.shard_dir.trim().is_empty() {
        None
    } else {
        match check_dir(&req.shard_dir, "Pasta de modelos fatiados") {
            Ok(p) => Some(p),
            Err(e) => return bad_request(e),
        }
    };

    let effective_shard_dir = shard_dir.clone().unwrap_or_else(|| {
        crate::config::default_shard_dir()
            .to_string_lossy()
            .into_owned()
    });
    if let Err(e) = reject_nested_shard_dir(&model_dirs, &effective_shard_dir) {
        return bad_request(e);
    }
    let shard_max_gb = match req.shard_max_gb {
        None => crate::storage::DEFAULT_SHARD_MAX_GB,
        Some(value) if value.is_finite() && value > 0.0 => value,
        Some(_) => {
            return bad_request("o limite da pasta de modelos fatiados precisa ser maior que zero")
        }
    };

    {
        let mut cfg = state.auth.write().await;
        cfg.model_dirs = Some(model_dirs);
        cfg.shard_dir = shard_dir;
        cfg.shard_max_gb = shard_max_gb;
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar pom.json: {e}"));
        }
    }

    invalidate_catalog(state.database.clone()).await;
    info!("pastas de modelos atualizadas pela interface");
    get_paths(State(state)).await.into_response()
}

#[derive(Debug, Deserialize)]
pub struct SaveAdvertiseAddrRequest {
    /// Empty means "clear the pin" — same convention as `SavePathsRequest.shard_dir`.
    #[serde(default)]
    pub advertise_addr: String,
}

/// Pin (or clear) the address this node advertises to peers, live — no
/// restart. See `Network::set_advertise_addr`'s doc comment for why this is
/// a live update rather than the `save_setup`-style respawn other config
/// changes use: a respawn re-execs with the *original* CLI argv, and
/// `--advertise-addr`/`--public-addr` always beat the config file value, so
/// it would be silently ineffective for a node ever launched with either
/// flag on its command line.
pub(super) async fn save_advertise_addr(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveAdvertiseAddrRequest>,
) -> Response {
    let raw = req.advertise_addr.trim();
    let normalized = if raw.is_empty() {
        None
    } else {
        match crate::network::advertised_addr(raw, state.network.bind_addr.port()) {
            Some(a) => Some(a),
            None => return bad_request(format!("“{raw}” não é um endereço válido.")),
        }
    };

    {
        let mut cfg = state.auth.write().await;
        cfg.advertise_addr = normalized.clone();
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar pom.json: {e}"));
        }
    }

    state.network.set_advertise_addr(normalized.as_deref());
    info!("endereço anunciado atualizado pela interface: {normalized:?}");

    Json(json!({
        "advertise_addr": normalized,
        "effective_address": state.network.announce_addr(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SaveTlsRequest {
    pub tls_required: bool,
}

/// Liga ou desliga o TLS entre peers, para o cluster inteiro.
///
/// Não precisa de reinício, e pode ser virado quantas vezes o operador quiser:
/// o servidor aceita as duas falas desde o arranque (`serve.rs`), e o que muda
/// aqui é qual delas os clientes usam e se texto claro remoto passa a ser
/// recusado. Os demais nós descobrem pela catraca de `tls_policy`, dentro de um
/// ciclo de gossip (5 s), nos dois sentidos.
pub(super) async fn save_tls(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveTlsRequest>,
) -> Response {
    // A autoridade é verificada **aqui**, no servidor. Esconder o interruptor
    // na tela de um nó comum é conveniência; isto é o controle.
    if state.auth.read().await.role != crate::config::Role::Principal {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "só o nó principal decide o TLS do cluster." })),
        )
            .into_response();
    }

    {
        let mut cfg = state.auth.write().await;
        cfg.tls_required = req.tls_required;
        if let Err(e) = cfg.save() {
            return bad_request(format!(
                "não deu para gravar {}: {e}",
                crate::brand::config_filename()
            ));
        }
    }

    // A ordem importa pouco, mas as três têm que acontecer juntas: o esquema
    // das URLs e o cache de clientes, o que o anúncio diz, e se o servidor
    // passa a recusar texto claro de fora.
    state.peer_http.apply_cluster_policy(req.tls_required);
    state.network.set_tls(req.tls_required);
    if let Some(t) = &state.tls_server {
        t.set_strict(req.tls_required);
    }
    info!(
        "TLS entre peers {} pela interface",
        if req.tls_required {
            "ligado"
        } else {
            "desligado"
        }
    );

    Json(json!({ "tls_required": req.tls_required })).into_response()
}

#[derive(Deserialize)]
pub struct SaveClusterLogsRequest {
    pub logs_enabled: bool,
}

/// Liga ou desliga a coleta do cluster sem reinício. O principal grava a
/// política e os demais nós a adotam apenas após a busca autenticada descrita
/// em `cluster_logs_policy`.
pub(super) async fn save_cluster_logs(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveClusterLogsRequest>,
) -> Response {
    if state.auth.read().await.role != crate::config::Role::Principal {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "só o nó principal decide os logs do cluster." })),
        )
            .into_response();
    }

    {
        let mut cfg = state.auth.write().await;
        cfg.cluster_logs_enabled = req.logs_enabled;
        if let Err(error) = cfg.save() {
            return bad_request(format!(
                "não deu para gravar {}: {error}",
                crate::brand::config_filename()
            ));
        }
    }

    state.network.set_cluster_logs_enabled(req.logs_enabled);
    state.cluster_logs.set_enabled(req.logs_enabled);
    info!(
        "logs de processo do cluster {} pela interface",
        if req.logs_enabled { "ligados" } else { "desligados" }
    );
    Json(json!({ "logs_enabled": req.logs_enabled })).into_response()
}

pub(super) async fn models(State(state): State<GatewayState>) -> impl IntoResponse {
    let catalog = local_catalog(&state).await;
    Json(json!({
        // Não `sysmon`: aquele é o hostname cru do sistema operacional e
        // ignora o `node_name` do pom.json. `network.hostname` é o nome que
        // este nó anuncia aos peers, e é o que o resto da tela mostra - ver a
        // nota gêmea em `node_info`. Quem lia daqui era o seletor "Enviar
        // para..." da tela de fatias, e ele era o único lugar do aplicativo
        // que chamava a máquina por outro nome.
        "hostname": state.network.hostname,
        "node_id": state.network.node_id,
        "models": catalog.models,
        "shard_sets": catalog.shard_sets,
        "scanned": catalog.scanned,
        "metadata_complete": catalog.metadata_complete,
    }))
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RttReport {
    pub(super) source_node_id: String,
    pub(super) measurements: Vec<crate::route::RttMeasurement>,
}

/// Publica apenas as medições feitas por este nó. A autenticação é necessária
/// porque o relatório faz parte da telemetria interna usada para escolher
/// rotas de inferência.
pub(super) async fn network_rtt(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Json<RttReport> {
    if !state
        .auth
        .read()
        .await
        .profile
        .allows(crate::profile::ProfileAction::ViewNetworkPeers)
    {
        return Json(RttReport {
            source_node_id: state.network.node_id.clone(),
            measurements: Vec::new(),
        });
    }
    let mut measurements = local_rtt_measurements(&state);
    measurements.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));

    Json(RttReport {
        source_node_id: state.network.node_id.clone(),
        measurements,
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatTelemetryRequest {
    #[serde(default)]
    pub(super) node_ids: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct ChatTelemetryMeasurement {
    pub(super) from_node_id: String,
    pub(super) to_node_id: String,
    pub(super) rtt_ms: f64,
    pub(super) transport: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ChatTelemetryResponse {
    pub(super) measurements: Vec<ChatTelemetryMeasurement>,
}

/// Combina a sonda local com os relatórios direcionais dos próprios peers.
///
/// A tela de implantação já usa esta mesma fonte para as arestas do plano,
/// mas o chat precisa de uma consulta sob demanda para a execução recém
/// terminada. Cada relatório é aceito somente quando o `source_node_id` bate
/// com o peer que respondeu: sem essa verificação, um gateway intermediário
/// poderia fazer uma medida parecer ser de outra máquina.
pub(super) async fn chat_telemetry(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(request): Json<ChatTelemetryRequest>,
) -> Json<ChatTelemetryResponse> {
    if !state
        .auth
        .read()
        .await
        .profile
        .allows(crate::profile::ProfileAction::ViewNetworkPeers)
    {
        return Json(ChatTelemetryResponse {
            measurements: Vec::new(),
        });
    }
    let requested: std::collections::HashSet<String> = request
        .node_ids
        .into_iter()
        .filter(|node_id| !node_id.is_empty())
        .collect();
    if requested.len() < 2 {
        return Json(ChatTelemetryResponse {
            measurements: Vec::new(),
        });
    }

    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;
    let connection_key = state.auth.read().await.connection_key.clone();
    let mut measurements = Vec::new();

    append_chat_telemetry_measurements(
        &mut measurements,
        &self_id,
        RttReport {
            source_node_id: self_id.clone(),
            measurements: local_rtt_measurements(&state),
        },
        &requested,
    );

    for peer in peers {
        if peer.node_id == self_id || !requested.contains(&peer.node_id) {
            continue;
        }
        let Some(base) = peer.gateway_url() else {
            continue;
        };
        let Ok(http) = state.peer_http.for_peer(&peer.node_id) else {
            continue;
        };
        let response = http
            .get(format!("{base}/api/ui/network/rtt"))
            .timeout(PEER_TIMEOUT)
            .bearer_auth(&connection_key)
            .send()
            .await;
        let Ok(response) = response else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(report) = response.json::<RttReport>().await else {
            continue;
        };
        append_chat_telemetry_measurements(&mut measurements, &peer.node_id, report, &requested);
    }

    measurements.sort_by(|a, b| {
        a.from_node_id
            .cmp(&b.from_node_id)
            .then_with(|| a.to_node_id.cmp(&b.to_node_id))
    });
    Json(ChatTelemetryResponse { measurements })
}

pub(super) async fn chat_history(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    match state.database.chat_sessions() {
        Ok(sessions) => Json(sessions).into_response(),
        Err(error) => {
            tracing::error!("não foi possível ler o histórico do chat: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível ler o histórico do chat" })),
            )
                .into_response()
        }
    }
}

pub(super) async fn save_chat_history(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(session): Json<crate::db::ChatSessionRecord>,
) -> Response {
    if let Err(error) = session.validate() {
        return bad_request(error.to_string());
    }
    if let Err(error) = state.database.save_chat_session(&session) {
        tracing::error!("não foi possível guardar o histórico do chat: {error:#}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "não foi possível guardar o histórico do chat" })),
        )
            .into_response();
    }
    Json(session).into_response()
}

pub(super) async fn delete_chat_history(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
) -> Response {
    match state.database.delete_chat_session(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!("não foi possível apagar a conversa: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível apagar a conversa" })),
            )
                .into_response()
        }
    }
}

pub(super) async fn clear_chat_history(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    match state.database.clear_chat_sessions() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!("não foi possível limpar o histórico do chat: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível limpar o histórico do chat" })),
            )
                .into_response()
        }
    }
}

pub(super) fn local_rtt_measurements(state: &GatewayState) -> Vec<crate::route::RttMeasurement> {
    let mut measurements = state.network.rtts.snapshot();
    let measured_targets: std::collections::HashSet<String> = measurements
        .iter()
        .map(|measurement| measurement.target_node_id.clone())
        .collect();
    for (node_id, route) in state.network.routes.snapshot() {
        if !measured_targets.contains(&node_id) {
            measurements.push(crate::route::RttMeasurement {
                target_node_id: node_id,
                rtt_ms: route.rtt_ms,
                transport: "direct-health".to_string(),
            });
        }
    }
    measurements
}

pub(super) fn append_chat_telemetry_measurements(
    output: &mut Vec<ChatTelemetryMeasurement>,
    expected_source: &str,
    report: RttReport,
    requested: &std::collections::HashSet<String>,
) {
    if report.source_node_id != expected_source {
        return;
    }
    output.extend(
        report
            .measurements
            .into_iter()
            .filter(|measurement| {
                requested.contains(&measurement.target_node_id)
                    && measurement.target_node_id != expected_source
            })
            .map(|measurement| ChatTelemetryMeasurement {
                from_node_id: expected_source.to_string(),
                to_node_id: measurement.target_node_id,
                rtt_ms: measurement.rtt_ms,
                transport: measurement.transport,
            }),
    );
}
