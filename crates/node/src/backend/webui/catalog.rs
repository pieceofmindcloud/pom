use super::*;
// ── Node view ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct NodeInfo {
    pub node_id: String,
    pub short_id: String,
    pub hostname: String,
    /// Commit this binary was built from, so "which build is that peer running?"
    /// is answerable without a shell on the machine.
    /// test
    pub build_commit: String,
    /// This node accepts updating itself through the interface. The screen only
    /// draws the button when this is true, so it never offers what will give 403.
    pub dev_update: bool,
    /// Controls whether development-only POM deployment settings are exposed.
    pub pom_dev_mode: bool,
    pub profile: crate::profile::NodeProfile,
    /// Human-facing network name shown by the leave action in the main menu.
    pub network_name: String,
    /// This node is the principal of the network it is in: its own network,
    /// which is inherent to the machine and cannot be left.
    pub own_network: bool,
    /// Name of this node's own network while it is in standby, i.e. while
    /// the node is a member of someone else's. Leaving returns to it.
    pub standby_network: Option<String>,
    /// This node's copy of the trust chain: `version`, whether a peer
    /// announces a newer one (`outdated`) and on which hosts (`newer_on`).
    pub cluster_version: serde_json::Value,
    /// This cluster's connection key. It is not screen decoration: the interface
    /// shows it under "Connection Key" **and** keeps it to authenticate the calls
    /// that require `ConnectionAuth` (`lib/queries.ts`, `useNode`). It is the
    /// same across the whole cluster and is already in this machine's `pom.json`,
    /// and this route is only served behind the administrative boundary
    /// (`require_admin_for_surface`), so whoever reads it has already proved to
    /// be this machine's operator. Removing it from here left the browser
    /// without a credential and the screen reading `undefined.length`.
    pub connection_key: String,
    pub authority_level: Option<crate::admin_trust::AdminLevel>,
    pub authority_fingerprint: Option<String>,
    pub p2p_port: u16,
    /// UDP addresses announced by the node, needed for the plan to rebuild
    /// the tunnel's TCP endpoint addresses in the process that runs the hop.
    pub addr_candidates: Vec<String>,
    /// Identity and capabilities of the p2p tunnel announced by this node.
    pub endpoint_id: Option<String>,
    pub inference_frame_capabilities: Vec<String>,
    pub gateway_port: u16,
    pub admin_exposure: AdminExposure,
    pub admin_password_configured: bool,
    pub inference_port: Option<u16>,
    pub config: Option<NodeConfig>,
    pub status: RuntimeStatus,
    pub system: SystemLoad,
    /// What this machine's running plugins serve to the cluster
    /// (`host.services`), for the dashboard to draw next to the machine.
    pub plugin_services: Vec<crate::plugin_services::PluginService>,
    /// Disk used by this machine's KV snapshot directory (`kv-slots`).
    pub kv_cache: KvCacheDisk,
    pub peer_count: usize,
    pub load: u32,
    pub active_model: String,
    pub active_deployment: String,
    /// True while the operator has taken this node off the network.
    pub detached: bool,
    /// The name the operator gave this machine (`Config.node_name`); `None`
    /// means it goes by the system hostname, which `hostname` then carries.
    pub node_name: Option<String>,
    /// Raw operator-pinned address (`Config.advertise_addr`), for pre-filling
    /// the edit form — `None` means "nothing pinned," not "unknown."
    pub advertise_addr: Option<String>,
    /// What this node actually puts in `Announce.addr` right now
    /// (`Network::announce_addr()`, full `host:port`): the pin if set, else
    /// a peer-observed public address, else the `0.0.0.0:<port>` "ask the
    /// receiver" sentinel. Always present — lets the UI show the fix taking
    /// effect without a second round trip.
    pub effective_address: String,
    /// Whether the cluster speaks TLS between peers right now.
    pub tls_required: bool,
    /// Whether **this** node may change that - only the principal may. The screen
    /// uses it to decide between a switch and a read-only value; the check that
    /// counts is the one in `save_tls`, on the server.
    pub tls_authority: bool,
    /// Whether the cluster prefers HTTP/3 over QUIC on new hops.
    pub quic_enabled: bool,
    /// Only the principal may change the QUIC preference.
    pub quic_authority: bool,
    /// Whether this node collects and sends the cluster's process logs.
    pub cluster_logs_enabled: bool,
    /// Only the principal may change the logs policy.
    pub cluster_logs_authority: bool,
    /// Whether this node accepts the global conversation channel.
    pub network_chat_enabled: bool,
    /// Only the principal may change the network Chat policy.
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
        node_name,
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
            cfg.node_name.clone(),
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
    let kv_cache = {
        let dir = state.auth.read().await.slot_save_dir();
        kv_cache_disk(dir.as_deref())
    };
    let plugin_services = crate::plugin_services::node_services(&state.plugins).await;
    Json(NodeInfo {
        short_id: state.network.node_id[..state.network.node_id.len().min(12)].to_string(),
        node_id: state.network.node_id.clone(),
        // Not `system.hostname`: that's the raw OS hostname, and ignores
        // `node_name` from pom.json. `network.hostname` is what
        // `with_identity` (main.rs) actually announces to peers — the same
        // value `/admin/nodes` already shows correctly on the Network tab.
        hostname: state.network.hostname(),
        build_commit: crate::build_commit(),
        dev_update: crate::devupdate::enabled(),
        pom_dev_mode: crate::pom_dev_mode_enabled(),
        profile,
        network_name,
        own_network: state.admin_trust.document().primary.pubkey
            == state.authority_principal.pubkey,
        standby_network: super::standby::standby_name(),
        cluster_version: crate::admin::chain_status(&state).await,
        connection_key,
        authority_level,
        authority_fingerprint,
        p2p_port: state.network.bind_addr.port(),
        addr_candidates: state.network.self_addr_candidates(),
        endpoint_id: state.network.endpoint_id.clone(),
        inference_frame_capabilities: if let Some(caps) = state.network.plugin_engine_capabilities()
        {
            caps
        } else {
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
            if state.network.p2p_tunnel_ready() {
                capabilities.push(crate::pipeline::CAP_P2P_TUNNEL.to_string());
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
        node_name,
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
        plugin_services,
        kv_cache,
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
        Ok(Some(logs)) => Json(logs).into_response(),
        Ok(None) => (
            StatusCode::FORBIDDEN,
            Json(json!({
                "code": "logs_plugin_required",
                "error": "the cluster process logs live in the logs plugin, which is not active on this node.",
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not read the logs: {error}") })),
        )
            .into_response(),
    }
}

pub(super) async fn local_is_primary(state: &GatewayState) -> bool {
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
            anyhow::bail!("the previous password is incorrect");
        }
    }
    crate::auth::hash_admin_password(new_password).map(Some)
}

/// Persists the boundary of the administrative interface. The pipeline routes
/// are not changed: the gateway listener stays dual-stack and the inference
/// server keeps listening on IPv4/IPv6 for the tunnel fallback.
pub(super) async fn save_admin_access(
    State(state): State<GatewayState>,
    Json(req): Json<SaveAdminAccessRequest>,
) -> Response {
    let port = match u16::try_from(req.gateway_port) {
        Ok(port) => port,
        Err(_) => return bad_request("the interface port must be between 1 and 65535"),
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
            Err(error) => return bad_request(format!("could not protect the password: {error}")),
        };
        if next.is_none() {
            return bad_request(
                "a password is required when the interface accepts connections from the network",
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
                Json(json!({ "error": "the password preference changed; try again" })),
            )
                .into_response();
        }
        let mut next = cfg.clone();
        next.admin_exposure = req.admin_exposure;
        next.gateway_port = port;
        // Localhost mode deliberately has no password. Network mode receives
        // the existing hash or the newly derived one, never plaintext.
        next.admin_access_token = next_password_hash;
        // Changing the password back to the default is allowed, and then the warning returns.
        next.admin_password_default = req.admin_exposure.requires_password()
            && if req.new_password.is_empty() {
                cfg.admin_password_default
            } else {
                req.new_password == crate::auth::DEFAULT_ADMIN_PASSWORD
            };

        if let Err(error) = next.save() {
            return bad_request(format!(
                "could not write {}: {error}",
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
        "administrative access preference updated from the interface"
    );

    if restart {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            let error = crate::respawn::respawn_self();
            tracing::error!("respawn failed, the node stays up: {error}");
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
    catalog::rebuild_shard_sets_fast(&mut pending);
    pending.metadata_complete = false;
    pending
}

/// Files the enrichment read and discarded leave the current inventory too.
///
/// The fast scan only knows sidecars by name (`mmproj*`, `mtp-*`); one known
/// only by its header (an MTP draft named `Ckpt_Q38`, or with no extension at
/// all) enters as a pending model, and `catalog::enrich` drops it once the
/// header shows what it is. Left in the inventory it stayed pending forever,
/// and every finished enrichment started the next one at once - hundreds of
/// passes per second. Returns the paths removed.
pub(super) fn drop_discarded(
    current: &mut Catalog,
    input: &Catalog,
    enriched: &Catalog,
) -> Vec<String> {
    let kept: std::collections::HashSet<&str> = enriched
        .models
        .iter()
        .map(|model| model.path.as_str())
        .collect();
    let discarded: std::collections::HashSet<&str> = input
        .models
        .iter()
        .map(|model| model.path.as_str())
        .filter(|path| !kept.contains(path))
        .collect();
    if discarded.is_empty() {
        return Vec::new();
    }
    let mut removed = Vec::new();
    current.models.retain(|model| {
        let drop = !catalog::metadata_is_current(model) && discarded.contains(model.path.as_str());
        if drop {
            removed.push(model.path.clone());
        }
        !drop
    });
    if !removed.is_empty() {
        catalog::rebuild_shard_sets(current);
    }
    removed
}

/// Copy completed metadata into a newer inventory by filesystem identity.
/// Inventory refreshes necessarily create a new `Arc`, so pointer equality is
/// not a valid publication condition. `sidecars` is left alone: the newer
/// inventory read them itself (`scan_catalog`).
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
    tokio::task::spawn_blocking(move || {
        let mut catalog = catalog::discover_fast(&dirs, &shards);
        // Sidecars go with the inventory, not with the enrichment: the
        // enrichment only runs while some model still lacks its headers, so a
        // list filled only there vanished on the next refresh. Headers are
        // cached by path, size and mtime, so after the first read this costs a
        // `stat` per file.
        catalog.sidecars = catalog::discover_sidecars(&catalog.sidecar_roots);
        catalog
    })
    .await
    .unwrap_or_else(|e| {
        warn!("model scan failed: {e}");
        Catalog {
            models: Vec::new(),
            shard_sets: Vec::new(),
            scanned: Vec::new(),
            metadata_complete: false,
            sidecars: Vec::new(),
            sidecar_roots: Vec::new(),
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
            warn!("catalog enrichment failed: {e}");
            let mut catalog = (*fallback).clone();
            catalog.metadata_complete = true;
            for model in &mut catalog.models {
                model.metadata_identity = catalog::current_metadata_identity(model);
                model.metadata_error = Some(format!("the catalog enrichment task failed: {e}"));
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
            "starting catalog enrichment - {} models, {} slice sets",
            partial.models.len(),
            partial.shard_sets.len()
        );
        let enriched = enrich_catalog(partial.clone()).await;
        let next = {
            let mut guard = catalog_cache().lock().await;
            let Some(current) = guard.as_mut() else {
                warn!(
                    "catalog enrichment completed in {:.1}s, but discarded: cache missing",
                    started.elapsed().as_secs_f32()
                );
                return;
            };
            if current.enriching.as_ref() != Some(&roots) {
                warn!(
                    "catalog enrichment completed in {:.1}s, but discarded: task replaced",
                    started.elapsed().as_secs_f32()
                );
                return;
            }

            let applied =
                reconcile_enriched_catalog(Arc::make_mut(&mut current.catalog), &enriched);
            let discarded =
                drop_discarded(Arc::make_mut(&mut current.catalog), &partial, &enriched);
            if !discarded.is_empty() {
                let catalog = Arc::make_mut(&mut current.catalog);
                catalog.metadata_complete = catalog.models.iter().all(catalog::metadata_is_current);
                info!(
                    "catalog: {} file(s) are not deployable models (projector or MTP draft): {}",
                    discarded.len(),
                    discarded.join(", ")
                );
            }
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
                "catalog metadata completed in {:.1}s - {} applied, {} failures, {} pending",
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
                    warn!("could not store the catalog history: {error:#}");
                }
                Err(error) => {
                    warn!("catalog history task failed: {error}");
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
                warn!("catalog scan completed, but discarded: cache missing");
                return;
            };
            if current.refreshing.as_ref() != Some(&roots) {
                warn!("catalog scan discarded: task replaced");
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
                    warn!("could not store the catalog history: {error:#}");
                }
                Err(error) => {
                    warn!("catalog history task failed: {error}");
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
                    sidecars: Vec::new(),
                    sidecar_roots: Vec::new(),
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

/// Finds, in this machine's catalog, the file that a range of a deployment
/// should load - when there is neither a live path nor a `part_sha256` to
/// resolve (old record, or a node that was already down during a `redeploy`).
///
/// Whole model: matches by catalog name, like `deploy::parse_whole`.
/// Slice: matches the run's group/label and the exact layer range, like
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
            "initial catalog in {:.1}s - {} models, {} slice sets; headers in the background",
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
    let workspace = cfg.workspace_root_path();
    let shard_max_bytes = cfg.shard_max_bytes();
    Json(json!({
        "model_dirs": cfg.model_dir_paths().iter().map(|d| path_entry(d, None)).collect::<Vec<_>>(),
        "shard_dir": path_entry(&shard, Some(shard_max_bytes)),
        "workspace_root": path_entry(&workspace, None),
        "shard_max_gb": cfg.shard_max_gb(),
        "using_defaults": cfg.model_dirs_are_default(),
        "detected": catalog::default_model_dirs()
            .iter().map(|d| path_entry(d, None)).collect::<Vec<_>>(),
        "default_shard_dir": crate::config::default_shard_dir().to_string_lossy(),
        "default_workspace_root": crate::config::default_workspace_root().to_string_lossy(),
        // The name of the environment variable that sets the field, when it is
        // in effect: the screen locks the field and says where the value comes from.
        "model_dirs_env": crate::config::model_dirs_from_env()
            .map(|_| crate::brand::env_var("MODEL_DIRS")),
        "shard_dir_env": crate::config::shard_dir_from_env()
            .map(|_| crate::brand::env_var("SHARD_DIR")),
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
        tracing::error!("could not store the thermal policy: {error:#}");
        return bad_request("could not store the thermal policy");
    }
    {
        let mut cfg = state.auth.write().await;
        cfg.thermal = policy;
    }
    info!(
        limit_c = policy.limit_c,
        hysteresis_c = policy.hysteresis_c,
        cooldown_secs = policy.cooldown_secs,
        "thermal policy updated from the interface"
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
            return bad_request(format!("could not write pom.json: {e}"));
        }
    }
    info!(
        cache_type_k = req.cache_type_k.as_str(),
        cache_type_v = req.cache_type_v.as_str(),
        "KV cache types updated from the interface"
    );
    get_cache_types(State(state)).await.into_response()
}

#[derive(Debug, Deserialize)]
pub struct SavePathsRequest {
    pub model_dirs: Vec<String>,
    /// Empty means "back to `<project_root>/models`".
    #[serde(default)]
    pub shard_dir: String,
    /// Empty restores the user-home workspace default (~/pom_workspace).
    #[serde(default)]
    pub workspace_root: String,
    #[serde(default)]
    pub shard_max_gb: Option<f64>,
}

/// Creates a folder the operator is saving and that does not exist yet.
///
/// Saving a path that is not there is usually the operator naming the folder
/// they want, so the screen creates it instead of refusing. Only an absolute
/// path is created: a relative one would land wherever the process runs.
fn create_missing_dir(path: &std::path::Path, trimmed: &str, what: &str) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if !path.is_absolute() {
        return Err(format!(
            "{what}: “{trimmed}” does not exist; provide the absolute path to create the folder."
        ));
    }
    std::fs::create_dir_all(path)
        .map_err(|error| format!("{what}: could not create “{trimmed}”: {error}"))?;
    info!("{what}: folder created at {trimmed}");
    Ok(())
}

/// Accepts a directory on this machine, creating it when it does not exist.
pub(super) fn check_dir(raw: &str, what: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{what}: empty path."));
    }
    let path = std::path::Path::new(trimmed);
    create_missing_dir(path, trimmed, what)?;
    if !path.is_dir() {
        return Err(format!(
            "{what}: “{trimmed}” is a file, and what goes here is the folder that contains it."
        ));
    }
    Ok(trimmed.to_string())
}

/// The workspace is shared with plugin processes, so it must be an absolute
/// directory that this container can both see and write. The probe is
/// create-new and uniquely named: validation never overwrites existing data.
pub(super) fn check_workspace_dir(raw: &str, what: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{what}: empty path."));
    }
    let path = std::path::Path::new(trimmed);
    if !path.is_absolute() {
        return Err(format!("{what}: the path must be absolute."));
    }
    create_missing_dir(path, trimmed, what)?;
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!("{what}: could not access “{trimmed}” inside the container: {error}")
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "{what}: “{trimmed}” is a file, and what goes here is the folder that contains it."
        ));
    }
    std::fs::read_dir(path).map_err(|error| {
        format!("{what}: could not list “{trimmed}” inside the container: {error}")
    })?;

    let probe = path.join(format!(
        ".pom-workspace-write-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|error| {
            format!("{what}: the folder “{trimmed}” is not writable inside the container: {error}")
        })?;
    if let Err(error) = std::fs::remove_file(&probe) {
        return Err(format!(
            "{what}: could not clean up the write test in “{trimmed}”: {error}"
        ));
    }
    Ok(trimmed.to_string())
}

fn paths_overlap(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

pub(super) fn reject_workspace_overlap(
    model_dirs: &[String],
    shard_dir: &str,
    workspace_root: &str,
) -> Result<(), String> {
    let workspace = std::path::Path::new(workspace_root)
        .canonicalize()
        .map_err(|e| format!("Workspace: could not resolve the path: {e}"))?;
    let shard = std::path::Path::new(shard_dir)
        .canonicalize()
        .map_err(|e| format!("Sliced models folder: could not resolve the path: {e}"))?;
    if paths_overlap(&workspace, &shard) {
        return Err(
            "the workspace cannot be the same as the slices folder, nor sit inside or contain it"
                .to_string(),
        );
    }
    for model in model_dirs {
        let model = std::path::Path::new(model)
            .canonicalize()
            .map_err(|e| format!("Whole models folder: could not resolve {model}: {e}"))?;
        if paths_overlap(&workspace, &model) {
            return Err(
                "the workspace cannot be the same as a whole models folder, nor sit inside or contain it"
                    .to_string(),
            );
        }
    }
    Ok(())
}

#[test]
fn workspace_dir_validation_requires_an_absolute_writable_directory_and_creates_a_missing_one() {
    let root = tempfile::tempdir().unwrap();
    assert!(check_workspace_dir("relative", "Workspace").is_err());
    let missing = root.path().join("missing").join("nested");
    assert!(check_workspace_dir(&missing.to_string_lossy(), "Workspace").is_ok());
    assert!(missing.is_dir());
    let file = root.path().join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    assert!(check_workspace_dir(&file.to_string_lossy(), "Workspace").is_err());
    assert_eq!(
        check_workspace_dir(&root.path().to_string_lossy(), "Workspace").unwrap(),
        root.path().to_string_lossy()
    );
}

#[test]
fn a_missing_storage_folder_is_created_on_save_but_only_from_an_absolute_path() {
    let root = tempfile::tempdir().unwrap();
    let models = root.path().join("new-models");
    assert!(check_dir(&models.to_string_lossy(), "Whole models folder").is_ok());
    assert!(models.is_dir());
    assert!(check_dir("relative-folder-that-should-not-exist", "Pasta").is_err());
    assert!(!std::path::Path::new("relative-folder-that-should-not-exist").exists());
    let file = root.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(check_dir(&file.to_string_lossy(), "Pasta").is_err());
}

pub(super) fn reject_nested_shard_dir(
    model_dirs: &[String],
    shard_dir: &str,
) -> Result<(), String> {
    let shard = std::path::Path::new(shard_dir)
        .canonicalize()
        .map_err(|e| format!("Sliced models folder: could not resolve the path: {e}"))?;
    for model in model_dirs {
        let model = std::path::Path::new(model)
            .canonicalize()
            .map_err(|e| format!("Whole models folder: could not resolve {model}: {e}"))?;
        if shard == model || shard.starts_with(&model) {
            return Err(
                "the sliced models folder cannot be the same as or sit inside a whole models folder".to_string(),
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
        match check_dir(raw, "Whole models folder") {
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
        match check_dir(&req.shard_dir, "Sliced models folder") {
            Ok(p) => Some(p),
            Err(e) => return bad_request(e),
        }
    };

    // `POM_MODEL_DIRS`/`POM_SHARD_DIR` win over `pom.json`: with the variable
    // set, its field is not written (the screen shows the variable's value,
    // and saving it would copy it into the file) and the checks use the value
    // that is actually in effect.
    let model_dirs_env = crate::config::model_dirs_from_env();
    let shard_dir_env = crate::config::shard_dir_from_env();
    let effective_model_dirs: Vec<String> = match &model_dirs_env {
        Some(dirs) => dirs
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect(),
        None => model_dirs.clone(),
    };
    let effective_shard_dir = match &shard_dir_env {
        Some(dir) => dir.to_string_lossy().into_owned(),
        None => shard_dir.clone().unwrap_or_else(|| {
            crate::config::default_shard_dir()
                .to_string_lossy()
                .into_owned()
        }),
    };
    if let Err(e) = reject_nested_shard_dir(&effective_model_dirs, &effective_shard_dir) {
        return bad_request(e);
    }
    let workspace_root = if req.workspace_root.trim().is_empty() {
        None
    } else {
        match check_workspace_dir(&req.workspace_root, "Workspace") {
            Ok(path) => Some(path),
            Err(e) => return bad_request(e),
        }
    };
    let effective_workspace_root = workspace_root.clone().unwrap_or_else(|| {
        crate::config::default_workspace_root()
            .to_string_lossy()
            .into_owned()
    });
    if let Err(e) = check_workspace_dir(&effective_workspace_root, "Workspace") {
        return bad_request(e);
    }
    if let Err(e) = reject_workspace_overlap(
        &effective_model_dirs,
        &effective_shard_dir,
        &effective_workspace_root,
    ) {
        return bad_request(e);
    }
    let shard_max_gb = match req.shard_max_gb {
        None => crate::storage::DEFAULT_SHARD_MAX_GB,
        Some(value) if value.is_finite() && value > 0.0 => value,
        Some(_) => {
            return bad_request("the limit of the sliced models folder must be greater than zero");
        }
    };

    {
        let mut cfg = state.auth.write().await;
        if model_dirs_env.is_none() {
            cfg.model_dirs = Some(model_dirs);
        }
        if shard_dir_env.is_none() {
            cfg.shard_dir = shard_dir;
        }
        cfg.workspace_root = workspace_root;
        cfg.shard_max_gb = shard_max_gb;
        if let Err(e) = cfg.save() {
            return bad_request(format!("could not write pom.json: {e}"));
        }
        // An emptied field returns to a default that may not exist yet.
        for (dir, error) in cfg.create_default_storage_dirs() {
            tracing::warn!(
                "could not create the default folder {}: {error}",
                dir.display()
            );
        }
    }

    if let Err(error) = crate::plugins::refresh_host_context(&state).await {
        tracing::warn!(%error, "plugin context was not updated after saving the workspace");
    }

    invalidate_catalog(state.database.clone()).await;
    info!("model folders updated from the interface");
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
            None => return bad_request(format!("“{raw}” is not a valid address.")),
        }
    };

    {
        let mut cfg = state.auth.write().await;
        cfg.advertise_addr = normalized.clone();
        if let Err(e) = cfg.save() {
            return bad_request(format!("could not write pom.json: {e}"));
        }
    }

    state.network.set_advertise_addr(normalized.as_deref());
    info!("advertised address updated from the interface: {normalized:?}");

    Json(json!({
        "advertise_addr": normalized,
        "effective_address": state.network.announce_addr(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SaveNodeNameRequest {
    /// Empty means "go by the system hostname again".
    #[serde(default)]
    pub node_name: String,
}

/// Longest name a machine may take: what the discovery card already keeps.
const NODE_NAME_MAX_CHARS: usize = 64;

/// Renames this machine and applies it live, with no restart: the next
/// announce carries the new name, so every peer and screen follows. It is
/// only a label - the machine's identity is its key (`node_id`), which a new
/// name does not touch.
pub(super) async fn save_node_name(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveNodeNameRequest>,
) -> Response {
    let name = req.node_name.trim();
    if name.chars().count() > NODE_NAME_MAX_CHARS {
        return bad_request(format!(
            "the machine name has at most {NODE_NAME_MAX_CHARS} characters."
        ));
    }
    if name.chars().any(char::is_control) {
        return bad_request("the machine name cannot have control characters.".to_string());
    }
    let node_name = (!name.is_empty()).then(|| name.to_string());

    {
        let mut cfg = state.auth.write().await;
        cfg.node_name = node_name.clone();
        if let Err(e) = cfg.save() {
            return bad_request(format!("could not write pom.json: {e}"));
        }
    }

    let hostname = match node_name.clone() {
        Some(name) => name,
        None => sysmon::sample().await.hostname,
    };
    state.network.set_hostname(hostname.clone());
    state
        .cluster_logs
        .set_identity(state.network.node_id.clone(), hostname.clone());
    info!("machine name updated from the interface: {hostname}");

    Json(json!({ "node_name": node_name, "hostname": hostname })).into_response()
}

/// The relay options of this machine and what it does with them.
pub(super) async fn get_relay(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    Json(state.network.relay_gate().status()).into_response()
}

/// Saves the relay options and applies them live, with no restart: new
/// relay connections follow them at once and the next announce carries the offer.
/// An option the command line fixed for this run is saved but stays fixed.
pub(super) async fn save_relay(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<crate::config::RelayConfig>,
) -> Response {
    {
        let mut cfg = state.auth.write().await;
        cfg.relay = req;
        if let Err(e) = cfg.save() {
            return bad_request(format!("could not write pom.json: {e}"));
        }
    }
    let gate = state.network.relay_gate();
    gate.set(req.everyone, req.cluster, req.transfers);
    info!("relay options updated from the interface: {req:?}");
    Json(gate.status()).into_response()
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
            Json(json!({ "error": "only the principal node decides the QUIC of the cluster." })),
        )
            .into_response();
    }
    {
        let mut cfg = state.auth.write().await;
        cfg.quic_enabled = req.quic_enabled;
        if let Err(error) = cfg.save() {
            return bad_request(format!(
                "could not write {}: {error}",
                crate::brand::config_filename()
            ));
        }
    }
    state.peer_http.set_quic(req.quic_enabled);
    crate::peer_client::set_quic_enabled(req.quic_enabled);
    info!(
        "QUIC between peers {} from the interface",
        if req.quic_enabled { "on" } else { "off" }
    );
    Json(json!({ "quic_enabled": req.quic_enabled })).into_response()
}

/// Turns TLS between peers on or off, for the whole cluster.
///
/// It needs no restart, and can be flipped as many times as the operator wants:
/// the server accepts both protocols from startup (`serve.rs`), and what changes
/// here is which of them the clients use and whether remote plaintext starts
/// being refused. The other nodes find out through the `tls_policy` ratchet,
/// within one gossip cycle (5 s), in both directions.
pub(super) async fn save_tls(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveTlsRequest>,
) -> Response {
    // Authority is checked **here**, on the server. Hiding the switch on the
    // screen of an ordinary node is convenience; this is the control.
    if !local_is_primary(&state).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "only the principal node decides the TLS of the cluster." })),
        )
            .into_response();
    }

    {
        let mut cfg = state.auth.write().await;
        cfg.tls_required = req.tls_required;
        if let Err(e) = cfg.save() {
            return bad_request(format!(
                "could not write {}: {e}",
                crate::brand::config_filename()
            ));
        }
    }

    // The order matters little, but all three have to happen together: the URL
    // scheme and the client cache, what the announcement says, and whether the
    // server starts refusing plaintext from outside.
    state.peer_http.apply_cluster_policy(req.tls_required);
    state.network.set_tls(req.tls_required);
    if let Some(t) = &state.tls_server {
        t.set_strict(req.tls_required);
    }
    info!(
        "TLS between peers {} from the interface",
        if req.tls_required { "on" } else { "off" }
    );

    Json(json!({ "tls_required": req.tls_required })).into_response()
}

#[derive(Deserialize)]
pub struct SaveClusterLogsRequest {
    pub logs_enabled: bool,
}

/// Turns the cluster's collection on or off with no restart. The principal
/// writes the policy and the other nodes adopt it only after the authenticated
/// fetch described in `cluster_logs_policy`.
pub(super) async fn save_cluster_logs(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveClusterLogsRequest>,
) -> Response {
    if !local_is_primary(&state).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "only the principal node decides the logs of the cluster." })),
        )
            .into_response();
    }
    // The admin only tells the network to collect; keeping the lines is the
    // logs plugin's, so collection is not turned on where nobody keeps them.
    if req.logs_enabled
        && !state
            .plugins
            .status(crate::plugins::LOGS_PLUGIN_CODE)
            .await
            .is_some_and(|status| status.active)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "code": "logs_plugin_required",
                "error": "activate the logs plugin on this node to turn on the cluster process logs.",
            })),
        )
            .into_response();
    }

    {
        let mut cfg = state.auth.write().await;
        cfg.cluster_logs_enabled = req.logs_enabled;
        if let Err(error) = cfg.save() {
            return bad_request(format!(
                "could not write {}: {error}",
                crate::brand::config_filename()
            ));
        }
    }

    state.network.set_cluster_logs_enabled(req.logs_enabled);
    state.cluster_logs.set_enabled(req.logs_enabled);
    info!(
        "cluster process logs {} from the interface",
        if req.logs_enabled { "on" } else { "off" }
    );
    Json(json!({ "logs_enabled": req.logs_enabled })).into_response()
}

#[derive(Deserialize, Serialize)]
pub struct SaveNetworkChatRequest {
    pub network_chat_enabled: bool,
}

/// Forwards the decision to the real principal when the screen is open on a
/// secondary admin. The administrative chain remains the authority, and the
/// destination writes the value to its own pom.json before announcing it.
async fn forward_network_chat_to_primary(
    state: &GatewayState,
    req: &SaveNetworkChatRequest,
) -> Response {
    let primary = state.admin_trust.current_primary();
    let Some(peer) = state.all_peers().await.into_iter().find(|peer| {
        peer.node_id == primary.node_id
            && crate::identity::trust_peer_id_from_hex(&peer.pubkey).as_deref() == Some(primary.peer_id.as_str())
            && peer.pubkey == primary.pubkey
            && peer.gateway_port.is_some()
    }) else {
        return bad_request("the principal node is not available to decide the network Chat");
    };
    match state
        .control_post_json(&peer, "/api/ui/network/chat", req, PEER_TIMEOUT)
        .await
    {
        Ok(response) if response.status().is_success() => {
            Json(json!({ "network_chat_enabled": req.network_chat_enabled })).into_response()
        }
        Ok(response) => {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "the principal node refused the decision".to_string());
            (status, body).into_response()
        }
        Err(error) => bad_request(format!("the principal node is unreachable: {error:#}")),
    }
}

/// Turns the network Chat on or off for the whole cluster. The announced value
/// only triggers the authenticated fetch on the other nodes; only the real
/// principal writes the policy, and a secondary admin forwards the action to it.
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
                "could not write {}: {error}",
                crate::brand::config_filename()
            ));
        }
    }
    state
        .network
        .set_network_chat_enabled(req.network_chat_enabled);
    state.network_chat.set_enabled(req.network_chat_enabled);
    info!(
        "Network Chat {} from the interface",
        if req.network_chat_enabled {
            "on"
        } else {
            "off"
        }
    );
    Json(json!({ "network_chat_enabled": req.network_chat_enabled })).into_response()
}

pub(super) async fn models(State(state): State<GatewayState>) -> impl IntoResponse {
    let catalog = local_catalog(&state).await;
    Json(json!({
        // Not `sysmon`: that one is the raw operating system hostname and
        // ignores the `node_name` from pom.json. `network.hostname` is the name
        // this node announces to peers, and it is what the rest of the screen
        // shows - see the twin note in `node_info`. What read from here was the
        // "Send to..." selector of the slices screen, and it was the only place
        // in the application that called the machine by another name.
        "hostname": state.network.hostname(),
        "node_id": state.network.node_id,
        "models": catalog.models,
        "shard_sets": catalog.shard_sets,
        "sidecars": catalog.sidecars,
        "scanned": catalog.scanned,
        "metadata_complete": catalog.metadata_complete,
    }))
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RttReport {
    pub(super) source_node_id: String,
    pub(super) measurements: Vec<crate::route::RttMeasurement>,
}

/// Publishes only the measurements made by this node. Authentication is needed
/// because the report is part of the internal telemetry used to choose
/// inference routes.
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

    let response = state
        .control_get(peer, "/api/ui/node", PEER_TIMEOUT)
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let node = response.json::<serde_json::Value>().await.ok()?;
    reported_peer_load(&node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admin_password_can_be_defined_without_a_previous_password() {
        let hash = next_admin_password_hash(None, "", "new password")
            .expect("set the password")
            .expect("hash of the set password");

        assert!(crate::auth::verify_admin_password(&hash, "new password"));
        assert!(!hash.contains("new password"));
    }

    #[test]
    fn changing_admin_password_requires_the_previous_password() {
        let old_hash = crate::auth::hash_admin_password("old password").expect("old password");
        assert!(next_admin_password_hash(Some(&old_hash), "errada", "new password").is_err());

        let new_hash = next_admin_password_hash(Some(&old_hash), "old password", "new password")
            .expect("change the password")
            .expect("hash of the new password");
        assert!(crate::auth::verify_admin_password(
            &new_hash,
            "new password"
        ));
        assert!(!crate::auth::verify_admin_password(
            &new_hash,
            "old password"
        ));
    }

    #[test]
    fn infer_load_stays_unknown_when_the_candidate_did_not_report_it() {
        assert_eq!(reported_peer_load(&json!({ "load": 4 })), Some(4));
        assert_eq!(reported_peer_load(&json!({})), None);
        assert_eq!(reported_peer_load(&json!({ "load": -1 })), None);
    }

    #[tokio::test]
    async fn the_process_log_console_needs_the_logs_plugin() {
        let state = crate::gateway::testkit::secondary_admin_state("secondary").await;
        let response = cluster_logs(
            ConnectionAuth,
            State(state),
            Query(ClusterLogsQuery {
                deploy_id: Some("deploy-1".into()),
                limit: Some(10),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("refusal body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("refusal JSON");
        assert_eq!(body["code"], "logs_plugin_required");
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
            rtt_valid_for_secs: crate::route::RTT_MEASURE_INTERVAL.as_secs(),
            candidates: Vec::new(),
        });
    }

    let peers = state.all_peers().await;
    let mut heads = crate::pipeline::model_head_candidates(&peers, model, deploy_id);
    // a head still loading its model is not a choice: a request sent to it waits until it is ready
    heads.retain(|peer| !peer.is_warming());
    heads.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    let rtt_snapshots = state.network.rtts.snapshot_with_age();
    let rtts = rtt_snapshots
        .iter()
        .filter(|entry| entry.age < crate::route::RTT_MEASURE_INTERVAL)
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
                Some(entry) if entry.age < crate::route::RTT_MEASURE_INTERVAL => (
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
        rtt_valid_for_secs: crate::route::RTT_MEASURE_INTERVAL.as_secs(),
        candidates,
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatTelemetryRequest {
    #[serde(default)]
    pub(super) node_ids: Vec<String>,
    /// OpenAI ID of the execution. When absent, the dashboard queries the most
    /// recent completed execution that has the requested edges.
    #[serde(default)]
    pub(super) execution_id: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct ChatTelemetryMeasurement {
    pub(super) from_node_id: String,
    pub(super) to_node_id: String,
    pub(super) rtt_ms: f64,
    pub(super) transport: String,
    /// The edge goes through a circuit on a relay (p2p tunnel with no direct
    /// connection): every frame crosses an intermediate server.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) relayed: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ChatTelemetryResponse {
    pub(super) measurements: Vec<ChatTelemetryMeasurement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pipeline_cost: Option<ChatTelemetryPipelineCost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) pipeline_routes: Vec<ChatTelemetryPipelineRoute>,
    /// Measured speed of each peer of the active model (test on serving and
    /// real traffic), coming from its `/health`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) peer_perf: HashMap<String, ChatTelemetryPeerPerf>,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct ChatTelemetryPeerPerf {
    pub(super) prefill_tokens_per_sec: Option<f64>,
    pub(super) decode_tokens_per_sec: Option<f64>,
    pub(super) active_generations: u32,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct ChatTelemetryPipelineCost {
    pub(super) legs_ms: Vec<Option<f64>>,
    pub(super) compute_ms: Option<f64>,
}

#[derive(Debug, Serialize, PartialEq, Clone)]
pub(super) struct ChatTelemetryPipelineRoute {
    /// Full path, including the virtual gateway on the return.
    pub(super) node_ids: Vec<String>,
    /// Gateway -> head, handoffs and tail -> gateway, in that order.
    pub(super) legs_ms: Vec<Option<f64>>,
    /// Sum of the legs when all were measured; does not include compute.
    pub(super) rtt_ms: Option<f64>,
    /// Compute of one decode step summed over the route's peers, when all
    /// have a measured speed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) compute_ms: Option<f64>,
    /// Network paid per token in the chosen return mode. It is not the sum of
    /// the legs: in the direct reply each leg costs half an RTT, and in the ring
    /// the tail's return is not paid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) network_ms: Option<f64>,
    /// `direct` or `ring`, as the planner would decide for this route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) return_mode: Option<crate::fastest::ReturnMode>,
    /// One decode token through the route: network in the chosen mode plus compute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) total_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tokens_per_sec: Option<f64>,
    /// The gateway was appended at the end as the tail's return; false when it
    /// serves the tail itself and the last edge is a forward handoff.
    #[serde(skip)]
    pub(super) returns_to_gateway: bool,
    /// Some leg of the route goes through a relay; the route needs to be reviewed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) relayed: bool,
    /// Which legs, as `[from, to]`. The first may be the gateway's own: it
    /// holds only for requests that enter through this node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) relayed_legs: Vec<[String; 2]>,
    /// The tail's direct return would go through a relay, and the chosen mode is
    /// the ring: POM bypasses the relay by returning the reply over the forward
    /// connections.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) relay_bypassed_by_ring: bool,
}

/// The `from -> to` leg is pinned to the p2p tunnel and `from` only reaches
/// `to` through a circuit on a relay. `returns` picks the gateway plane (the
/// tail's return) instead of the inference one.
fn leg_is_relayed(
    from: &str,
    to: &str,
    returns: bool,
    inputs: &crate::fastest::CostInputs,
) -> bool {
    inputs.perf.get(from).is_some_and(|perf| {
        let kinds = if returns {
            &perf.gateway_route_kinds
        } else {
            &perf.route_kinds
        };
        kinds.get(to).map(String::as_str) == Some("tunnel") && perf.relayed_peers.contains(to)
    })
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

/// Produces every complete route of the model, not only the one chosen by the
/// planner, estimated with the same sources and the same arithmetic as the
/// planner (`fastest::describe_chain`): RTT of each peer's pinned routes,
/// measured speed and return mode. That is why the card shows the same value on
/// any node of the network. The order is the lowest time per token first, then
/// the ID vector as a stable tie-break.
pub(super) fn pipeline_routes_for(
    peers: &[crate::network::PeerInfo],
    model: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    inputs: &crate::fastest::CostInputs,
) -> Vec<ChatTelemetryPipelineRoute> {
    if model.is_empty() || total_layers == 0 {
        return Vec::new();
    }
    let candidates = crate::pipeline::model_pipeline_candidates(peers, model, deploy_id);
    let mut paths = Vec::new();
    collect_pipeline_route_paths(&candidates, 0, total_layers, &mut Vec::new(), &mut paths);

    let mut routes = paths
        .into_iter()
        .filter_map(|path| {
            let chain = path
                .iter()
                .map(|node_id| {
                    candidates
                        .iter()
                        .copied()
                        .find(|peer| &peer.node_id == node_id)
                })
                .collect::<Option<Vec<_>>>()?;
            let view = crate::fastest::describe_chain(&chain, inputs);
            // The gateway is a virtual return endpoint, except when it also
            // serves the terminal slice. In that case the pipeline path
            // already ends at the gateway and appending it would create a
            // fake self-hop and show the same node twice in the card.
            let mut node_ids = path;
            let returns_to_gateway = node_ids.last().map(String::as_str) != Some(inputs.requester);
            if returns_to_gateway {
                node_ids.push(inputs.requester.to_owned());
            }
            let rtt_ms = view
                .legs_ms
                .iter()
                .copied()
                .collect::<Option<Vec<_>>>()
                .map(|legs| legs.into_iter().sum());
            let total_ms = view.decode_ms_per_token;
            let mut relayed_legs = std::iter::once(inputs.requester)
                .chain(chain.iter().map(|peer| peer.node_id.as_str()))
                .collect::<Vec<_>>()
                .windows(2)
                .filter(|pair| leg_is_relayed(pair[0], pair[1], false, inputs))
                .map(|pair| [pair[0].to_owned(), pair[1].to_owned()])
                .collect::<Vec<_>>();
            // The tail's return only counts when the chosen mode uses it: in
            // the ring the reply returns over the forward connections, and the
            // relay of the direct return is bypassed.
            let return_relayed = returns_to_gateway
                && chain.last().is_some_and(|tail| {
                    leg_is_relayed(&tail.node_id, inputs.requester, true, inputs)
                });
            let ring = view.return_mode == Some(crate::fastest::ReturnMode::Ring);
            if return_relayed && !ring {
                if let Some(tail) = chain.last() {
                    relayed_legs.push([tail.node_id.clone(), inputs.requester.to_owned()]);
                }
            }
            let relay_bypassed_by_ring = return_relayed && ring;
            let relayed = !relayed_legs.is_empty();
            Some(ChatTelemetryPipelineRoute {
                node_ids,
                legs_ms: view.legs_ms,
                rtt_ms,
                compute_ms: view.compute_ms,
                network_ms: view.network_ms,
                return_mode: view.return_mode,
                total_ms,
                tokens_per_sec: total_ms
                    .filter(|total| *total > 0.0)
                    .map(|total| (1000.0 / total * 10.0).round() / 10.0),
                returns_to_gateway,
                relayed,
                relayed_legs,
                relay_bypassed_by_ring,
            })
        })
        .collect::<Vec<_>>();
    routes.sort_by(|left, right| {
        match (
            left.total_ms.or(left.rtt_ms),
            right.total_ms.or(right.rtt_ms),
        ) {
            (Some(left), Some(right)) => left.total_cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.node_ids.cmp(&right.node_ids))
    });
    routes
}

/// RTT and transport of the pinned routes on each edge of the model's routes,
/// in the format of the execution measurements. It is network information: the
/// source peer measures the edge and publishes it in `/health`, so any node
/// sees the same value, whether or not it was the gateway of an execution. The
/// tail's return uses the `gateway` destination, as in the trace.
pub(super) fn planned_route_measurements(
    routes: &[ChatTelemetryPipelineRoute],
    inputs: &crate::fastest::CostInputs,
) -> Vec<ChatTelemetryMeasurement> {
    let transport = |kind: Option<&String>| match kind.map(String::as_str) {
        Some("tunnel") => "p2p-tunnel".to_owned(),
        Some(kind) => kind.to_owned(),
        None => String::new(),
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for route in routes {
        let appended = route.returns_to_gateway;
        let last = route.node_ids.len().saturating_sub(1);
        for (index, pair) in route.node_ids.windows(2).enumerate() {
            let (from, to) = (&pair[0], &pair[1]);
            let returns = appended && index + 1 == last;
            let to_key = if returns {
                "gateway".to_owned()
            } else {
                to.clone()
            };
            if from == to || !seen.insert((from.clone(), to_key.clone())) {
                continue;
            }
            let perf = inputs.perf.get(from);
            let (rtt, kind) = if returns {
                (
                    perf.and_then(|p| p.gateway_rtt_ms.get(to)).copied(),
                    perf.and_then(|p| p.gateway_route_kinds.get(to)),
                )
            } else {
                (
                    perf.and_then(|p| p.route_rtt_ms.get(to)).copied(),
                    perf.and_then(|p| p.route_kinds.get(to)),
                )
            };
            if let Some(rtt_ms) = rtt {
                out.push(ChatTelemetryMeasurement {
                    from_node_id: from.clone(),
                    to_node_id: to_key,
                    rtt_ms,
                    transport: transport(kind),
                    relayed: leg_is_relayed(from, to, returns, inputs),
                });
            }
        }
    }
    out
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

/// Extracts the model edge measurement from the trace the execution has just
/// produced.
///
/// `hops` represents nodes, while the transport of an edge goes into the
/// downstream hop. So, in `head -> tail`, the gateway's probe in `hops[0]`
/// cannot be used to name the edge: the evidence of the edge is in
/// `hops[1].decode.transport_summary`. The `remote-stream` case adds a
/// synthetic entry at hop zero, and so the same rule still holds.
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
                    relayed: false,
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
                relayed: false,
            })
        })
        .collect()
}

/// Returns the legs in cycle order and the compute observed on the peers. A
/// missing leg stays `null`: the card does not turn incomplete telemetry into
/// zero and does not close the total from a wall clock subtraction. Without
/// `pipeline_cycle`, it keeps the old contract without inventing a partial cost.
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

/// Returns only the transport observed in the distributed execution.
///
/// `RttTable` remains the source of probes for route selection, but it is not
/// a measurement of the execution: in particular, a p2p probe must not show
/// up in the Chat or on the map when the real frame crossed over `https-link`.
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
            peer_perf: HashMap::new(),
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
            peer_perf: HashMap::new(),
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
                    tracing::warn!(%error, execution_id, "could not query the execution trace");
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
                tracing::warn!(%error, "could not query the last telemetry");
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

    let mut peer_perf = HashMap::new();
    let pipeline_routes = if request.execution_id.is_none() {
        let peers = state.all_peers().await;
        let active_selector = state.displayed_model().await;
        let (model, deploy_id) = crate::deployment::split_selector(&active_selector);
        let total_layers = crate::pipeline::model_pipeline_candidates(&peers, model, deploy_id)
            .iter()
            .map(|peer| peer.total_layers)
            .max()
            .unwrap_or(0);
        let perf = model_peer_perf(&state, &peers, model, deploy_id).await;
        peer_perf = perf
            .iter()
            .map(|(node_id, perf)| {
                let tps = |ms: Option<f64>| {
                    ms.filter(|ms| *ms > 0.0)
                        .map(|ms| (1000.0 / ms * 10.0).round() / 10.0)
                };
                (
                    node_id.clone(),
                    ChatTelemetryPeerPerf {
                        prefill_tokens_per_sec: tps(perf.prefill_ms_per_token),
                        decode_tokens_per_sec: tps(perf.decode_ms_per_step),
                        active_generations: perf.active_generations,
                    },
                )
            })
            .collect();
        // The leg that leaves this node: the transport and the relay come from
        // the route book itself, because a gateway that serves no slice has no
        // inference `/health` in the admission probe.
        let mut perf = perf;
        {
            let book = crate::route_plan::book();
            let own = perf.entry(state.network.node_id.clone()).or_default();
            if own.route_kinds.is_empty() {
                own.route_kinds = book.kind_map(crate::route_plan::Plane::Inference);
            }
            own.relayed_peers.extend(book.relayed_nodes());
        }
        // The same inputs as the planner (`plan_by_delivery_time`).
        let mut requester_rtt = state
            .network
            .rtts
            .fresh_snapshot()
            .into_iter()
            .map(|measurement| (measurement.target_node_id, measurement.rtt_ms))
            .collect::<HashMap<_, _>>();
        requester_rtt
            .extend(crate::route_plan::book().rtt_map(crate::route_plan::Plane::Inference));
        let mut local_load = HashMap::new();
        for peer in &peers {
            local_load.insert(peer.node_id.clone(), state.loads.load(&peer.node_id).await);
        }
        let inputs = crate::fastest::CostInputs {
            requester: &state.network.node_id,
            requester_rtt_ms: &requester_rtt,
            perf: &perf,
            local_load: &local_load,
            prompt_tokens: 1.0,
            decode_tokens: 1.0,
            forced_return: crate::fastest::forced_return_mode(),
        };
        let routes = pipeline_routes_for(&peers, model, deploy_id, total_layers, &inputs);
        // The dashboard shows the network, not this node's last execution: the
        // planned route is the same on any node; the trace only covers what it
        // has not measured yet.
        let planned = planned_route_measurements(&routes, &inputs);
        measurements.retain(|m| {
            !planned
                .iter()
                .any(|p| p.from_node_id == m.from_node_id && p.to_node_id == m.to_node_id)
        });
        measurements.extend(planned);
        routes
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
        peer_perf,
    })
}

/// Maximum age of a peer's probe that the Dashboard accepts without probing again.
const PANEL_PERF_MAX_AGE: Duration = Duration::from_secs(15);

/// Speed of the active model's peers. It comes from the last admission probe;
/// if some peer has not gone through one yet (no request since it came up),
/// the probe runs here, with the same short ceiling as admission.
async fn model_peer_perf(
    state: &GatewayState,
    peers: &[crate::network::PeerInfo],
    model: &str,
    deploy_id: Option<&str>,
) -> HashMap<String, crate::fastest::PeerPerf> {
    let candidates: Vec<crate::network::PeerInfo> =
        crate::pipeline::model_pipeline_candidates(peers, model, deploy_id)
            .into_iter()
            .cloned()
            .collect();
    // The admission probe only runs when this node serves a request; on a
    // node nobody uses as a gateway the snapshot aged without bound and the
    // Dashboard showed the route a peer had right after coming up.
    if candidates.iter().any(|peer| {
        state
            .admission_health
            .perf_older_than(&peer.node_id, PANEL_PERF_MAX_AGE)
    }) {
        let _ = crate::gateway::ready_for_pipeline(
            &state.peer_http,
            &state.admission_health,
            &state.network,
            candidates.clone(),
            crate::gateway::ADMISSION_PROBE_TIMEOUT,
        )
        .await;
    }
    let mut perf = state.admission_health.perf_snapshot();
    perf.retain(|node_id, _| candidates.iter().any(|peer| peer.node_id == *node_id));
    perf
}

pub(super) async fn chat_history(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    match state.database.chat_sessions() {
        Ok(sessions) => Json(sessions).into_response(),
        Err(error) => {
            tracing::error!("could not read the chat history: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not read the chat history" })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct NetworkChatQuery {
    /// `network` (default) or `lobby`. Any other value reads the main room.
    #[serde(default)]
    room: Option<String>,
}

pub(super) async fn network_chat_snapshot(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<NetworkChatQuery>,
) -> Response {
    let room = match query.room.as_deref() {
        Some(crate::network_lobby::LOBBY_ROOM) => crate::network_lobby::LOBBY_ROOM,
        _ => crate::network_chat::NETWORK_CHAT_ROOM,
    };
    let mut messages = match state.network_chat.history(room, 100) {
        Ok(messages) => messages,
        Err(error) => {
            tracing::error!("could not read the network Chat: {error:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not read the network Chat" })),
            )
                .into_response();
        }
    };
    for message in &mut messages {
        message.file_data.clear();
    }
    let mut participants = vec![json!({
        "nodeId": state.network.node_id,
        "hostname": state.network.hostname(),
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
        "notices": state.network_chat.notice_snapshot(),
        "dropped": state.network_chat.dropped(),
        "room": room,
        // The lobby room is read through the network Chat.
        "lobbyEnabled": state.network_chat.enabled() && state.network_chat.lobby_enabled().await,
        // Candidates talking in the lobby, with the hardware they declared.
        "lobbyGuests": state
            .network_chat
            .lobby_guests()
            .snapshot()
            .into_iter()
            .map(|(node_id, card)| json!({ "nodeId": node_id, "card": card }))
            .collect::<Vec<_>>(),
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
            Json(json!({ "error": "the network Chat token is required" })),
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
            tracing::error!("could not read the network Chat archive: {error:#}");
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
        tracing::error!("could not store the chat history: {error:#}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "could not store the chat history" })),
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
            tracing::error!("could not delete the conversation: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not delete the conversation" })),
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
            tracing::error!("could not clear the chat history: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not clear the chat history" })),
            )
                .into_response()
        }
    }
}

pub(super) fn local_rtt_measurements(state: &GatewayState) -> Vec<crate::route::RttMeasurement> {
    state.network.rtts.fresh_snapshot()
}
