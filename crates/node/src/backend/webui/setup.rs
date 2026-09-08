use super::*;
// ── Setup API ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SetupKeyResponse {
    pub connection_key: String,
    pub addresses: Vec<String>,
}

pub async fn get_setup_key(State(config): State<AuthState>) -> impl IntoResponse {
    let raw = format!("{}-{}", uuid::Uuid::new_v4(), std::process::id());
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let key = hex::encode(hasher.finalize());

    {
        let mut cfg = config.write().await;
        cfg.connection_key = key.clone();
    }

    let mut addresses = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if let Ok(()) = socket.connect("8.8.8.8:80") {
            if let Ok(addr) = socket.local_addr() {
                addresses.push(format!("IPv4: {}:42777", addr.ip()));
            }
        }
    }

    Json(SetupKeyResponse {
        connection_key: key,
        addresses,
    })
}

#[derive(Deserialize)]
pub struct SaveSetupRequest {
    pub profile: String,
    #[serde(default)]
    pub bootstrap: String,
    #[serde(default)]
    pub connection_key: String,
}

pub async fn save_setup(
    State(config): State<AuthState>,
    Json(req): Json<SaveSetupRequest>,
) -> impl IntoResponse {
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
    let mut cfg = config.write().await;
    cfg.profile = profile;

    if profile != crate::profile::NodeProfile::Admin {
        if !req.connection_key.is_empty() {
            cfg.connection_key = req.connection_key.clone();
        }
        if !req.bootstrap.is_empty() {
            cfg.bootstrap_nodes = req
                .bootstrap
                .split(',')
                .map(crate::config::normalize_peer_addr)
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    match cfg.save() {
        Ok(()) => {
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
