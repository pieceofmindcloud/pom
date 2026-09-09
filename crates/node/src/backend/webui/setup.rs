use super::*;
// ── Setup API ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SetupState {
    pub config: AuthState,
    pub database: Arc<crate::db::PomDb>,
}

impl axum::extract::FromRef<SetupState> for AuthState {
    fn from_ref(state: &SetupState) -> Self {
        state.config.clone()
    }
}

#[derive(Debug, Serialize)]
pub struct SetupGeoResponse {
    pub countries: Vec<crate::db::GeoCountryReference>,
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
    pub suggested_advertise_addr: Option<String>,
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
    pub public_key: String,
    pub trust_filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_connection_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SetupAdminTokenResponse {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct SetupAnchorPreviewRequest {
    pub cluster_id: String,
    #[serde(default)]
    pub visibility: crate::config::NetworkVisibility,
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

async fn setup_hardware(config: AuthState) -> SetupHardwareResponse {
    let system = sysmon::sample().await;
    let gpu_detected = !system.gpus.is_empty();
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
        suggested_advertise_addr: suggested_ip.map(|ip| {
            if ip.contains(':') {
                format!("[{ip}]:{}", cfg.p2p_port)
            } else {
                format!("{ip}:{}", cfg.p2p_port)
            }
        }),
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
    }
}

pub async fn get_setup_hardware(State(config): State<AuthState>) -> impl IntoResponse {
    Json(setup_hardware(config).await)
}

pub async fn get_setup_geo_reference(State(state): State<SetupState>) -> Response {
    match state.database.geo_reference() {
        Ok(countries) => Json(SetupGeoResponse { countries }).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não foi possível ler a referência geográfica: {error:#}") })),
        )
            .into_response(),
    }
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
        quantization: Some("GGUF · referência".to_string()),
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
            Json(json!({ "error": "o nome público da rede é obrigatório" })),
        )
            .into_response();
    }
    match crate::admin_trust::local_primary_document(cluster_id.to_string()) {
        Ok((document, fingerprint, public_key)) => match serde_json::to_string_pretty(&document) {
            Ok(document) => Json(SetupAnchorPreview {
                cluster_id: cluster_id.to_string(),
                document,
                fingerprint,
                public_key,
                trust_filename: crate::config::admin_trust_filename(),
                private_connection_key: (request.visibility
                    != crate::config::NetworkVisibility::Public)
                    .then(fresh_connection_key),
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
            Json(json!({ "error": format!("não foi possível criar a raiz: {error:#}") })),
        )
            .into_response(),
    }
}

pub async fn generate_setup_admin_token() -> impl IntoResponse {
    Json(SetupAdminTokenResponse {
        token: crate::webui::new_admin_access_token(),
    })
}

pub async fn get_setup_health(
    State(config): State<AuthState>,
    Json(request): Json<SetupHealthRequest>,
) -> impl IntoResponse {
    let hardware = setup_hardware(config).await;
    Json(SetupHealthResponse {
        gpu_detected: hardware.gpu_detected,
        cuda_working: hardware.cuda_available,
        vulkan_working: hardware.vulkan_available,
        configuration_ready: request.configuration_ready,
        p2p_discovery_working: hardware.p2p_discovery_available,
        public_ipv6_enabled: hardware.public_ipv6_available,
    })
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
    pub admin_exposure: crate::config::AdminExposure,
    #[serde(default)]
    pub admin_access_token: String,
    #[serde(default)]
    pub tls_required: bool,
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
        "infer" => crate::profile::NodeProfile::Infer,
        "node" => crate::profile::NodeProfile::Node,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "perfil inválido" })),
            )
                .into_response();
        }
    };

    let mut network = req.network.clone();
    network.country_code = network.country_code.trim().to_ascii_uppercase();
    network.country_name = network.country_name.trim().to_string();
    network.city = network.city.trim().to_string();
    if let Err(error) = crate::config::validate_network_settings(&network) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    network.geo_label = if network.country_code.is_empty() || network.city.is_empty() {
        String::new()
    } else {
        let identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
        crate::config::geo_network_label(
            &network.country_code,
            &network.city,
            &identity.node_id.0,
        )
    };
    let visibility = network.visibility;
    let requires_connection_key = visibility != crate::config::NetworkVisibility::Public;
    if profile != crate::profile::NodeProfile::Admin
        && requires_connection_key
        && req.connection_key.trim().is_empty()
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "reason": "connection_key_required",
                "error": "redes privadas exigem a chave compartilhada da rede"
            })),
        )
            .into_response();
    }

    let trust_document = if req.trust_document.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a raiz de confiança é obrigatória" })),
        )
            .into_response();
    } else {
        if req.cluster_id.trim().is_empty() || req.trust_document.trim().is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "a raiz de confiança administrativa é obrigatória" })),
            )
                .into_response();
        }
        let document = match crate::admin_trust::parse_transported(&req.trust_document) {
            Ok(document) => document,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("raiz inválida: {error:#}") })),
                )
                    .into_response();
            }
        };
        if document.anchor.cluster_id != req.cluster_id.trim() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "a raiz pertence a outra rede" })),
            )
                .into_response();
        }
        if profile == crate::profile::NodeProfile::Admin {
            if let Err(error) = crate::admin_trust::validate_local_primary_document(
                &document,
                req.cluster_id.trim(),
            ) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("raiz não conferida: {error:#}") })),
                )
                    .into_response();
            }
        }
        let expected = crate::admin_trust::anchor_fingerprint(&document);
        if !crate::admin_trust::fingerprint_matches(&req.trust_fingerprint, &expected) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "a impressão digital da raiz não confere" })),
            )
                .into_response();
        }
        document
    };

    let expected = crate::admin_trust::anchor_fingerprint(&trust_document);
    if !crate::admin_trust::fingerprint_matches(&req.trust_fingerprint, &expected) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a impressão digital da raiz não confere" })),
        )
            .into_response();
    }
    let path = crate::config::admin_trust_path();
    if path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "este nó já tem uma raiz de confiança; reancoragem é manual" })),
        )
            .into_response();
    }
    if let Err(error) = crate::admin_trust::AdminTrust::install_document(path, trust_document) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não foi possível salvar a raiz: {error:#}") })),
        )
            .into_response();
    }

    let mut cfg = config.write().await;
    cfg.profile = profile;
    cfg.setup_pending = false;
    cfg.node_name = (!req.node_name.trim().is_empty()).then(|| req.node_name.trim().to_string());

    if profile == crate::profile::NodeProfile::Admin {
        cfg.admin_exposure = req.admin_exposure;
        cfg.admin_access_token = req.admin_exposure.requires_token().then(|| {
            if req.admin_access_token.trim().is_empty() {
                crate::webui::new_admin_access_token()
            } else {
                req.admin_access_token.trim().to_string()
            }
        });
    }

    let supplied_connection_key = req.connection_key.trim();
    // Public networks do not have a shared admission secret, but the local
    // admin surface still needs a durable credential after setup.
    cfg.connection_key = connection_key_after_setup(&cfg.connection_key, supplied_connection_key);

    if profile != crate::profile::NodeProfile::Admin {
        if !req.bootstrap.is_empty() {
            cfg.bootstrap_nodes = req
                .bootstrap
                .split(',')
                .map(crate::config::normalize_peer_addr)
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    cfg.advertise_addr =
        (!req.advertise_addr.trim().is_empty()).then(|| req.advertise_addr.trim().to_string());
    cfg.network = network;
    // TLS is an optional network policy chosen by the administrator. Joining
    // a public or private network must not silently turn it on for a node or
    // infer profile, especially because libp2p already authenticates the
    // transport.
    cfg.tls_required = profile == crate::profile::NodeProfile::Admin && req.tls_required;
    cfg.resources.ram_reserve_pct = clamp_setup_percent(req.resources.ram_reserve_pct);
    cfg.resources.cpu_reserve_pct = clamp_setup_percent(req.resources.cpu_reserve_pct);
    if let Some(limit_c) = req.thermal_limit_c {
        let mut policy = cfg.thermal;
        policy.limit_c = limit_c;
        if let Err(error) = policy.validate() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("limite térmico inválido: {error}") })),
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
    drop(cfg);
    match save_result {
        Ok(()) => {
            if let Err(error) = state.database.save_network_settings(&network_for_db) {
                warn!("não foi possível salvar os detalhes da rede no banco: {error:#}");
            }
            // Trigger restart: spawn a new instance and exit.
            // The new instance will find the config file and start in full mode.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                let e = crate::respawn::respawn_self();
                tracing::error!("respawn falhou, o nó segue no ar: {e}");
            });
            Json(json!({"saved": true, "restart": true, "message": "Configuracao salva. Reiniciando..."})).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Um blob do store do Ollama: `<raiz>/blobs/sha256-…`, com `<raiz>/manifests`
/// ao lado.
///
/// O manifesto é que dá nome ao modelo, e ele não é reescrito quando o blob é
/// apagado por fora. A conferência do `manifests/` ao lado importa porque a
/// biblioteca do LM Studio também tem uma pasta `blobs/` — vazia (ver
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
    /// Sem isto, avisos viram 409 e nada é apagado.
    #[serde(default)]
    pub force: bool,
}

/// Apaga um GGUF do disco desta máquina.
///
/// Avisa, mas não impede: quem opera decide. O que não se faz é apagar em
/// silêncio algo que está no ar — a resposta nomeia o que segura o arquivo.
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
                Json(json!({ "error": format!("arquivo não encontrado: {}", req.path) })),
            )
                .into_response()
        }
    };

    // O mesmo portão que governa o que sai da máquina governa o que some dela.
    if let Err(e) = crate::transfer::ensure_gguf(&path) {
        return bad_request(e);
    }

    let size = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut warnings = Vec::new();

    // Servindo agora: apagar tira o modelo do ar sem que nada na tela diga, e
    // o nó segue anunciando cobertura até a próxima requisição quebrar.
    if let Some(cfg) = state.runtime.config().await {
        if std::path::Path::new(&cfg.model_path)
            .canonicalize()
            .map(|p| p == path)
            .unwrap_or(false)
        {
            warnings.push(json!({
                "kind": "serving",
                "message": format!(
                    "este nó está servindo as camadas {}-{} deste arquivo; apagar tira o modelo do ar",
                    cfg.start_layer, cfg.end_layer
                ),
            }));
        }
    }

    // Em trânsito: apagar por baixo de uma transferência em curso.
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
            "message": format!("há uma transferência em curso com este arquivo (job {id})"),
        }));
    }

    if is_ollama_blob(&path) {
        warnings.push(json!({
            "kind": "ollama_store",
            "message": "este arquivo está dentro do store do Ollama; apagá-lo deixa o manifesto do Ollama órfão. O certo lá é `ollama rm`",
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
            Json(json!({ "error": format!("não deu para apagar: {e}") })),
        )
            .into_response();
    }
    info!("🗑️  apagado do disco: {}", path.display());
    Json(json!({ "deleted": true, "path": target, "freed_bytes": size })).into_response()
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
    fn setup_percentages_stay_inside_the_operator_range() {
        assert_eq!(clamp_setup_percent(-4.0), 0.0);
        assert_eq!(clamp_setup_percent(42.5), 42.5);
        assert_eq!(clamp_setup_percent(140.0), 100.0);
        assert_eq!(clamp_setup_percent(f64::NAN), 0.0);
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
        assert_eq!(connection_key_after_setup("old-local-key", "new-key"), "new-key");
        let generated = connection_key_after_setup("", "");
        assert!(!generated.is_empty());
    }
}
