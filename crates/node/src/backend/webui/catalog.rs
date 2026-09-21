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
    /// Controls whether development-only POM deployment settings are exposed.
    pub pom_desenv: bool,
    pub profile: crate::profile::NodeProfile,
    /// Human-facing network name shown by the leave action in the main menu.
    pub network_name: String,
    /// A chave de conexão deste cluster. Não é enfeite de tela: a interface a
    /// mostra em "Chave de Conexão" **e** a guarda para autenticar as chamadas
    /// que exigem `ConnectionAuth` (`lib/queries.ts`, `useNode`). Ela é a
    /// mesma em todo o cluster e já está no `pom.json` desta máquina, e esta
    /// rota só é servida atrás da fronteira administrativa
    /// (`require_admin_for_surface`), então quem a lê já provou ser o
    /// operador desta máquina. Tirá-la daqui deixou o navegador sem
    /// credencial e a tela a ler `undefined.length`.
    pub connection_key: String,
    pub authority_level: Option<crate::admin_trust::AdminLevel>,
    pub authority_fingerprint: Option<String>,
    pub p2p_port: u16,
    /// Endereços UDP anunciados pelo nó, necessários para o plano reconstruir
    /// as multiaddresses TCP do túnel no processo que executar o hop.
    pub addr_candidates: Vec<String>,
    /// Identidade e capabilities do túnel libp2p anunciadas por este nó.
    pub libp2p_peer_id: Option<String>,
    pub inference_frame_capabilities: Vec<String>,
    pub gateway_port: u16,
    pub admin_exposure: AdminExposure,
    pub admin_password_configured: bool,
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
    /// Se o cluster prefere HTTP/3 sobre QUIC nos novos hops.
    pub quic_enabled: bool,
    /// Só o principal pode mudar a preferência QUIC.
    pub quic_authority: bool,
    /// Se este nó recolhe e envia logs de processo do cluster.
    pub cluster_logs_enabled: bool,
    /// Só o principal pode mudar a política de logs.
    pub cluster_logs_authority: bool,
    /// Se este nó aceita o canal global de conversa.
    pub network_chat_enabled: bool,
    /// Só o principal pode mudar a política do Chat da rede.
    pub network_chat_authority: bool,
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
        network_name,
        connection_key,
        authority_level,
        authority_fingerprint,
        advertise_addr,
        tls_required,
        quic_enabled,
        tls_authority,
        quic_authority,
        cluster_logs_authority,
        network_chat_authority,
        thermal,
        admin_exposure,
        admin_password_configured,
    ) = {
        let cfg = state.auth.read().await;
        (
            cfg.profile,
            cfg.network.public_name.clone(),
            cfg.connection_key.clone(),
            state
                .admin_trust
                .effective_authority(&state.authority_principal, cfg.profile)
                .map(|authority| match authority {
                    crate::admin_trust::EffectiveAuthority::Primary => {
                        crate::admin_trust::AdminLevel::Primary
                    }
                    crate::admin_trust::EffectiveAuthority::Secondary => {
                        crate::admin_trust::AdminLevel::Secondary
                    }
                }),
            (cfg.profile == crate::profile::NodeProfile::Admin)
                .then(|| crate::admin_trust::fingerprint_of(&state.authority_principal.pubkey)),
            cfg.advertise_addr.clone(),
            cfg.tls_required,
            cfg.quic_enabled,
            state
                .admin_trust
                .effective_authority(&state.authority_principal, cfg.profile)
                == Some(crate::admin_trust::EffectiveAuthority::Primary),
            state
                .admin_trust
                .effective_authority(&state.authority_principal, cfg.profile)
                == Some(crate::admin_trust::EffectiveAuthority::Primary),
            state
                .admin_trust
                .effective_authority(&state.authority_principal, cfg.profile)
                == Some(crate::admin_trust::EffectiveAuthority::Primary),
            state
                .admin_trust
                .effective_authority(&state.authority_principal, cfg.profile)
                == Some(crate::admin_trust::EffectiveAuthority::Primary),
            cfg.thermal,
            cfg.admin_exposure,
            cfg.admin_access_token
                .as_deref()
                .is_some_and(|hash| !hash.is_empty()),
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
        pom_desenv: crate::pom_desenv_enabled(),
        profile,
        network_name,
        connection_key,
        authority_level,
        authority_fingerprint,
        p2p_port: state.network.bind_addr.port(),
        addr_candidates: state.network.self_addr_candidates(),
        libp2p_peer_id: state.network.libp2p_peer_id.clone(),
        inference_frame_capabilities: {
            let mut capabilities = vec![
                crate::pipeline::CAP_FRAME_BATCH.to_string(),
                crate::pipeline::CAP_FRAME_DTYPE_F8.to_string(),
                crate::pipeline::CAP_FRAME_DTYPE_I4G64.to_string(),
                crate::pipeline::CAP_FRAME_DTYPE_I4G128.to_string(),
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
        admin_password_configured,
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
        quic_enabled,
        tls_authority,
        quic_authority,
        cluster_logs_enabled: state.network.cluster_logs_enabled(),
        cluster_logs_authority,
        network_chat_enabled: state.network.network_chat_enabled(),
        network_chat_authority,
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

async fn local_is_primary(state: &GatewayState) -> bool {
    let profile = state.auth.read().await.profile;
    state
        .admin_trust
        .effective_authority(&state.authority_principal, profile)
        == Some(crate::admin_trust::EffectiveAuthority::Primary)
}

#[derive(Debug, Serialize)]
pub struct AdminAccessInfo {
    pub admin_exposure: AdminExposure,
    pub gateway_port: u16,
    /// The hash is never returned. This flag lets the settings screen explain
    /// whether a network exposure already has a password to protect it.
    pub password_configured: bool,
}

pub(super) async fn admin_access_info(State(state): State<GatewayState>) -> Json<AdminAccessInfo> {
    let cfg = state.auth.read().await;
    Json(AdminAccessInfo {
        admin_exposure: cfg.admin_exposure,
        gateway_port: cfg.gateway_port,
        password_configured: cfg
            .admin_access_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .is_some(),
    })
}

#[derive(Debug, Deserialize)]
pub struct SaveAdminAccessRequest {
    pub admin_exposure: AdminExposure,
    pub gateway_port: u32,
    #[serde(default)]
    pub current_password: String,
    #[serde(default)]
    pub new_password: String,
}

/// Return the hash that should be persisted after an administrative password
/// change. Keeping this rule pure makes the first definition and the guarded
/// rotation independently testable, while the handler remains responsible for
/// exposure and persistence.
pub(crate) fn next_admin_password_hash(
    existing_hash: Option<&str>,
    current_password: &str,
    new_password: &str,
) -> anyhow::Result<Option<String>> {
    let existing_hash = existing_hash.filter(|hash| !hash.is_empty());
    if new_password.is_empty() {
        return Ok(existing_hash.map(str::to_owned));
    }
    if let Some(existing_hash) = existing_hash {
        if !crate::auth::verify_admin_password(existing_hash, current_password) {
            anyhow::bail!("a senha anterior está incorreta");
        }
    }
    crate::auth::hash_admin_password(new_password).map(Some)
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

    let existing_hash = state.auth.read().await.admin_access_token.clone();
    let next_password_hash = if req.admin_exposure.requires_password() {
        let current_password = req.current_password.clone();
        let new_password = req.new_password.clone();
        let existing_hash_for_task = existing_hash.clone();
        let result = tokio::task::spawn_blocking(move || {
            next_admin_password_hash(
                existing_hash_for_task.as_deref(),
                &current_password,
                &new_password,
            )
        })
        .await;
        let next = match result {
            Ok(Ok(next)) => next,
            Ok(Err(error)) => return bad_request(error.to_string()),
            Err(error) => {
                return bad_request(format!("não foi possível proteger a senha: {error}"))
            }
        };
        if next.is_none() {
            return bad_request(
                "uma senha é obrigatória quando a interface aceita conexões da rede",
            );
        }
        next
    } else {
        None
    };

    let (restart, password_configured, invalidate_sessions) = {
        let mut cfg = state.auth.write().await;
        if cfg.admin_access_token != existing_hash {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "a preferência de senha mudou; tente novamente" })),
            )
                .into_response();
        }
        let mut next = cfg.clone();
        next.admin_exposure = req.admin_exposure;
        next.gateway_port = port;
        // Localhost mode deliberately has no password. Network mode receives
        // the existing hash or the newly derived one, never plaintext.
        next.admin_access_token = next_password_hash;

        if let Err(error) = next.save() {
            return bad_request(format!(
                "não deu para gravar {}: {error}",
                crate::brand::config_filename()
            ));
        }
        let restart = port != state.gateway_port;
        let password_configured = next.admin_access_token.is_some();
        let invalidate_sessions = cfg.admin_exposure != next.admin_exposure
            || cfg.admin_access_token != next.admin_access_token;
        *cfg = next;
        (restart, password_configured, invalidate_sessions)
    };

    if invalidate_sessions {
        state.admin_sessions.lock().await.invalidate_all();
    }

    info!(
        exposure = ?req.admin_exposure,
        gateway_port = port,
        restart,
        password_changed = !req.new_password.is_empty(),
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
        "password_configured": password_configured,
    }))
    .into_response()
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
pub(super) fn spawn_enrichment(
    database: Arc<crate::db::PomDb>,
    roots: CatalogRoots,
    partial: Arc<Catalog>,
) {
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

pub(super) fn reject_nested_shard_dir(
    model_dirs: &[String],
    shard_dir: &str,
) -> Result<(), String> {
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
            return bad_request("o limite da pasta de modelos fatiados precisa ser maior que zero");
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

#[derive(Deserialize)]
pub struct SaveQuicRequest {
    pub quic_enabled: bool,
}

pub(super) async fn save_quic(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveQuicRequest>,
) -> Response {
    if !local_is_primary(&state).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "só o nó principal decide o QUIC do cluster." })),
        )
            .into_response();
    }
    {
        let mut cfg = state.auth.write().await;
        cfg.quic_enabled = req.quic_enabled;
        if let Err(error) = cfg.save() {
            return bad_request(format!(
                "não deu para gravar {}: {error}",
                crate::brand::config_filename()
            ));
        }
    }
    state.peer_http.set_quic(req.quic_enabled);
    crate::peer_client::set_quic_ativo(req.quic_enabled);
    info!(
        "QUIC entre peers {} pela interface",
        if req.quic_enabled {
            "ligado"
        } else {
            "desligado"
        }
    );
    Json(json!({ "quic_enabled": req.quic_enabled })).into_response()
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
    if !local_is_primary(&state).await {
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
    if !local_is_primary(&state).await {
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
        if req.logs_enabled {
            "ligados"
        } else {
            "desligados"
        }
    );
    Json(json!({ "logs_enabled": req.logs_enabled })).into_response()
}

#[derive(Deserialize, Serialize)]
pub struct SaveNetworkChatRequest {
    pub network_chat_enabled: bool,
}

/// Repassa a decisão ao principal real quando a tela está aberta num admin
/// secundário. A cadeia administrativa continua sendo a autoridade, e o
/// destino grava o valor no próprio pom.json antes de anunciá-lo.
async fn forward_network_chat_to_primary(
    state: &GatewayState,
    req: &SaveNetworkChatRequest,
) -> Response {
    let primary = state.admin_trust.current_primary();
    let Some(peer) = state.all_peers().await.into_iter().find(|peer| {
        peer.node_id == primary.node_id
            && peer.libp2p_peer_id.as_deref() == Some(primary.peer_id.as_str())
            && peer.authority_pubkey == primary.pubkey
            && peer.gateway_port.is_some()
    }) else {
        return bad_request("o nó principal não está disponível para decidir o Chat da rede");
    };
    let Some(base) = peer.gateway_url() else {
        return bad_request("o nó principal não anuncia uma porta de gateway");
    };
    let Ok(client) = state.peer_http.for_peer(&peer.node_id) else {
        return bad_request("sem cliente para o nó principal");
    };
    let request = match state
        .control_json_request(
            client.post(format!("{base}/api/ui/network/chat")),
            "/api/ui/network/chat",
            req,
        )
        .await
    {
        Ok(request) => request.timeout(PEER_TIMEOUT),
        Err(error) => {
            return bad_request(format!(
                "não foi possível assinar a decisão do Chat: {error}"
            ));
        }
    };
    match request.send().await {
        Ok(response) if response.status().is_success() => {
            Json(json!({ "network_chat_enabled": req.network_chat_enabled })).into_response()
        }
        Ok(response) => {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "o nó principal recusou a decisão".to_string());
            (status, body).into_response()
        }
        Err(error) => bad_request(format!("o nó principal está inacessível: {error}")),
    }
}

/// Liga ou desliga o Chat da rede para o cluster inteiro. O valor anunciado
/// só desperta a busca autenticada nos demais nós; apenas o principal real
/// grava a política, e um admin secundário encaminha a ação para ele.
pub(super) async fn save_network_chat(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveNetworkChatRequest>,
) -> Response {
    if !local_is_primary(&state).await {
        return forward_network_chat_to_primary(&state, &req).await;
    }

    {
        let mut cfg = state.auth.write().await;
        cfg.network_chat_enabled = req.network_chat_enabled;
        if let Err(error) = cfg.save() {
            return bad_request(format!(
                "não deu para gravar {}: {error}",
                crate::brand::config_filename()
            ));
        }
    }
    state
        .network
        .set_network_chat_enabled(req.network_chat_enabled);
    state.network_chat.set_enabled(req.network_chat_enabled);
    info!(
        "Chat da rede {} pela interface",
        if req.network_chat_enabled {
            "ligado"
        } else {
            "desligado"
        }
    );
    Json(json!({ "network_chat_enabled": req.network_chat_enabled })).into_response()
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
pub(super) async fn network_rtt(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Json<RttReport> {
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
pub(super) struct InferenceHeadsQuery {
    pub(super) model: String,
}

#[derive(Debug, Serialize)]
pub(super) struct InferenceHeadCandidate {
    pub(super) node_id: String,
    pub(super) short_id: String,
    pub(super) hostname: String,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    pub(super) total_layers: u32,
    /// In-flight requests reported by the candidate itself. `None` means the
    /// candidate's live node view could not be read.
    pub(super) load: Option<u32>,
    pub(super) runtime_capacity: Option<crate::network::RuntimeCapacity>,
    pub(super) rtt_ms: Option<f64>,
    pub(super) rtt_age_ms: Option<u64>,
    pub(super) rtt_state: &'static str,
    pub(super) selected: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct InferenceHeadsResponse {
    pub(super) model: String,
    /// `selected`, `incomplete_pipeline`, `no_candidate` or `invalid_model`.
    pub(super) status: &'static str,
    pub(super) selected_node_id: Option<String>,
    pub(super) selection_policy: &'static str,
    pub(super) rtt_valid_for_secs: u64,
    pub(super) candidates: Vec<InferenceHeadCandidate>,
}

/// Read a candidate's live load from the candidate itself. The local tracker
/// only knows requests that entered this gateway, so it cannot stand in for a
/// remote candidate and its unseen entries must not become zero.
fn reported_peer_load(node: &serde_json::Value) -> Option<u32> {
    node.get("load")
        .and_then(serde_json::Value::as_u64)
        .and_then(|load| u32::try_from(load).ok())
}

async fn live_peer_load(state: &GatewayState, peer: &PeerInfo) -> Option<u32> {
    if peer.node_id == state.network.node_id {
        return Some(state.loads.load(&peer.node_id).await);
    }

    let client = state.peer_http.for_peer(&peer.node_id).ok()?;
    for base in peer.gateway_candidates() {
        let request = state
            .control_get_request(client.get(format!("{base}/api/ui/node")), "/api/ui/node")
            .await
            .ok()?;
        let response = tokio::time::timeout(PEER_TIMEOUT, request.send()).await;
        let Ok(Ok(response)) = response else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(node) = response.json::<serde_json::Value>().await else {
            continue;
        };
        return reported_peer_load(&node);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admin_password_can_be_defined_without_a_previous_password() {
        let hash = next_admin_password_hash(None, "", "senha nova")
            .expect("definir a senha")
            .expect("hash da senha definida");

        assert!(crate::auth::verify_admin_password(&hash, "senha nova"));
        assert!(!hash.contains("senha nova"));
    }

    #[test]
    fn changing_admin_password_requires_the_previous_password() {
        let old_hash = crate::auth::hash_admin_password("senha antiga").expect("senha antiga");
        assert!(next_admin_password_hash(Some(&old_hash), "errada", "senha nova").is_err());

        let new_hash = next_admin_password_hash(Some(&old_hash), "senha antiga", "senha nova")
            .expect("trocar a senha")
            .expect("hash da senha nova");
        assert!(crate::auth::verify_admin_password(&new_hash, "senha nova"));
        assert!(!crate::auth::verify_admin_password(
            &new_hash,
            "senha antiga"
        ));
    }

    #[test]
    fn infer_load_stays_unknown_when_the_candidate_did_not_report_it() {
        assert_eq!(reported_peer_load(&json!({ "load": 4 })), Some(4));
        assert_eq!(reported_peer_load(&json!({})), None);
        assert_eq!(reported_peer_load(&json!({ "load": -1 })), None);
    }

    #[tokio::test]
    async fn secondary_admin_reads_its_local_cluster_log_replica() {
        let state = crate::gateway::testkit::secondary_admin_state("secondary").await;
        state
            .database
            .append_cluster_logs(
                &[crate::cluster_logs::LogEntry {
                    node_id: "worker".into(),
                    node_name: "worker-1".into(),
                    deploy_id: "deploy-1".into(),
                    origin_at: "2026-09-11T12:00:00Z".into(),
                    level: "INFO".into(),
                    target: "worker::test".into(),
                    message: "replica local".into(),
                }],
                0,
            )
            .expect("gravar réplica local");

        let response = cluster_logs(
            ConnectionAuth,
            State(state),
            Query(ClusterLogsQuery {
                deploy_id: Some("deploy-1".into()),
                limit: Some(10),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("corpo da réplica");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON da réplica");
        assert_eq!(body["entries"][0]["message"], "replica local");
    }
}

/// Gives an infer node only the layer-zero choices for one model. It is a
/// read-only capability view, not a cluster browser: no peer outside these
/// candidates crosses this boundary, and the response contains no control
/// action or authority field.
pub(super) async fn inference_heads(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<InferenceHeadsQuery>,
) -> Json<InferenceHeadsResponse> {
    let requested_model = query.model.trim().to_string();
    let (model, deploy_id) = crate::deployment::split_selector(&requested_model);
    if model.is_empty() {
        return Json(InferenceHeadsResponse {
            model: requested_model,
            status: "invalid_model",
            selected_node_id: None,
            selection_policy: "coverage_then_weighted_rtt_load",
            rtt_valid_for_secs: crate::route::RTT_INTERVALO_MEDICAO.as_secs(),
            candidates: Vec::new(),
        });
    }

    let peers = state.all_peers().await;
    let mut heads = crate::pipeline::model_head_candidates(&peers, model, deploy_id);
    heads.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    let rtt_snapshots = state.network.rtts.snapshot_with_age();
    let rtts = rtt_snapshots
        .iter()
        .filter(|entry| entry.age < crate::route::RTT_INTERVALO_MEDICAO)
        .map(|entry| {
            (
                entry.measurement.target_node_id.clone(),
                entry.measurement.rtt_ms,
            )
        })
        .collect::<HashMap<_, _>>();

    let total_layers = crate::pipeline::model_pipeline_candidates(&peers, model, deploy_id)
        .iter()
        .map(|peer| peer.total_layers)
        .max()
        .unwrap_or(0);
    let selected_node_id = if heads.is_empty() || total_layers == 0 {
        None
    } else {
        crate::pipeline::plan_pipeline_with_rtt(
            &peers,
            model,
            deploy_id,
            total_layers,
            &state.loads,
            &rtts,
        )
        .await
        .and_then(|plan| {
            plan.steps
                .first()
                .filter(|step| step.start_layer == 0)
                .map(|step| step.node_id.clone())
        })
    };

    let loads =
        futures::future::join_all(heads.iter().map(|peer| live_peer_load(&state, peer))).await;
    let candidates = if heads.is_empty() {
        Vec::new()
    } else {
        let mut candidates = Vec::with_capacity(heads.len());
        for (peer, load) in heads.into_iter().zip(loads) {
            let rtt = rtt_snapshots
                .iter()
                .find(|entry| entry.measurement.target_node_id == peer.node_id);
            let (rtt_ms, rtt_age_ms, rtt_state) = match rtt {
                Some(entry) if entry.age < crate::route::RTT_INTERVALO_MEDICAO => (
                    Some(entry.measurement.rtt_ms),
                    Some(entry.age.as_millis().try_into().unwrap_or(u64::MAX)),
                    "fresh",
                ),
                Some(entry) => (
                    Some(entry.measurement.rtt_ms),
                    Some(entry.age.as_millis().try_into().unwrap_or(u64::MAX)),
                    "stale",
                ),
                None => (None, None, "unmeasured"),
            };
            candidates.push(InferenceHeadCandidate {
                node_id: peer.node_id.clone(),
                short_id: peer.node_id.chars().take(8).collect(),
                hostname: peer.hostname.clone(),
                start_layer: peer.start_layer,
                end_layer: peer.end_layer,
                total_layers: peer.total_layers,
                load,
                runtime_capacity: peer.runtime_capacity,
                rtt_ms,
                rtt_age_ms,
                rtt_state,
                selected: selected_node_id.as_deref() == Some(peer.node_id.as_str()),
            });
        }
        candidates
    };

    Json(InferenceHeadsResponse {
        model: requested_model,
        status: if candidates.is_empty() {
            "no_candidate"
        } else if selected_node_id.is_some() {
            "selected"
        } else {
            "incomplete_pipeline"
        },
        selected_node_id,
        selection_policy: "coverage_then_weighted_rtt_load",
        rtt_valid_for_secs: crate::route::RTT_INTERVALO_MEDICAO.as_secs(),
        candidates,
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatTelemetryRequest {
    #[serde(default)]
    pub(super) node_ids: Vec<String>,
    /// ID OpenAI da execução. Quando ausente, o painel consulta a execução
    /// concluída mais recente que tiver as arestas pedidas.
    #[serde(default)]
    pub(super) execution_id: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pipeline_cost: Option<ChatTelemetryPipelineCost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) pipeline_routes: Vec<ChatTelemetryPipelineRoute>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct ChatTelemetryPipelineCost {
    pub(super) legs_ms: Vec<Option<f64>>,
    pub(super) compute_ms: Option<f64>,
}

#[derive(Debug, Serialize, PartialEq, Clone)]
pub(super) struct ChatTelemetryPipelineRoute {
    /// Caminho completo, incluindo o gateway virtual no retorno.
    pub(super) node_ids: Vec<String>,
    /// Gateway -> cabeça, handoffs e cauda -> gateway, nessa ordem.
    pub(super) legs_ms: Vec<Option<f64>>,
    /// Soma das pernas quando todas foram medidas; não inclui compute.
    pub(super) rtt_ms: Option<f64>,
}

fn collect_pipeline_route_paths(
    candidates: &[&crate::network::PeerInfo],
    next_layer: u32,
    total_layers: u32,
    path: &mut Vec<String>,
    routes: &mut Vec<Vec<String>>,
) {
    if next_layer >= total_layers {
        routes.push(path.clone());
        return;
    }

    let mut next = candidates
        .iter()
        .copied()
        .filter(|peer| peer.start_layer == next_layer && peer.end_layer >= next_layer)
        .collect::<Vec<_>>();
    next.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    for peer in next {
        if path.iter().any(|node_id| node_id == &peer.node_id) {
            continue;
        }
        path.push(peer.node_id.clone());
        collect_pipeline_route_paths(
            candidates,
            peer.end_layer.saturating_add(1),
            total_layers,
            path,
            routes,
        );
        path.pop();
    }
}

/// Produz todas as rotas completas do modelo, não apenas a escolhida pelo
/// planner. A ordem do resultado é menor soma de RTT primeiro, depois o vetor
/// de IDs dos nós como desempate estável. Assim o card mostra o head rápido
/// primeiro sem esconder uma alternativa implantada.
pub(super) fn pipeline_routes_for(
    peers: &[crate::network::PeerInfo],
    model: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    gateway_node_id: &str,
    pairwise_rtt: &HashMap<(String, String), f64>,
) -> Vec<ChatTelemetryPipelineRoute> {
    if model.is_empty() || total_layers == 0 {
        return Vec::new();
    }
    let candidates = crate::pipeline::model_pipeline_candidates(peers, model, deploy_id);
    let mut paths = Vec::new();
    collect_pipeline_route_paths(&candidates, 0, total_layers, &mut Vec::new(), &mut paths);

    let mut routes = paths
        .into_iter()
        .map(|path| {
            // The gateway is a virtual return endpoint, except when it also
            // serves the terminal slice. In that case the pipeline path
            // already ends at the gateway and appending it would create a
            // fake self-hop and show the same node twice in the card.
            let mut node_ids = path;
            if node_ids.last().map(String::as_str) != Some(gateway_node_id) {
                node_ids.push(gateway_node_id.to_owned());
            }
            let leg_nodes = std::iter::once(gateway_node_id.to_owned())
                .chain(node_ids.iter().cloned())
                .collect::<Vec<_>>();
            let legs_ms = leg_nodes
                .windows(2)
                .map(|pair| {
                    if pair[0] == pair[1] {
                        return Some(0.0);
                    }
                    pairwise_rtt
                        .get(&(pair[0].clone(), pair[1].clone()))
                        .copied()
                        .filter(|rtt| rtt.is_finite() && *rtt >= 0.0)
                })
                .collect::<Vec<_>>();
            let rtt_ms = legs_ms
                .iter()
                .copied()
                .collect::<Option<Vec<_>>>()
                .map(|legs| legs.into_iter().sum());
            ChatTelemetryPipelineRoute {
                node_ids,
                legs_ms,
                rtt_ms,
            }
        })
        .collect::<Vec<_>>();
    routes.sort_by(|left, right| {
        match (left.rtt_ms, right.rtt_ms) {
            (Some(left), Some(right)) => left.total_cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.node_ids.cmp(&right.node_ids))
    });
    routes
}

/// Retorna o RTT de fio observado no último trace, por perna direcionada.
///
/// A sonda de seleção (`RttTable`) é libp2p e mede o endpoint `/health`; ela é
/// um fallback útil quando não há execução recente, mas não representa
/// necessariamente o transporte escolhido pelo pipeline. Quando o trace tem
/// `net_rtt_median_ms`, ele é a fonte do card. O decode tem precedência sobre
/// o prefill se a mesma perna aparecer nas duas fases.
pub(super) fn execution_network_rtts(
    trace: &serde_json::Value,
    gateway_node_id: &str,
) -> HashMap<(String, String), f64> {
    let mut values = HashMap::new();
    let Some(cycle) = trace.get("pipeline_cycle") else {
        return values;
    };

    for phase in ["prefill", "decode"] {
        let Some(legs) = cycle.get(phase).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for leg in legs {
            let Some(mut from_node_id) = leg
                .get("from_node_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(mut to_node_id) = leg
                .get("to_node_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let Some((_, rtt_ms)) = leg
                .get("transport_summary")
                .and_then(dominant_transport_measurement)
            else {
                continue;
            };
            if from_node_id == "gateway" {
                from_node_id = gateway_node_id.to_owned();
            }
            if to_node_id == "gateway" {
                to_node_id = gateway_node_id.to_owned();
            }
            values.insert((from_node_id, to_node_id), rtt_ms);
        }
    }
    values
}

fn dominant_transport_measurement(summary: &serde_json::Value) -> Option<(String, f64)> {
    summary
        .get("transports")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|entry| {
            let transport = entry.get("transport")?.as_str()?.to_owned();
            let calls = entry.get("calls")?.as_u64()?;
            if calls == 0 {
                return None;
            }
            let metric = |key: &str| {
                entry
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .or_else(|| {
                        entry
                            .get(key)
                            .and_then(serde_json::Value::as_u64)
                            .map(|value| value as f64)
                    })
            };
            // This endpoint feeds the network panel and route labels. The
            // full execution RTT includes remote processing, so prefer the
            // transport's wire RTT and keep the old field as a fallback for
            // traces written before net_rtt_median_ms existed.
            let rtt_ms = metric("net_rtt_median_ms").or_else(|| metric("rtt_median_ms"))?;
            Some((calls, transport, rtt_ms))
        })
        .max_by_key(|(calls, _, _)| *calls)
        .map(|(_, transport, rtt_ms)| (transport, rtt_ms))
}

fn cycle_legs(trace: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    trace
        .get("pipeline_cycle")
        .and_then(|cycle| cycle.get("decode"))
        .and_then(serde_json::Value::as_array)
        .filter(|legs| !legs.is_empty())
}

/// Extrai a medição da aresta de modelo a partir do trace que a execução
/// acabou de produzir.
///
/// `hops` representa nós, enquanto o transporte de uma aresta entra no hop
/// downstream. Assim, em `head -> tail`, o probe do gateway em `hops[0]` não
/// pode ser usado para nomear a aresta: a evidência da aresta está em
/// `hops[1].decode.transport_summary`. O caso `remote-stream` acrescenta uma
/// entrada sintética no hop zero, e por isso a mesma regra continua válida.
pub(super) fn execution_transport_measurements(
    trace: &serde_json::Value,
    requested: &std::collections::HashSet<String>,
) -> Vec<ChatTelemetryMeasurement> {
    if let Some(legs) = cycle_legs(trace) {
        return legs
            .iter()
            .filter_map(|leg| {
                let from_node_id = leg.get("from_node_id")?.as_str()?;
                let to_node_id = leg.get("to_node_id")?.as_str()?;
                if from_node_id == "gateway"
                    || !requested.contains(from_node_id)
                    || (to_node_id != "gateway" && !requested.contains(to_node_id))
                {
                    return None;
                }
                let summary = leg.get("transport_summary")?;
                let (transport, rtt_ms) = dominant_transport_measurement(summary)?;
                Some(ChatTelemetryMeasurement {
                    from_node_id: from_node_id.to_owned(),
                    to_node_id: to_node_id.to_owned(),
                    rtt_ms,
                    transport,
                })
            })
            .collect();
    }

    let Some(hops) = trace.get("hops").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };

    hops.windows(2)
        .filter_map(|window| {
            let from_node_id = window[0].get("node_id")?.as_str()?;
            let to_node_id = window[1].get("node_id")?.as_str()?;
            if from_node_id == to_node_id
                || !requested.contains(from_node_id)
                || !requested.contains(to_node_id)
            {
                return None;
            }

            let phase = window[1]
                .get("decode")
                .filter(|phase| {
                    phase
                        .get("transport_summary")
                        .and_then(|summary| summary.get("transports"))
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|entries| {
                            entries.iter().any(|entry| {
                                entry.get("calls").and_then(serde_json::Value::as_u64) > Some(0)
                            })
                        })
                })
                .or_else(|| window[1].get("prefill"))?;
            let entries = phase
                .get("transport_summary")
                .and_then(|summary| summary.get("transports"))
                .and_then(serde_json::Value::as_array)?;
            let (transport, rtt_ms) = dominant_transport_measurement(&json!({
                "transports": entries,
            }))?;

            Some(ChatTelemetryMeasurement {
                from_node_id: from_node_id.to_owned(),
                to_node_id: to_node_id.to_owned(),
                rtt_ms,
                transport,
            })
        })
        .collect()
}

/// Retorna as pernas na ordem do ciclo e o compute observado nos peers. Uma
/// perna ausente permanece `null`: o card não transforma telemetria incompleta
/// em zero e não fecha o total a partir de uma subtração do wall clock. Sem
/// `pipeline_cycle`, mantém o contrato antigo sem inventar um custo parcial.
pub(super) fn execution_pipeline_cost(
    trace: &serde_json::Value,
) -> Option<ChatTelemetryPipelineCost> {
    let hops = trace.get("hops").and_then(serde_json::Value::as_array)?;
    if hops.is_empty() {
        return None;
    }
    let expected_pairs: Vec<(String, String)> = std::iter::once((
        "gateway".to_owned(),
        hops.first()?.get("node_id")?.as_str()?.to_owned(),
    ))
    .chain(hops.windows(2).filter_map(|window| {
        Some((
            window[0].get("node_id")?.as_str()?.to_owned(),
            window[1].get("node_id")?.as_str()?.to_owned(),
        ))
    }))
    .chain(std::iter::once((
        hops.last()?.get("node_id")?.as_str()?.to_owned(),
        "gateway".to_owned(),
    )))
    .collect();

    let cycle = cycle_legs(trace)?;
    let legs_ms = expected_pairs
        .iter()
        .map(|(from_node_id, to_node_id)| {
            cycle
                .iter()
                .find(|leg| {
                    leg.get("from_node_id").and_then(serde_json::Value::as_str)
                        == Some(from_node_id.as_str())
                        && leg.get("to_node_id").and_then(serde_json::Value::as_str)
                            == Some(to_node_id.as_str())
                })
                .and_then(|leg| leg.get("transport_summary"))
                .and_then(dominant_transport_measurement)
                .map(|(_, rtt_ms)| rtt_ms)
        })
        .collect();

    let compute_ms = hops
        .iter()
        .map(|hop| hop.get("decode")?.get("processing_ms")?.as_f64())
        .collect::<Option<Vec<_>>>()
        .map(|values| values.into_iter().sum());

    Some(ChatTelemetryPipelineCost {
        legs_ms,
        compute_ms,
    })
}

/// Reúne as sondas direcionais frescas que cada peer já mantém localmente.
/// Uma sonda do gateway só conhece seus destinos; sem os relatórios dos
/// próprios peers, o card nunca conseguiria somar um pipeline com mais de um
/// salto nem medir a perna final de volta ao gateway.
async fn pairwise_rtt_for_pipeline(
    state: &GatewayState,
    peers: &[crate::network::PeerInfo],
) -> HashMap<(String, String), f64> {
    let self_id = state.network.node_id.clone();
    let mut values = state
        .network
        .rtts
        .fresh_snapshot()
        .into_iter()
        .map(|measurement| {
            (
                (self_id.clone(), measurement.target_node_id),
                measurement.rtt_ms,
            )
        })
        .collect::<HashMap<_, _>>();

    let reports = futures::future::join_all(peers.iter().filter_map(|peer| {
        if peer.node_id == self_id {
            return None;
        }
        let client = state.peer_http.for_peer(&peer.node_id).ok()?;
        let peer = peer.clone();
        let state = state.clone();
        Some(async move {
            for base in peer.gateway_candidates() {
                let Ok(request) = state
                    .control_get_request(
                        client.get(format!("{base}/api/ui/network/rtt")),
                        "/api/ui/network/rtt",
                    )
                    .await
                else {
                    continue;
                };
                let Ok(Ok(response)) = tokio::time::timeout(PEER_TIMEOUT, request.send()).await
                else {
                    continue;
                };
                if !response.status().is_success() {
                    continue;
                }
                let Ok(report) = response.json::<RttReport>().await else {
                    continue;
                };
                if report.source_node_id == peer.node_id {
                    return Some(report);
                }
            }
            None
        })
    }))
    .await;

    for report in reports.into_iter().flatten() {
        for measurement in report.measurements {
            values.insert(
                (report.source_node_id.clone(), measurement.target_node_id),
                measurement.rtt_ms,
            );
        }
    }
    values
}

/// Retorna apenas o transporte observado na execução distribuída.
///
/// `RttTable` continua sendo a fonte de probes para seleção de rota, mas não é
/// uma medida da execução: em particular, uma probe libp2p não pode aparecer
/// no Chat ou no mapa quando o frame real cruzou por `https-link`.
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
            pipeline_cost: None,
            pipeline_routes: Vec::new(),
        });
    }
    let requested: std::collections::HashSet<String> = request
        .node_ids
        .into_iter()
        .filter(|node_id| !node_id.is_empty())
        .collect();
    if requested.len() < 2 && request.execution_id.is_some() {
        return Json(ChatTelemetryResponse {
            measurements: Vec::new(),
            pipeline_cost: None,
            pipeline_routes: Vec::new(),
        });
    }

    let trace = if let Some(execution_id) = request.execution_id.as_deref() {
        let mut trace = None;
        for attempt in 0..5 {
            match state.audit.telemetry(execution_id).await {
                Ok(found) => {
                    trace = found;
                    if trace.is_some() || attempt == 4 {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, execution_id, "não foi possível consultar o trace da execução");
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        trace
    } else if requested.len() >= 2 {
        match state.audit.recent_telemetries(32).await {
            Ok(traces) => traces
                .into_iter()
                .find(|trace| !execution_transport_measurements(trace, &requested).is_empty()),
            Err(error) => {
                tracing::warn!(%error, "não foi possível consultar a última telemetria");
                None
            }
        }
    } else {
        None
    };
    let mut measurements = trace
        .as_ref()
        .map(|trace| execution_transport_measurements(trace, &requested))
        .unwrap_or_default();
    let pipeline_cost = trace.as_ref().and_then(execution_pipeline_cost);

    let pipeline_routes = if request.execution_id.is_none() {
        let peers = state.all_peers().await;
        let active_selector = state.active_model().await;
        let (model, deploy_id) = crate::deployment::split_selector(&active_selector);
        let total_layers = crate::pipeline::model_pipeline_candidates(&peers, model, deploy_id)
            .iter()
            .map(|peer| peer.total_layers)
            .max()
            .unwrap_or(0);
        let mut pairwise_rtt = pairwise_rtt_for_pipeline(&state, &peers).await;
        if let Some(trace) = trace.as_ref() {
            // A completed execution tells us which transport actually carried
            // each leg. Prefer that wire RTT over the independent libp2p
            // health probes, which may have a different path and queueing.
            pairwise_rtt.extend(execution_network_rtts(trace, &state.network.node_id));
        }
        pipeline_routes_for(
            &peers,
            model,
            deploy_id,
            total_layers,
            &state.network.node_id,
            &pairwise_rtt,
        )
    } else {
        Vec::new()
    };

    measurements.sort_by(|a, b| {
        a.from_node_id
            .cmp(&b.from_node_id)
            .then_with(|| a.to_node_id.cmp(&b.to_node_id))
    });
    Json(ChatTelemetryResponse {
        measurements,
        pipeline_cost,
        pipeline_routes,
    })
}

pub(super) async fn chat_history(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
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

pub(super) async fn network_chat_snapshot(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let mut messages = match state
        .network_chat
        .history(crate::network_chat::NETWORK_CHAT_ROOM, 100)
    {
        Ok(messages) => messages,
        Err(error) => {
            tracing::error!("não foi possível ler o Chat da rede: {error:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível ler o Chat da rede" })),
            )
                .into_response();
        }
    };
    for message in &mut messages {
        message.file_data.clear();
    }
    let mut participants = vec![json!({
        "nodeId": state.network.node_id,
        "hostname": state.network.hostname,
        "online": true,
        "profile": format!("{:?}", state.network.profile()),
    })];
    let blocked = state.auth.read().await.blocked_nodes.clone();
    participants.extend(
        state
            .network
            .alive_peers()
            .await
            .into_iter()
            .filter(|peer| peer.node_id != state.network.node_id)
            .filter(|peer| !blocked.iter().any(|node_id| node_id == &peer.node_id))
            .map(|peer| {
                json!({
                    "nodeId": peer.node_id,
                    "hostname": peer.hostname,
                    "online": true,
                    "profile": format!("{:?}", peer.profile),
                })
            }),
    );
    Json(json!({
        "enabled": state.network_chat.enabled(),
        "messages": messages,
        "participants": participants,
        "typing": state.network_chat.typing_snapshot(),
        "dropped": state.network_chat.dropped(),
    }))
    .into_response()
}

pub(super) async fn network_chat_action(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(request): Json<crate::network_chat::ChatRequest>,
) -> Response {
    if request.api_key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "o token do Chat da rede é obrigatório" })),
        )
            .into_response();
    }
    match state.network_chat.submit(request).await {
        Ok(response) if response.accepted => Json(response).into_response(),
        Ok(response) => (StatusCode::FORBIDDEN, Json(response)).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

pub(super) async fn network_chat_file(
    _auth: ConnectionAuth,
    Path(message_id): Path<String>,
    State(state): State<GatewayState>,
) -> Response {
    let file = match state.database.network_chat_file(&message_id) {
        Ok(Some(file)) => file,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!("não foi possível ler arquivo do Chat da rede: {error:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut response = Body::from(file.0).into_response();
    if let Ok(value) = HeaderValue::from_str(&file.1) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
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

pub(super) async fn clear_chat_history(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
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
    state.network.rtts.fresh_snapshot()
}
