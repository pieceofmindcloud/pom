use super::*;
// ── Setup API ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SetupState {
    pub config: AuthState,
    pub database: Arc<crate::db::PomDb>,
    pub local_discovery: Arc<crate::local_discovery::LocalDiscoveryRuntime>,
}

impl axum::extract::FromRef<SetupState> for AuthState {
    fn from_ref(state: &SetupState) -> Self {
        state.config.clone()
    }
}

#[derive(Debug, Serialize)]
pub struct SetupHardwareResponse {
    pub system: SystemLoad,
    pub gpu_detected: bool,
    pub cuda_available: bool,
    pub vulkan_available: bool,
    pub backend: String,
    pub p2p_discovery_available: bool,
    pub public_ipv6_available: bool,
    pub p2p_port: u16,
    pub default_model_dir: String,
    pub default_shard_dir: String,
    pub shard_max_gb: f64,
    pub thermal_limit_c: f64,
    pub ram_reserve_pct: f64,
    pub cpu_reserve_pct: f64,
    pub model_dirs: Vec<String>,
    pub shard_dir: String,
    pub node_name: Option<String>,
    pub network: crate::config::NetworkSettings,
    pub public_directory: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SetupModel {
    pub id: String,
    pub name: String,
    pub source: String,
    pub repo_id: Option<String>,
    pub path: Option<String>,
    pub size_gb: Option<f64>,
    pub quantization: Option<String>,
    pub memory_gb: Option<f64>,
    pub installed: bool,
    pub recommended: bool,
}

#[derive(Debug, Serialize)]
pub struct SetupModelsResponse {
    pub models: Vec<SetupModel>,
    pub model_dirs: Vec<String>,
    pub default_model_dir: String,
    pub default_shard_dir: String,
}

#[derive(Debug, Serialize)]
pub struct SetupAnchorPreview {
    pub cluster_id: String,
    pub document: String,
    pub fingerprint: String,
    pub trust_filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_connection_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetupAnchorPreviewRequest {
    pub cluster_id: String,
    #[serde(default)]
    pub visibility: crate::config::NetworkVisibility,
    #[serde(default)]
    pub network: crate::config::NetworkSettings,
    #[serde(default)]
    pub relay_peers: Vec<String>,
    #[serde(default)]
    pub public_directory: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetupHealthRequest {
    #[serde(default)]
    pub configuration_ready: bool,
}

#[derive(Debug, Serialize)]
pub struct SetupHealthResponse {
    pub gpu_detected: bool,
    pub cuda_working: bool,
    pub vulkan_working: bool,
    pub configuration_ready: bool,
    pub p2p_discovery_working: bool,
    pub public_ipv6_enabled: bool,
    pub firewall_allowed: bool,
    pub firewall_supported: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetupAutoDiscoveryStartRequest {
    pub trust_document: String,
    #[serde(default)]
    pub connection_key: String,
}

#[derive(Debug, Serialize)]
pub struct SetupAutoDiscoveryStatus {
    pub enabled: bool,
    pub active: bool,
    pub searching: bool,
    pub found: bool,
    pub port: u16,
    pub peers: Vec<crate::local_discovery::LocalDiscoveryPeer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<crate::local_discovery::LocalDiscoveryResult>,
}

pub(crate) fn clamp_setup_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

pub(crate) fn fresh_connection_key() -> String {
    let raw = format!("{}-{}", uuid::Uuid::new_v4(), std::process::id());
    hex::encode(Sha256::digest(raw.as_bytes()))
}

fn connection_key_after_setup(existing: &str, supplied: &str) -> String {
    if !supplied.trim().is_empty() {
        supplied.trim().to_string()
    } else if !existing.trim().is_empty() {
        existing.to_string()
    } else {
        fresh_connection_key()
    }
}

/// Signed metadata travelling in the trust document. No address goes in here:
/// whoever receives the document finds the principal by its key through pkarr,
/// and the relays by the keys in `relay_hints`.
fn trust_metadata_from_network(
    network: &crate::config::NetworkSettings,
    relay_peers: &[String],
    public_directory: &[String],
) -> anyhow::Result<crate::admin_trust::TrustNetworkMetadata> {
    let directory =
        crate::public_directory::PublicDirectorySettings::from_urls(public_directory.iter())?
            .effective_with_env()?;
    Ok(crate::admin_trust::TrustNetworkMetadata {
        public_name: network.public_name.trim().to_string(),
        organization: network.organization.trim().to_string(),
        visibility: network.visibility,
        access_policy: network.access_policy,
        relay_hints: super::trust::relay_hint_keys(relay_peers),
        directory_replicas: directory.replicas,
    })
}

pub(super) fn network_from_trust_metadata(
    metadata: &crate::admin_trust::TrustNetworkMetadata,
) -> crate::config::NetworkSettings {
    crate::config::NetworkSettings {
        public_name: metadata.public_name.clone(),
        organization: metadata.organization.clone(),
        visibility: metadata.visibility,
        access_policy: metadata.access_policy,
        ..Default::default()
    }
}

fn setup_gpu_detected(gpus: &[sysmon::GpuStat]) -> bool {
    gpus.iter().any(|gpu| {
        matches!(
            gpu.vendor,
            crate::sysmon::GpuVendor::Nvidia | crate::sysmon::GpuVendor::Amd
        )
    })
}

async fn setup_hardware(config: AuthState, public_directory: Vec<String>) -> SetupHardwareResponse {
    let system = sysmon::sample().await;
    let gpu_detected = setup_gpu_detected(&system.gpus);
    let cuda_available = cfg!(feature = "cuda")
        && system
            .gpus
            .iter()
            .any(|gpu| matches!(gpu.vendor, crate::sysmon::GpuVendor::Nvidia));
    let vulkan_available = cfg!(feature = "vulkan")
        && system
            .gpus
            .iter()
            .any(|gpu| matches!(gpu.vendor, crate::sysmon::GpuVendor::Amd));
    let backend = if cuda_available {
        "CUDA"
    } else if vulkan_available {
        "Vulkan"
    } else if cfg!(feature = "metal") {
        "Metal"
    } else if gpu_detected {
        "GPU"
    } else {
        "CPU"
    };
    let cfg = config.read().await;
    let suggested_ip = tokio::task::spawn_blocking(crate::addrs::preferred_local_advertise_ip)
        .await
        .unwrap_or(None);
    SetupHardwareResponse {
        system,
        gpu_detected,
        cuda_available,
        vulkan_available,
        backend: backend.to_string(),
        p2p_discovery_available: std::net::UdpSocket::bind("0.0.0.0:0").is_ok(),
        public_ipv6_available: suggested_ip.as_deref().is_some_and(|ip| ip.contains(':')),
        p2p_port: cfg.p2p_port,
        default_model_dir: crate::config::default_model_dir()
            .to_string_lossy()
            .into_owned(),
        default_shard_dir: crate::config::default_shard_dir()
            .to_string_lossy()
            .into_owned(),
        shard_max_gb: cfg.shard_max_gb(),
        thermal_limit_c: cfg.thermal.limit_c,
        ram_reserve_pct: cfg.resources.ram_reserve_pct,
        cpu_reserve_pct: cfg.resources.cpu_reserve_pct,
        model_dirs: cfg
            .model_dir_paths()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        shard_dir: cfg.shard_dir_path().to_string_lossy().into_owned(),
        node_name: cfg.node_name.clone(),
        network: cfg.network.clone(),
        public_directory,
    }
}

pub async fn get_setup_hardware(State(state): State<SetupState>) -> impl IntoResponse {
    let public_directory = state
        .database
        .public_directory_settings()
        .ok()
        .flatten()
        .unwrap_or_default()
        .effective_with_env()
        .map(|settings| settings.replicas)
        .unwrap_or_default();
    Json(setup_hardware(state.config, public_directory).await)
}

/// The wizard's reads of the offline city database; same answers as the
/// operator routes in `network.rs`.
pub async fn get_setup_geo_countries() -> Response {
    super::network::geo_countries_response()
}

pub async fn get_setup_geo_cities(Query(query): Query<super::network::GeoCitiesQuery>) -> Response {
    super::network::geo_cities_response(query)
}

pub async fn get_setup_geo_nearest(
    Query(query): Query<super::network::GeoNearestQuery>,
) -> Response {
    super::network::geo_nearest_response(query)
}

pub fn reference_models() -> Vec<SetupModel> {
    [
        ("Qwen/Qwen2.5-7B-Instruct-GGUF", "Qwen 2.5 7B", 4.7, 8.0),
        (
            "bartowski/Llama-3.2-3B-Instruct-GGUF",
            "Llama 3.2 3B",
            2.0,
            4.0,
        ),
        ("unsloth/gemma-3-4b-it-GGUF", "Gemma 3 4B", 3.1, 5.0),
    ]
    .into_iter()
    .map(|(repo_id, name, size_gb, memory_gb)| SetupModel {
        id: repo_id.to_string(),
        name: name.to_string(),
        source: "huggingface".to_string(),
        repo_id: Some(repo_id.to_string()),
        path: None,
        size_gb: Some(size_gb),
        quantization: Some("GGUF · reference".to_string()),
        memory_gb: Some(memory_gb),
        installed: false,
        recommended: name.contains("3B") || name.contains("4B"),
    })
    .collect()
}

pub async fn get_setup_models(State(config): State<AuthState>) -> impl IntoResponse {
    let (model_dirs, shard_dir) = {
        let cfg = config.read().await;
        (cfg.model_dir_paths(), cfg.shard_dir_path())
    };
    let scan_dirs = model_dirs.clone();
    let catalog =
        tokio::task::spawn_blocking(move || crate::catalog::discover(&scan_dirs, &shard_dir))
            .await
            .unwrap_or_else(|_| crate::catalog::discover(&[], std::path::Path::new(".")));
    let mut models = catalog
        .models
        .into_iter()
        .map(|model| {
            let size_gb = model.size_bytes as f64 / 1_073_741_824.0;
            SetupModel {
                id: model.id,
                name: model.name,
                source: "local".to_string(),
                repo_id: None,
                path: Some(model.path),
                size_gb: Some(size_gb),
                quantization: model.quant,
                memory_gb: Some((size_gb * 1.15).max(size_gb)),
                installed: true,
                recommended: false,
            }
        })
        .collect::<Vec<_>>();
    models.extend(reference_models());
    Json(SetupModelsResponse {
        models,
        model_dirs: model_dirs
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        default_model_dir: crate::config::default_model_dir()
            .to_string_lossy()
            .into_owned(),
        default_shard_dir: crate::config::default_shard_dir()
            .to_string_lossy()
            .into_owned(),
    })
}

pub async fn preview_setup_anchor(Json(request): Json<SetupAnchorPreviewRequest>) -> Response {
    let cluster_id = request.cluster_id.trim();
    if cluster_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the public network name is required" })),
        )
            .into_response();
    }
    let mut network = request.network;
    network.public_name = cluster_id.to_string();
    network.visibility = request.visibility;
    if let Err(error) = crate::config::validate_network_settings(&network) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    let metadata = match trust_metadata_from_network(
        &network,
        &request.relay_peers,
        &request.public_directory,
    ) {
        Ok(metadata) => metadata,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid network metadata: {error:#}") })),
            )
                .into_response();
        }
    };
    match crate::admin_trust::local_primary_document(cluster_id.to_string(), Some(metadata)) {
        Ok((document, fingerprint, _public_key)) => match serde_json::to_string_pretty(&document) {
            Ok(document) => Json(SetupAnchorPreview {
                cluster_id: cluster_id.to_string(),
                document,
                fingerprint,
                trust_filename: crate::config::admin_trust_filename(),
                private_connection_key: (!network.open_admission()).then(fresh_connection_key),
            })
            .into_response(),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response(),
        },
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not create the root: {error:#}") })),
        )
            .into_response(),
    }
}

pub async fn get_setup_health(
    State(state): State<SetupState>,
    Json(request): Json<SetupHealthRequest>,
) -> impl IntoResponse {
    let hardware = setup_hardware(state.config.clone(), Vec::new()).await;
    let firewall = {
        let config = state.config.read().await;
        crate::windows_firewall::setup_check(&state.database, config.p2p_port)
    };
    Json(SetupHealthResponse {
        gpu_detected: hardware.gpu_detected,
        cuda_working: hardware.cuda_available,
        vulkan_working: hardware.vulkan_available,
        configuration_ready: request.configuration_ready,
        p2p_discovery_working: hardware.p2p_discovery_available,
        public_ipv6_enabled: hardware.public_ipv6_available,
        firewall_allowed: firewall.allowed,
        firewall_supported: firewall.supported,
    })
}

pub async fn get_setup_auto_discovery_status(State(state): State<SetupState>) -> impl IntoResponse {
    let status = state.local_discovery.status().await;
    Json(SetupAutoDiscoveryStatus {
        enabled: status.enabled,
        active: status.active,
        searching: status.searching,
        found: status.found,
        port: status.port,
        peers: status.peers,
        result: state.local_discovery.peek_result().await,
    })
}

pub async fn start_setup_auto_discovery(
    State(state): State<SetupState>,
    Json(request): Json<SetupAutoDiscoveryStartRequest>,
) -> Response {
    if state.config.read().await.profile != crate::profile::NodeProfile::Admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "only the administrator creates the local standby" })),
        )
            .into_response();
    }
    if request.trust_document.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the trust document is required" })),
        )
            .into_response();
    }
    if let Err(error) = crate::admin_trust::parse_transported(&request.trust_document) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("invalid root: {error:#}") })),
        )
            .into_response();
    }
    match state
        .local_discovery
        .set_admin_material(true, request.trust_document, request.connection_key)
        .await
    {
        Ok(()) => get_setup_auto_discovery_status(State(state))
            .await
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("could not start local discovery: {error:#}") })),
        )
            .into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct SetupAutoDiscoverySearchRequest {
    /// Name the wizard shows for this machine; it goes in the request so the
    /// administrator can see who is joining.
    #[serde(default)]
    pub node_name: String,
}

pub async fn search_setup_auto_discovery(
    State(state): State<SetupState>,
    request: Option<Json<SetupAutoDiscoverySearchRequest>>,
) -> Response {
    let node_name = request
        .map(|Json(request)| request.node_name.trim().to_string())
        .filter(|name| !name.is_empty());
    let machine =
        crate::local_discovery::DiscoveredMachine::from_system(&sysmon::sample().await, node_name);
    match state.local_discovery.begin_search(Some(machine)).await {
        Ok(request_id) => (
            StatusCode::ACCEPTED,
            Json(json!({ "request_id": request_id })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not look for local admins: {error:#}") })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct SaveSetupRequest {
    pub profile: String,
    #[serde(default)]
    pub bootstrap: String,
    #[serde(default)]
    pub connection_key: String,
    #[serde(default)]
    pub advertise_addr: String,
    #[serde(default)]
    pub cluster_id: String,
    #[serde(default)]
    pub trust_document: String,
    #[serde(default)]
    pub trust_fingerprint: String,
    #[serde(default)]
    pub network: crate::config::NetworkSettings,
    #[serde(default)]
    pub resources: crate::config::ResourcePolicy,
    #[serde(default)]
    pub thermal_limit_c: Option<f64>,
    #[serde(default)]
    pub admin_password: String,
    #[serde(default)]
    pub model_dirs: Vec<String>,
    #[serde(default)]
    pub shard_dir: String,
    #[serde(default)]
    pub shard_max_gb: f64,
    #[serde(default)]
    pub setup_model: Option<String>,
    #[serde(default)]
    pub node_name: String,
    #[serde(default)]
    pub public_directory: Vec<String>,
}

/// A name a network can carry, from this machine's hostname.
fn local_network_name(hostname: &str) -> String {
    let name: String = hostname
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(64)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        brand_network_fallback()
    } else {
        name.to_string()
    }
}

fn brand_network_fallback() -> String {
    format!("{}-local", crate::brand::slug())
}

/// Gives a machine without a network one of its own, with no question asked:
/// this node is its admin, the network is named after the host, and
/// discovery is off (`NetworkVisibility::Local`), so nothing is announced and
/// no directory lists it. The operator renames it and turns discovery on
/// later, in Network settings. `config` is either blank or the preference
/// shell left by leaving a network; machine preferences in it are kept.
///
/// A trust root found on disk here belongs to a network whose configuration is
/// gone, so this node can no longer take part in it. It is never overwritten
/// or deleted: it is moved to `setup-backups/<date>/` and the node goes on
/// with a network of its own.
pub async fn bootstrap_local_network(
    mut config: crate::config::Config,
    database: &crate::db::PomDb,
) -> anyhow::Result<crate::config::Config> {
    let trust_path = crate::config::admin_trust_path();
    set_aside_orphan_trust(&trust_path)?;
    let name = match config.node_name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => local_network_name(name),
        _ => local_network_name(&sysmon::sample().await.hostname),
    };
    let network = crate::config::NetworkSettings {
        public_name: name.clone(),
        visibility: crate::config::NetworkVisibility::Local,
        ..Default::default()
    };
    crate::config::validate_network_settings(&network)?;
    let metadata = trust_metadata_from_network(&network, &[], &[])?;
    let (document, _fingerprint, _public_key) =
        crate::admin_trust::local_primary_document(name, Some(metadata))?;
    let password_hash = tokio::task::spawn_blocking(|| {
        crate::auth::hash_admin_password(crate::auth::DEFAULT_ADMIN_PASSWORD)
    })
    .await
    .context("hash the default password")??;
    crate::admin_trust::AdminTrust::install_document(trust_path, document)?;

    config.profile = crate::profile::NodeProfile::Admin;
    config.setup_pending = false;
    // Same exposure the wizard leaves: reachable on the network behind the
    // default password, which the panel asks to change on first use.
    config.admin_exposure = crate::config::AdminExposure::Network;
    config.admin_access_token = Some(password_hash);
    config.admin_password_default = true;
    config.connection_key = fresh_connection_key();
    config.bootstrap_nodes.clear();
    config.advertise_addr = None;
    config.network = network;
    if config.model_dirs.is_none() {
        config.model_dirs = Some(vec![crate::config::default_model_dir()
            .to_string_lossy()
            .into_owned()]);
    }
    if config.shard_dir.is_none() {
        config.shard_dir = Some(
            crate::config::default_shard_dir()
                .to_string_lossy()
                .into_owned(),
        );
    }
    config.save()?;
    for (dir, error) in config.create_default_storage_dirs() {
        warn!(
            "could not create the default folder {}: {error}",
            dir.display()
        );
    }
    if let Err(error) = database.save_network_settings(&config.network) {
        warn!("could not save the network details in the database: {error:#}");
    }
    Ok(config)
}

/// Moves a trust root left without its configuration, and its replay file,
/// into `setup-backups/<date>/`.
fn set_aside_orphan_trust(trust_path: &std::path::Path) -> anyhow::Result<()> {
    let present: Vec<_> = [
        trust_path.to_path_buf(),
        crate::admin_trust::replay_path_for(trust_path),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect();
    if present.is_empty() {
        return Ok(());
    }
    let backup = crate::config::project_root()
        .join("setup-backups")
        .join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    std::fs::create_dir_all(&backup)?;
    for path in &present {
        let Some(name) = path.file_name() else {
            continue;
        };
        std::fs::rename(path, backup.join(name))
            .with_context(|| format!("could not set aside {}", path.display()))?;
    }
    warn!(
        "a trust root without its network configuration was moved to {}",
        backup.display()
    );
    Ok(())
}

pub async fn install_setup_trust_anchor(
    Json(body): Json<crate::webui::InstallAnchorRequest>,
) -> Response {
    // Setup mode is served by a listener bound only to 127.0.0.1. This is the
    // transport boundary equivalent of the peer-address check in trust.rs.
    let trust = crate::admin_trust::AdminTrust::load_or_empty(crate::config::admin_trust_path());
    match crate::webui::adopt_transported_anchor(&trust, &body) {
        Ok(fingerprint) => {
            Json(json!({ "anchored": true, "fingerprint": fingerprint })).into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{error:#}") })),
        )
            .into_response(),
    }
}

pub async fn save_setup(
    State(state): State<SetupState>,
    Json(req): Json<SaveSetupRequest>,
) -> impl IntoResponse {
    let config = state.config;
    let profile = match req.profile.as_str() {
        "admin" => crate::profile::NodeProfile::Admin,
        "visitor" => crate::profile::NodeProfile::Visitor,
        "worker" => crate::profile::NodeProfile::Worker,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid profile" })),
            )
                .into_response();
        }
    };

    let trust_document = if req.trust_document.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the trust root is required" })),
        )
            .into_response();
    } else {
        match crate::admin_trust::parse_transported(&req.trust_document) {
            Ok(document) => document,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid root: {error:#}") })),
                )
                    .into_response();
            }
        }
    };
    let document_cluster_id = trust_document.anchor.cluster_id.clone();
    if profile == crate::profile::NodeProfile::Admin {
        if trust_document.setup.is_none() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "the administrative root must carry the network data" })),
            )
                .into_response();
        }
        if req.cluster_id.trim().is_empty() || req.cluster_id.trim() != document_cluster_id {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "the root belongs to another network" })),
            )
                .into_response();
        }
        if let Err(error) = crate::admin_trust::validate_local_primary_document(
            &trust_document,
            req.cluster_id.trim(),
        ) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("root not verified: {error:#}") })),
            )
                .into_response();
        }
    }
    // `bootstrap` and `advertise_addr` are local hints typed by the operator
    // (they may be DNS names), never read from the trust document.
    let (mut network, directory_inputs, relay_hints) =
        if profile == crate::profile::NodeProfile::Admin {
            (
                req.network.clone(),
                req.public_directory.clone(),
                Vec::new(),
            )
        } else {
            let Some(metadata) = trust_document.setup.as_ref() else {
                return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "the root does not contain the initial network settings" })),
            )
                .into_response();
            };
            (
                network_from_trust_metadata(metadata),
                metadata.directory_replicas.clone(),
                metadata.relay_hints.clone(),
            )
        };
    let bootstrap = req.bootstrap.trim().to_string();
    let advertise_addr = req.advertise_addr.clone();
    network.public_name = network.public_name.trim().to_string();
    // Only the machine that creates the network decides its location. A
    // joining node starts from the signed descriptor, which carries none, and
    // adopts the rest of the settings from the principal after its first
    // authenticated announce.
    if profile == crate::profile::NodeProfile::Admin {
        if let Err(error) = crate::geo::apply_location(&mut network).and_then(|()| {
            crate::config::validate_network_settings(&network).map_err(|error| error.to_string())
        }) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    }
    let public_directory =
        match crate::public_directory::PublicDirectorySettings::from_urls(directory_inputs.iter())
            .and_then(|settings| settings.effective_with_env())
        {
            Ok(settings) => settings,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid public directory ticket: {error:#}") })),
                )
                    .into_response();
            }
        };
    network.geo_label = if network.country_code.is_empty() || network.city.is_empty() {
        String::new()
    } else {
        let identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
        crate::config::geo_network_label(&network.country_code, &network.city, &identity.node_id.0)
    };
    let requires_connection_key = !network.open_admission();
    if profile != crate::profile::NodeProfile::Admin
        && requires_connection_key
        && req.connection_key.trim().is_empty()
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "reason": "connection_key_required",
                "error": "private networks require the shared network key"
            })),
        )
            .into_response();
    }

    // The interface leaves the first access open to the network with the default
    // password: the wizard does not ask for a password, and the panel asks for
    // the change on first use. A password sent explicitly remains valid.
    let admin_password = if req.admin_password.is_empty() {
        crate::auth::DEFAULT_ADMIN_PASSWORD.to_string()
    } else {
        req.admin_password.clone()
    };
    let admin_password_default = admin_password == crate::auth::DEFAULT_ADMIN_PASSWORD;
    let admin_password_hash = match tokio::task::spawn_blocking(move || {
        crate::auth::hash_admin_password(&admin_password)
    })
    .await
    {
        Ok(Ok(hash)) => hash,
        Ok(Err(error)) => return bad_request(error.to_string()),
        Err(error) => return bad_request(format!("could not protect the password: {error}")),
    };

    let expected = crate::admin_trust::anchor_fingerprint(&trust_document);
    if !crate::admin_trust::fingerprint_matches(&req.trust_fingerprint, &expected) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the root fingerprint does not match" })),
        )
            .into_response();
    }
    let path = crate::config::admin_trust_path();
    if path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "this node already has a trust root; re-anchoring is manual" })),
        )
            .into_response();
    }
    if let Err(error) = crate::admin_trust::AdminTrust::install_document(path, trust_document) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not save the root: {error:#}") })),
        )
            .into_response();
    }

    let mut cfg = config.write().await;
    cfg.profile = profile;
    cfg.setup_pending = false;
    cfg.node_name = (!req.node_name.trim().is_empty()).then(|| req.node_name.trim().to_string());

    cfg.admin_exposure = crate::config::AdminExposure::Network;
    cfg.admin_access_token = Some(admin_password_hash);
    cfg.admin_password_default = admin_password_default;

    let supplied_connection_key = req.connection_key.trim();
    // Public networks do not have a shared admission secret, but the local
    // admin surface still needs a durable credential after setup.
    cfg.connection_key = connection_key_after_setup(&cfg.connection_key, supplied_connection_key);

    for relay in relay_hints {
        if !cfg.relay_peers.contains(&relay) {
            cfg.relay_peers.push(relay);
        }
    }
    if profile != crate::profile::NodeProfile::Admin {
        if !bootstrap.is_empty() {
            cfg.bootstrap_nodes = bootstrap
                .split(',')
                .map(crate::config::normalize_peer_addr)
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    cfg.advertise_addr = (profile == crate::profile::NodeProfile::Admin
        && !advertise_addr.trim().is_empty())
    .then(|| advertise_addr.trim().to_string());
    cfg.network = network;
    cfg.resources.ram_reserve_pct = clamp_setup_percent(req.resources.ram_reserve_pct);
    cfg.resources.cpu_reserve_pct = clamp_setup_percent(req.resources.cpu_reserve_pct);
    if let Some(limit_c) = req.thermal_limit_c {
        let mut policy = cfg.thermal;
        policy.limit_c = limit_c;
        if let Err(error) = policy.validate() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid thermal limit: {error}") })),
            )
                .into_response();
        }
        cfg.thermal = policy;
    }
    cfg.model_dirs = Some(if req.model_dirs.is_empty() {
        vec![crate::config::default_model_dir()
            .to_string_lossy()
            .into_owned()]
    } else {
        req.model_dirs
    });
    cfg.shard_dir = Some(if req.shard_dir.trim().is_empty() {
        crate::config::default_shard_dir()
            .to_string_lossy()
            .into_owned()
    } else {
        req.shard_dir.trim().to_string()
    });
    if req.shard_max_gb.is_finite() && req.shard_max_gb > 0.0 {
        cfg.shard_max_gb = req.shard_max_gb;
    }
    cfg.setup_model = req.setup_model;

    let network_for_db = cfg.network.clone();
    let save_result = cfg.save();
    if save_result.is_ok() {
        for (dir, error) in cfg.create_default_storage_dirs() {
            warn!(
                "could not create the default folder {}: {error}",
                dir.display()
            );
        }
    }
    drop(cfg);
    match save_result {
        Ok(()) => {
            if let Err(error) = state.database.save_network_settings(&network_for_db) {
                warn!("could not save the network details in the database: {error:#}");
            }
            if let Err(error) = state
                .database
                .save_public_directory_settings(&public_directory)
            {
                warn!("could not save the public directory tickets: {error:#}");
            }
            // Trigger restart: spawn a new instance and exit.
            // The new instance will find the config file and start in full mode.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                let e = crate::respawn::respawn_self();
                tracing::error!("respawn failed, the node stays up: {e}");
            });
            Json(json!({"saved": true, "restart": true, "message": "Configuration saved. Restarting..."})).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// A blob of the Ollama store: `<root>/blobs/sha256-...`, with `<root>/manifests`
/// beside it.
///
/// The manifest is what names the model, and it is not rewritten when the blob
/// is deleted from outside. Checking for the neighbouring `manifests/` matters
/// because the LM Studio library also has a `blobs/` folder - empty (see
/// `catalog::detect_source`).
pub(super) fn is_ollama_blob(path: &std::path::Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.file_name().and_then(|n| n.to_str()) != Some("blobs") {
        return false;
    }
    parent
        .parent()
        .map(|root| root.join("manifests").is_dir())
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
pub struct DeleteRequest {
    pub path: String,
    /// Without this, warnings become 409 and nothing is deleted.
    #[serde(default)]
    pub force: bool,
}

/// Deletes a GGUF from this machine's disk.
///
/// It warns but does not prevent: whoever operates decides. What is not done
/// is silently deleting something that is live - the response names what
/// holds the file.
pub(super) async fn delete_model(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeleteRequest>,
) -> Response {
    let path = match std::path::Path::new(&req.path).canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("file not found: {}", req.path) })),
            )
                .into_response()
        }
    };

    // A model file and nothing else: a GGUF (the gate that governs what
    // leaves the machine) or an image model file of the catalog.
    let model_dirs = state.auth.read().await.model_dir_paths();
    if let Err(e) = ensure_deletable_model(&path, &model_dirs) {
        return bad_request(e);
    }

    let size = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut warnings = Vec::new();

    // Serving now: deleting the model, one of its multipart files or one of
    // its active sidecars must never be silent.
    if let Some(cfg) = state.runtime.config().await {
        warnings.extend(active_resource_warnings(
            &cfg.model_path,
            cfg.mmproj_path.as_deref(),
            cfg.mtp_draft_path.as_deref(),
            cfg.start_layer,
            cfg.end_layer,
            &path,
        ));
    }

    // In transit: deleting from under a transfer in progress.
    let target = path.to_string_lossy().to_string();
    let in_flight: Vec<String> = state
        .transfers
        .jobs
        .read()
        .await
        .values()
        .filter(|j| j.path == target && (j.state == "running" || j.state == "verifying"))
        .map(|j| j.id.clone())
        .collect();
    for id in &in_flight {
        warnings.push(json!({
            "kind": "transfer",
            "message": format!("there is a transfer in progress with this file (job {id})"),
        }));
    }

    if is_ollama_blob(&path) {
        warnings.push(json!({
            "kind": "ollama_store",
            "message": "this file is inside the Ollama store; deleting it leaves the Ollama manifest orphaned. The right way there is `ollama rm`",
        }));
    }

    if !warnings.is_empty() && !req.force {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "warnings": warnings,
                "path": target,
                "size_bytes": size,
            })),
        )
            .into_response();
    }

    if let Err(e) = tokio::fs::remove_file(&path).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not delete: {e}") })),
        )
            .into_response();
    }
    info!("🗑️  deleted from disk: {}", path.display());
    Json(json!({ "deleted": true, "path": target, "freed_bytes": size })).into_response()
}

fn active_resource_warnings(
    model_path: &str,
    mmproj_path: Option<&str>,
    mtp_draft_path: Option<&str>,
    start_layer: u32,
    end_layer: u32,
    target: &std::path::Path,
) -> Vec<serde_json::Value> {
    let mut warnings = Vec::new();
    if active_multipart_file(model_path, target) {
        warnings.push(json!({
            "kind": "serving",
            "message": format!(
                "this node is serving layers {start_layer}-{end_layer} from this model; deleting the file takes the model offline"
            ),
        }));
    }
    if mmproj_path.is_some_and(|active| canonical_file_matches(active, target)) {
        warnings.push(json!({
            "kind": "serving",
            "message": "this node is using this file as its active vision projector; deleting it disables image input",
        }));
    }
    if mtp_draft_path.is_some_and(|active| canonical_file_matches(active, target)) {
        warnings.push(json!({
            "kind": "serving",
            "message": "this node is using this file as its active MTP draft; deleting it disables this draft",
        }));
    }
    warnings
}

fn canonical_file_matches(candidate: &str, target: &std::path::Path) -> bool {
    std::path::Path::new(candidate)
        .canonicalize()
        .map(|candidate| candidate == target)
        .unwrap_or(false)
}

/// Match any physical source file, not only the first file selected in the
/// runtime configuration. `declared_split_count` reads only the GGUF header;
/// the source maps the parts without loading their weights.
fn active_multipart_file(model_path: &str, target: &std::path::Path) -> bool {
    if canonical_file_matches(model_path, target) {
        return true;
    }
    if !crate::multipart::declared_split_count(std::path::Path::new(model_path))
        .ok()
        .flatten()
        .is_some_and(|count| count > 1)
    {
        return false;
    }
    crate::multipart::GgufSource::open(std::path::Path::new(model_path))
        .map(|source| {
            source.parts().iter().any(|part| {
                part.path
                    .canonicalize()
                    .map(|part| part == target)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// What the Full models screen may delete: a GGUF, or a file with an image
/// model extension inside one of the whole-model folders. The folder rule
/// keeps a generic extension like `.bin` from reaching any file on disk.
pub(crate) fn ensure_deletable_model(
    path: &std::path::Path,
    model_dirs: &[std::path::PathBuf],
) -> Result<(), String> {
    let gguf = crate::transfer::ensure_gguf(path);
    if gguf.is_ok() {
        return Ok(());
    }
    let inside = model_dirs.iter().any(|dir| {
        dir.canonicalize()
            .map(|dir| path.starts_with(dir))
            .unwrap_or(false)
    });
    if !inside {
        return gguf;
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    // Inside a model folder the extension is enough: a corrupt or cut-short
    // file is exactly the one worth deleting, and the folder is the guard.
    match extension.as_str() {
        "safetensors" | "sft" | "ckpt" | "pt" | "pth" | "bin" if path.is_file() => Ok(()),
        _ => gguf,
    }
}

pub(super) fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.into() })),
    )
        .into_response()
}

/// Unused query extractor placeholder kept out of the router; documented here
/// so future filters have an obvious home.
#[allow(dead_code)]
pub(super) type UiQuery = Query<HashMap<String, String>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_an_active_projector_or_mtp_draft_returns_a_serving_warning() {
        let dir = tempfile::tempdir().unwrap();
        let projector = dir.path().join("mmproj.gguf");
        let draft = dir.path().join("mtp.gguf");
        std::fs::write(&projector, b"projector").unwrap();
        std::fs::write(&draft, b"draft").unwrap();

        let projector_warnings = active_resource_warnings(
            "/models/model.gguf",
            Some(projector.to_str().unwrap()),
            None,
            0,
            32,
            &projector,
        );
        assert_eq!(projector_warnings.len(), 1);
        assert!(projector_warnings[0]["message"]
            .as_str()
            .unwrap()
            .contains("active vision projector"));

        let draft_warnings = active_resource_warnings(
            "/models/model.gguf",
            None,
            Some(draft.to_str().unwrap()),
            0,
            32,
            &draft,
        );
        assert_eq!(draft_warnings.len(), 1);
        assert!(draft_warnings[0]["message"]
            .as_str()
            .unwrap()
            .contains("active MTP draft"));
    }

    #[test]
    fn setup_percentages_stay_inside_the_operator_range() {
        assert_eq!(clamp_setup_percent(-4.0), 0.0);
        assert_eq!(clamp_setup_percent(42.5), 42.5);
        assert_eq!(clamp_setup_percent(140.0), 100.0);
        assert_eq!(clamp_setup_percent(f64::NAN), 0.0);
    }

    #[test]
    fn unified_apple_gpu_is_not_reported_as_a_dedicated_setup_gpu() {
        let apple = sysmon::GpuStat {
            index: 0,
            name: "Apple M4".into(),
            vendor: sysmon::GpuVendor::Apple,
            vram_total_mb: None,
            vram_used_mb: Some(128.0),
            vram_used_pct: None,
            utilization_pct: Some(5.0),
            power_watts: None,
            temperature_c: None,
        };
        let nvidia = sysmon::GpuStat {
            vendor: sysmon::GpuVendor::Nvidia,
            ..apple.clone()
        };

        assert!(!setup_gpu_detected(std::slice::from_ref(&apple)));
        assert!(setup_gpu_detected(std::slice::from_ref(&nvidia)));
    }

    #[test]
    fn reference_models_are_marked_as_remote_and_not_installed() {
        let references = reference_models();
        assert!(!references.is_empty());
        assert!(references.iter().all(|model| {
            model.source == "huggingface"
                && !model.installed
                && model
                    .repo_id
                    .as_deref()
                    .is_some_and(|repo| repo.contains('/'))
        }));
    }

    #[test]
    fn setup_keeps_or_generates_a_local_credential_without_a_public_secret() {
        assert_eq!(
            connection_key_after_setup("old-local-key", " "),
            "old-local-key"
        );
        assert_eq!(
            connection_key_after_setup("old-local-key", "new-key"),
            "new-key"
        );
        let generated = connection_key_after_setup("", "");
        assert!(!generated.is_empty());
    }

    #[test]
    fn public_network_metadata_allows_empty_manual_directory_replicas() {
        let mut network = crate::config::NetworkSettings::default();
        network.public_name = "POM-PUBLIC".into();
        network.visibility = crate::config::NetworkVisibility::Public;
        network.access_policy = crate::config::NetworkAccessPolicy::AnyoneWithAnchor;

        let metadata = trust_metadata_from_network(&network, &[], &[])
            .expect("public setup may omit manual directory replicas");

        assert!(metadata.directory_replicas.is_empty());
    }

    /// The trust document names relays by host name; a relay on a bare IP in
    /// `relay_peers` stays out.
    #[test]
    fn trust_metadata_carries_relay_names_and_no_address() {
        let mut network = crate::config::NetworkSettings::default();
        network.public_name = "POM".into();
        let metadata = trust_metadata_from_network(
            &network,
            &[
                "https://relay.example.org".into(),
                "http://198.51.100.1:52777".into(),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(metadata.relay_hints, vec!["https://relay.example.org/"]);
        let json = serde_json::to_string(&metadata).unwrap();
        assert!(!json.contains("198.51.100.1"), "{json}");
    }
}
