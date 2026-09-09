use super::*;
// ── Cluster launch ───────────────────────────────────────────────────────────

/// Stop the inference server on this node, announce the node as idle,
/// and free the inference port. Called by `launch` before reconfiguring
/// peers so no stale endpoint receives inference requests mid-transition.
pub(super) async fn stop_inference(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    info!("stop-inference recebido — parando servidor neste nó");
    state.runtime.stop().await;
    Json(json!({ "stopped": true })).into_response()
}

// ── Taking a node out of service ─────────────────────────────────────────────
//
// Two different things the operator asks for, deliberately separate routes even
// though one is currently a subset of the other: unloading frees this machine's
// memory and leaves it in the cluster, ready to be given a range again; leaving
// takes the machine off the network entirely.

/// Free this machine's memory: kill the inference server, forget the range, and
/// tell the peers this node serves nothing.
///
/// Mechanically the same work `stop-inference` does during a launch, but that
/// route exists for the launch orchestration and says nothing back about what
/// was released — which is the whole answer the operator is waiting for here.
pub(super) async fn unload_model(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let unloaded = state.runtime.config().await;
    if unloaded.is_none() {
        return Json(json!({ "unloaded": false, "reason": "nenhum modelo carregado" }))
            .into_response();
    }
    state.runtime.stop().await;
    info!("modelo descarregado pela interface");
    Json(json!({
        "unloaded": true,
        "model": unloaded.as_ref().map(|c| c.model_label.clone()),
        "start_layer": unloaded.as_ref().map(|c| c.start_layer),
        "end_layer": unloaded.as_ref().map(|c| c.end_layer),
    }))
    .into_response()
}

/// Outros parâmetros para a fatia que este nó já serve.
///
/// Só os parâmetros: o que identifica a fatia (arquivo, faixa de camadas,
/// posição no pipeline) não está aqui e não pode ser mudado por esta porta.
/// Ver `NodeConfig::with_params`.
#[derive(Debug, Deserialize)]
pub struct ReconfigureRequest {
    pub params: RuntimeParams,
    /// A identidade da fatia, quando quem chama já a conhece (um `redeploy`
    /// que descarregou tudo antes de subir de novo). Ausente = o nó relê o
    /// próprio `NodeConfig` vivo, que é o caminho de sempre e só serve para
    /// uma máquina que já está servindo.
    #[serde(default)]
    pub slice: Option<ReconfigureSlice>,
}

/// O que identifica a fatia que um nó deve servir, para o caso em que ele
/// **não** está servindo nada e não há `NodeConfig` vivo de onde partir.
///
/// Preenchido pelo coordenador de um `redeploy` a partir do registro da
/// implantação mais a fotografia do que cada nó estava rodando antes do
/// unload. `model_path` é o caminho vivo fotografado; sem ele, `part_sha256`
/// resolve o arquivo local por conteúdo (`transfer::find_part`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconfigureSlice {
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default)]
    pub mmproj_path: Option<String>,
    #[serde(default)]
    pub part_sha256: Option<String>,
    pub model_label: String,
    #[serde(default)]
    pub deployment_name: String,
    #[serde(default)]
    pub deploy_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    #[serde(default = "one_u32")]
    pub peer_count: u32,
    #[serde(default)]
    pub peer_index: u32,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub single_peer: bool,
    #[serde(default)]
    pub no_embeddings: Option<bool>,
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
}

pub(super) fn one_u32() -> u32 {
    1
}

/// Recarrega o modelo desta máquina com outros parâmetros.
///
/// É o caminho de "reimplantar mexendo num parâmetro": nada é planejado, nada
/// é copiado e nenhuma faixa muda de dono - o nó relê o **mesmo** arquivo com,
/// digamos, outro `n_ctx`. Passar pelo `deploy` inteiro para isso significaria
/// re-varrer o cluster e cunhar uma implantação nova, quando o que mudou cabe
/// num `apply`.
///
/// Recusa quando não há nada carregado: reconfigurar o vazio não tem sentido, e
/// inventar uma fatia a partir de parâmetros soltos seria adivinhar qual.
pub(super) async fn reconfigure(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ReconfigureRequest>,
) -> Response {
    match reconfigure_local(state, req.params, req.slice).await {
        Ok(cfg) => Json(json!({
            "reconfigured": true,
            "model": cfg.model_label,
            "start_layer": cfg.start_layer,
            "end_layer": cfg.end_layer,
        }))
        .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// A reconfiguração em si, sem HTTP em volta.
///
/// Existe separada porque o coordenador de um `redeploy` também é um nó da
/// implantação: quando a máquina a reconfigurar é ele mesmo, chamar a própria
/// rota por rede seria dar a volta no mundo para falar consigo.
pub(crate) async fn reconfigure_local(
    state: GatewayState,
    params: RuntimeParams,
    slice: Option<ReconfigureSlice>,
) -> Result<NodeConfig, String> {
    let cfg = match slice {
        // Identidade dada: monta o `NodeConfig` a partir dela, sirva o nó algo
        // ou não. É o caminho do `redeploy` que descarrega tudo antes de subir.
        Some(slice) => {
            // Como achar o arquivo, em ordem: o caminho vivo fotografado; a
            // parte por conteúdo (`part_sha256`); o catálogo local casando
            // rótulo + faixa de camadas. Só quando as três falham é erro.
            let shard_dir = state.shard_dir().await;
            let model_path = if let Some(path) = slice.model_path.clone().filter(|p| !p.is_empty())
            {
                path
            } else if let Some(path) = slice
                .part_sha256
                .as_deref()
                .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
            {
                path.to_string_lossy().into_owned()
            } else if let Some(path) = resolve_deployment_slice_file(
                &state,
                &slice.model_label,
                slice.start_layer,
                slice.end_layer,
                slice.total_layers,
                slice.peer_count,
            )
            .await
            {
                path
            } else {
                return Err(format!(
                    "esta máquina não tem mais o arquivo da faixa L{}-{} de \"{}\" ({}); \
                     use a tela de Implantar para copiá-lo de novo",
                    slice.start_layer,
                    slice.end_layer,
                    slice.model_label,
                    slice
                        .part_sha256
                        .as_deref()
                        .map(|s| format!("hash {}", &s[..12.min(s.len())]))
                        .unwrap_or_else(|| "sem hash de parte no registro".to_string()),
                ));
            };
            let tp_none = slice.tp.is_none();
            // `from_params` deixa `inference_port` em 0; a identidade da fatia
            // não o carrega. Sem este override o servidor de inferência subia
            // em `[::]:0` (porta efêmera) num redeploy, e o túnel/HTTP direto
            // não achava mais o peer.
            let inference_port = state.network.inference_port.ok_or_else(|| {
                "este nó subiu sem --inference-port; não dá para reconfigurar a fatia".to_string()
            })?;
            NodeConfig {
                deploy_id: slice.deploy_id,
                deployment_name: slice.deployment_name,
                model_path,
                mmproj_path: slice.mmproj_path,
                model_label: slice.model_label,
                start_layer: slice.start_layer,
                end_layer: slice.end_layer,
                total_layers: slice.total_layers,
                peer_count: slice.peer_count.max(1),
                peer_index: slice.peer_index,
                primary: slice.primary,
                tp: slice.tp,
                inference_port,
                single_peer: slice.single_peer,
                no_embeddings: slice.no_embeddings.unwrap_or(params.no_embeddings),
                ..NodeConfig::from_params(&params, tp_none)
            }
        }
        // Sem identidade: só troca os parâmetros do que já está carregado.
        None => {
            let Some(atual) = state.runtime.config().await else {
                warn!("reconfigure recusado: este nó não está servindo nada");
                return Err("este nó não está servindo nada para reconfigurar".into());
            };
            atual.with_params(&params)
        }
    };

    info!(
        "reconfigurando {} (camadas {}-{}) com parâmetros novos",
        cfg.model_label, cfg.start_layer, cfg.end_layer
    );

    state
        .runtime
        .apply(cfg.clone())
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(cfg)
}

/// A qual máquina o "sair da rede" se refere.
#[derive(Debug, Default, Deserialize)]
pub struct LeaveRequest {
    /// Máquina que deve sair. Ausente, ou o próprio `node_id`, significa esta.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// Take a node off the network: unload, stop announcing, forget the peers.
///
/// The HTTP gateway keeps serving, because the way back in is this same screen.
///
/// Corpo ausente ou sem `node_id` mantém o comportamento antigo — este nó sai —,
/// que é o que o menu "Este nó" e a tela de Rede sempre pediram. Com `node_id`
/// de outra máquina o pedido é **repassado** à mesma rota lá, como
/// `dev/update` já faz: o painel só fala com o próprio nó (caminhos relativos
/// em `lib/api.ts`), e um card de peer no Painel precisa alcançar aquela
/// máquina. Quem se desliga é sempre o dono da rede daquele processo; este nó
/// não tem como detachar outro por fora.
///
/// As duas metades têm donos diferentes, e é por isso que a tabela de escopo
/// não dá conta sozinha: sair é **abdicar do próprio posto**, e nenhum perfil
/// precisa de licença para isso; mandar **outra** máquina sair é ato de
/// administrador, e o repasse viaja com a credencial interna, que o alvo não
/// julga pelo perfil de quem pediu. Sem esta trava, o escopo mais largo da
/// rota transformaria "posso sair" em "posso expulsar qualquer um".
pub(super) async fn leave_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<LeaveRequest>>,
) -> Response {
    let target = body.and_then(|Json(b)| b.node_id).unwrap_or_default();
    if !target.is_empty() && target != state.network.node_id {
        if !has_admin_authority(&state).await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "só um administrador tira outra máquina da rede",
                    "node_id": target,
                })),
            )
                .into_response();
        }
        return forward_leave(&state, &target).await;
    }

    state.runtime.stop().await;
    state.network.detach().await;
    info!("nó saiu da rede pela interface");
    Json(json!({ "detached": true })).into_response()
}

/// Autoridade administrativa efetiva deste nó. `profile: admin` sozinho não
/// concede capacidade: é a concessão assinada que decide (ver `admin_trust`).
async fn has_admin_authority(state: &GatewayState) -> bool {
    let profile = state.auth.read().await.profile;
    state
        .admin_trust
        .effective_authority(&state.authority_principal, profile)
        .is_some()
}

/// Repassa o "sair da rede" ao peer escolhido, sem `node_id` — lá ele é local.
pub(super) async fn forward_leave(state: &GatewayState, node_id: &str) -> Response {
    let Some(base) = state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.gateway_url())
    else {
        return bad_request("peer desconhecido ou sem porta de gateway anunciada");
    };
    let Ok(cliente) = state.peer_http.for_peer(node_id) else {
        return bad_request(format!("sem cliente para o peer {node_id}"));
    };
    let body = json!({});
    let request = match state
        .control_json_request(
            cliente
                .post(format!("{base}/api/ui/leave"))
                .timeout(PEER_TIMEOUT),
            "/api/ui/leave",
            &body,
        )
        .await
    {
        Ok(request) => request,
        Err(error) => return bad_request(format!("não foi possível assinar a saída: {error}")),
    };
    match request.send().await {
        Ok(r) if r.status().is_success() => {
            info!("{node_id} saiu da rede a pedido deste nó");
            Json(json!({ "detached": true, "node_id": node_id })).into_response()
        }
        Ok(r) => bad_request(format!("a máquina recusou: HTTP {}", r.status())),
        Err(e) => bad_request(format!("máquina inacessível: {e}")),
    }
}

// ── Releasing a slice, and the network taking it back up ─────────────────────

/// What the operator asked for when releasing.
#[derive(Debug, Deserialize)]
pub struct ReleaseRequest {
    /// Try to hand the slice to another node afterwards. On by default; the
    /// operator can turn it off to free the memory and leave the range down.
    #[serde(default = "default_true")]
    pub reassign: bool,
}

pub(super) fn default_true() -> bool {
    true
}

/// A node that could take over a slice: idle, and already holding the file.
pub(super) struct Candidate {
    pub(super) node_id: String,
    pub(super) hostname: String,
    pub(super) gateway: String,
    /// Where that same slice lives on *that* peer's disk.
    pub(super) path: String,
}

/// Find a peer that is serving nothing and already has this exact slice.
///
/// "Already has it" is deliberate: the point of releasing is that this machine
/// needs its memory back *now*, so the handover has to be something another node
/// can act on immediately. Copying a 10 GB slice across the link first would take
/// ~17 minutes, which is not a handover — that is what the deploy screen is for.
///
/// Matching is by layer range plus shard group, not by file name: the same slice
/// legitimately sits under different paths on different machines.
pub(super) async fn find_release_candidate(
    state: &GatewayState,
    cfg: &NodeConfig,
) -> Option<Candidate> {
    let self_id = state.network.node_id.clone();
    for peer in state.all_peers().await {
        if peer.node_id == self_id {
            continue;
        }
        let Some(base) = peer.gateway_url() else {
            continue;
        };

        let Ok(cliente) = state.peer_http.for_peer(&peer.node_id) else {
            continue;
        };

        // Busy peers are out: a node serves one range at a time.
        let Ok(request) = state
            .control_get_request(
                cliente
                    .get(format!("{base}/api/ui/node"))
                    .timeout(PEER_TIMEOUT),
                "/api/ui/node",
            )
            .await
        else {
            continue;
        };
        match request.send().await {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or(json!({}));
                if !body.get("config").map(|c| c.is_null()).unwrap_or(true) {
                    continue;
                }
            }
            _ => continue,
        }

        let Ok(request) = state
            .control_get_request(
                cliente
                    .get(format!("{base}/api/ui/models"))
                    .timeout(PEER_TIMEOUT * 3),
                "/api/ui/models",
            )
            .await
        else {
            continue;
        };
        let Ok(r) = request.send().await else {
            continue;
        };
        if !r.status().is_success() {
            continue;
        }
        let body: serde_json::Value = r.json().await.unwrap_or(json!({}));

        let found = body
            .get("shard_sets")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .flat_map(|set| {
                set.get("shards")
                    .and_then(|s| s.as_array())
                    .into_iter()
                    .flatten()
            })
            .find(|shard| {
                let range = shard.get("shard_range").and_then(|r| r.as_array());
                let matches_range = range
                    .map(|r| {
                        r.first().and_then(|v| v.as_u64()) == Some(cfg.start_layer as u64)
                            && r.get(1).and_then(|v| v.as_u64()) == Some(cfg.end_layer as u64)
                    })
                    .unwrap_or(false);
                matches_range
                    && shard
                        .get("total_layers")
                        .and_then(|v| v.as_u64())
                        .map(|t| t == cfg.total_layers as u64)
                        .unwrap_or(true)
            })
            .and_then(|shard| shard.get("path").and_then(|p| p.as_str()).map(String::from));

        if let Some(path) = found {
            return Some(Candidate {
                node_id: peer.node_id.clone(),
                hostname: peer.hostname.clone(),
                gateway: base,
                path,
            });
        }
    }
    None
}

/// Who would take this node's slice if it were released right now.
///
/// Read-only, and the reason the confirmation dialog can promise something true:
/// warning "the model will stop answering" only when it actually will beats a
/// blanket scare that the operator learns to click through.
pub(super) async fn release_candidate(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let Some(cfg) = state.runtime.config().await else {
        return Json(json!({ "serving": false })).into_response();
    };
    let candidate = find_release_candidate(&state, &cfg).await;
    Json(json!({
        "serving": true,
        "slice": {
            "model": cfg.model_label,
            "start_layer": cfg.start_layer,
            "end_layer": cfg.end_layer,
            "total_layers": cfg.total_layers,
        },
        "candidate": candidate.map(|c| json!({
            "node_id": c.node_id,
            "hostname": c.hostname,
        })),
    }))
    .into_response()
}

/// Hand this node's slice back and free its RAM/VRAM, staying in the cluster.
///
/// The release is **never refused**, even when nobody can take the slice over.
/// That is the whole point of the feature: the machine needs its memory now, and
/// a node that has to ask permission to stop has not been given the freedom the
/// operator was promised. What the caller gets instead is a straight answer about
/// what the cluster looks like afterwards — `reassigned_to` when another node
/// picked the range up, or `uncovered: true` naming the range that is now down.
///
/// An uncovered range is not silent anywhere else either: the planner already
/// refuses to build a pipeline with a hole in it, so generation fails with the
/// gap named rather than falling back to a wrong answer.
pub(super) async fn release_slice(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<ReleaseRequest>>,
) -> Response {
    let Some(cfg) = state.runtime.config().await else {
        return bad_request("este nó não está servindo nenhuma fatia");
    };
    let reassign = body.map(|Json(r)| r.reassign).unwrap_or(true);

    // Free the memory first, and unconditionally. Looking for a successor before
    // stopping would make the operator wait on the network for the one thing
    // they asked to happen immediately.
    state.runtime.stop().await;
    info!(
        "fatia L{}-{} liberada pelo nó; memória devolvida",
        cfg.start_layer, cfg.end_layer
    );

    let released = json!({
        "model": cfg.model_label,
        "start_layer": cfg.start_layer,
        "end_layer": cfg.end_layer,
        "total_layers": cfg.total_layers,
    });

    if !reassign {
        return Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": "reatribuição desligada no pedido",
        }))
        .into_response();
    }

    let Some(candidate) = find_release_candidate(&state, &cfg).await else {
        warn!(
            "fatia L{}-{} ficou descoberta: nenhum nó ocioso tem esse arquivo",
            cfg.start_layer, cfg.end_layer
        );
        return Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": "nenhum outro nó está ocioso e com esta fatia no disco",
        }))
        .into_response();
    };

    // Same shape a launch dispatches, so the successor comes up exactly as this
    // node was: same label, same position in the split, same runtime params.
    let apply_req = ApplyRequest {
        // O sucessor entra na **mesma** implantação: é a mesma faixa do mesmo
        // conjunto, só que noutra máquina.
        deploy_id: cfg.deploy_id.clone(),
        source_model_path: candidate.path.clone(),
        // O sucessor carrega exatamente o arquivo que este nó carregava, e o
        // nome de uma parte já é o hash dela: é daí que a identidade sai.
        part_sha256: std::path::Path::new(&candidate.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| crate::pom::is_sha256_hex(s))
            .map(|s| s.to_string()),
        base_name: cfg.model_label.clone(),
        model_label: Some(cfg.model_label.clone()),
        deployment_name: cfg.deployment_name.clone(),
        start_layer: cfg.start_layer,
        end_layer: cfg.end_layer,
        total_layers: cfg.total_layers,
        peer_count: cfg.peer_count,
        peer_index: cfg.peer_index,
        primary: cfg.primary,
        tp: cfg.tp.clone(),
        skip_split: true,
        use_mmproj: cfg.mmproj_path.is_some(),
        mmproj_path: cfg.mmproj_path.clone(),
        merge: None,
        params: RuntimeParams::from(&cfg),
    };

    let cliente = match state.peer_http.for_peer(&candidate.node_id) {
        Ok(c) => c,
        Err(e) => return bad_request(format!("sem cliente para {}: {e}", candidate.node_id)),
    };
    let request = match state
        .control_json_request(
            cliente.post(format!("{}/api/ui/apply", candidate.gateway)),
            "/api/ui/apply",
            &apply_req,
        )
        .await
    {
        Ok(request) => request,
        Err(error) => {
            return bad_request(format!("não foi possível assinar a reassociação: {error}"))
        }
    };
    match request.send().await {
        Ok(r) if r.status().is_success() => {
            info!(
                "fatia L{}-{} assumida por {}",
                cfg.start_layer, cfg.end_layer, candidate.node_id
            );
            Json(json!({
                "released": true, "slice": released, "uncovered": false,
                "reassigned_to": {
                    "node_id": candidate.node_id,
                    "hostname": candidate.hostname,
                },
            }))
            .into_response()
        }
        Ok(r) => {
            let code = r.status();
            let text = r.text().await.unwrap_or_default();
            warn!("{} recusou assumir a fatia: HTTP {code}", candidate.node_id);
            Json(json!({
                "released": true, "slice": released,
                "reassigned_to": null, "uncovered": true,
                "reason": format!("{} recusou assumir: HTTP {code} {text}", candidate.hostname),
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": format!("{} inacessível: {e}", candidate.hostname),
        }))
        .into_response(),
    }
}

/// Announce again and go looking for the bootstrap nodes.
pub(super) async fn rejoin_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let assignment = state.local_assignment.read().await.clone();
    match state.network.rejoin(assignment.as_ref()).await {
        Ok(()) => Json(json!({ "detached": false })).into_response(),
        Err(e) => bad_request(format!("não deu para voltar à rede: {e}")),
    }
}

/// One local slice feeding a merge on this peer: the file and the range it
/// stands for. The peer joins these into one GGUF before serving it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeSource {
    /// Path of the slice *on this peer*.
    pub source_model_path: String,
    pub start_layer: u32,
    pub end_layer: u32,
}

/// One entry per peer taking part in the launch.
#[derive(Debug, Deserialize)]
pub struct LaunchTarget {
    pub node_id: String,
    /// Path of the source model *on that peer*.
    pub source_model_path: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub skip_split: bool,
    /// Hash da parte que este alvo carrega, quando o `.pom` já a conhece. É o
    /// que dispensa o destino de adivinhar um nome de arquivo.
    #[serde(default)]
    pub part_sha256: Option<String>,
    /// Local slices to join into one file before serving. When present, the
    /// peer merges them (`splitter::merge`) and serves the union; the merge
    /// replaces `skip_split` because the file covering the range only comes
    /// into existence through the merge.
    #[serde(default)]
    pub merge: Option<Vec<MergeSource>>,
    /// How many of this peer's layers go on its GPU, overriding
    /// `params.n_gpu_layers` for this peer alone.
    ///
    /// The only knob that is honestly per-machine: the rest of `RuntimeParams`
    /// is either protocol (context window, batch, chunk) or sampling, and both
    /// have to agree across the pipeline. Offload does not — a box with a 5060
    /// Ti and a CPU-only box in the same cluster want different numbers, and
    /// before this the whole launch had to settle on one.
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    /// Onde o cache KV deste peer mora, sobrepondo `params.offload_kqv` só para
    /// ele.
    ///
    /// Pela mesma razão que o offload: o cache é dimensionado pela janela de
    /// contexto, e uma máquina com 16 GB de VRAM e uma sem GPU no mesmo
    /// pipeline não podem estar as duas certas sobre onde ele cabe.
    #[serde(default)]
    pub offload_kqv: Option<bool>,
    /// Override por alvo, calculado a partir da topologia do plano. `None`
    /// preserva o comportamento automático do peer final.
    #[serde(default)]
    pub no_embeddings: Option<bool>,
    /// Posição deste peer no pipeline, quando ela não é a posição dele nesta
    /// requisição.
    ///
    /// `launch` deduz `peer_index`/`peer_count` da lista de alvos, o que só
    /// vale quando a requisição descreve o pipeline inteiro. Quem soma uma
    /// máquina de cada vez (`deploy::add_node_endpoint`) manda um alvo e
    /// precisa dizer a posição real - sem isto o nó somado se declara o último
    /// da divisão, sobe sem captura de residual e a inferência morre no
    /// primeiro token com "no embeddings".
    #[serde(default)]
    pub peer_index: Option<u32>,
    #[serde(default)]
    pub peer_count: Option<u32>,
    /// Identidade de tensor parallelism deste alvo (caminho C). Quando presente
    /// (primary ou worker de um grupo), o apply extrai o shard/registra a
    /// coletiva em vez de só fatiar por camadas.
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
    /// The target should load the adjacent multimodal projector.
    #[serde(default)]
    pub use_mmproj: bool,
    /// Projector path on the target peer, resolved from its shard metadata.
    #[serde(default)]
    pub mmproj_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LaunchRequest {
    /// A implantação que este launch está subindo. Cunhado por
    /// `deploy::deploy_endpoint`; vazio num launch avulso, que não passou por
    /// implantação nenhuma.
    #[serde(default)]
    pub deploy_id: String,
    pub base_name: String,
    /// The identity selected in the deployment screen, propagated unchanged
    /// to every apply request. Missing values keep older callers compatible.
    #[serde(default)]
    pub model_label: Option<String>,
    /// Nome operacional exibido em jobs, status e telemetria.
    #[serde(default)]
    pub deployment_name: String,
    pub total_layers: u32,
    pub targets: Vec<LaunchTarget>,
    /// Node that should act as the entry point.
    #[serde(default)]
    pub primary_node_id: Option<String>,
    #[serde(default)]
    pub params: RuntimeParams,
    #[serde(default)]
    pub use_mmproj: bool,
}

/// The cluster-wide settings as one peer will actually receive them.
///
/// Everything in `RuntimeParams` has to agree across the pipeline — the context
/// window and the batch decide the prompt budget the gateway enforces, and the
/// sampling is only used by the last peer — except how much of its own share a
/// machine offloads to its GPU. That one is a property of the machine.
pub(crate) fn params_for(base: &RuntimeParams, target: &LaunchTarget) -> RuntimeParams {
    let mut params = base.clone();
    if let Some(gpu) = target.n_gpu_layers {
        params.n_gpu_layers = gpu;
    }
    if let Some(kqv) = target.offload_kqv {
        params.offload_kqv = kqv;
    }
    if let Some(no_embeddings) = target.no_embeddings {
        params.no_embeddings = no_embeddings;
    }
    params
}

pub(super) fn launch_no_embeddings_evaluation(
    req: &LaunchRequest,
) -> crate::capabilities::NoEmbeddingsEvaluation {
    let mut ranges = Vec::new();
    for target in &req.targets {
        if target.tp.is_none() {
            let range = (target.start_layer, target.end_layer);
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
    }
    let pipeline_count = ranges.len() as u32;
    let peers = req
        .targets
        .iter()
        .map(|target| {
            let derived_index = if target.tp.is_some() {
                Some(0)
            } else {
                ranges
                    .iter()
                    .position(|range| *range == (target.start_layer, target.end_layer))
                    .map(|value| value as u32)
            };
            crate::capabilities::PeerTopology {
                node_id: target.node_id.clone(),
                start_layer: Some(target.start_layer),
                end_layer: Some(target.end_layer),
                total_layers: Some(req.total_layers),
                peer_index: target.peer_index.or(derived_index),
                peer_count: target.peer_count.or_else(|| {
                    if target.tp.is_some() {
                        Some(1)
                    } else {
                        Some(pipeline_count)
                    }
                }),
            }
        })
        .collect::<Vec<_>>();
    crate::capabilities::evaluate_no_embeddings(&peers)
}

/// Dispatch the planned configuration to every peer, including this one.
pub(crate) async fn launch(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<LaunchRequest>,
) -> Response {
    if req.targets.is_empty() {
        return bad_request("nenhum peer selecionado");
    }
    // Checked before phase 1, which stops inference on every target: a launch
    // that cannot possibly apply must not be allowed to take the cluster down
    // on its way to failing. `deploy.rs` refuses this earlier and with the
    // offending files named; this is the guard for every other caller.
    if req.total_layers == 0 {
        return bad_request(
            "não foi informado de quantas camadas é o modelo, e as fatias não podem ser \
             atribuídas sem esse total. Fatias geradas pela tela de Fatiar gravam esse dado; \
             as antigas não, e nesse caso o total é deduzido da cobertura das fatias.",
        );
    }
    if req.use_mmproj && !req.targets.iter().any(|target| target.use_mmproj) {
        return bad_request("este modelo não tem visão: nenhum alvo recebeu mmproj");
    }
    let deployment = match normalize_deployment_name(Some(&req.deployment_name), &req.base_name) {
        Ok(name) => name,
        Err(error) => return bad_request(error),
    };
    if let Some(t) = req.targets.iter().find(|t| t.end_layer >= req.total_layers) {
        return bad_request(format!(
            "a fatia L{}-{} passa do fim de um modelo de {} camadas",
            t.start_layer, t.end_layer, req.total_layers
        ));
    }

    if req.params.no_embeddings {
        if let Err(reason) = crate::capabilities::validate_no_embeddings(
            &launch_no_embeddings_evaluation(&req),
            true,
        ) {
            return bad_request(reason);
        }
    }

    let peers = state.all_peers().await;
    let self_id = state.network.node_id.clone();
    let peer_count = req.targets.len() as u32;

    // Phase 1: stop inference on every target peer so no stale endpoint
    // receives inference requests mid-transition, and to free ports before
    // the new inference servers try to bind.
    for target in &req.targets {
        if target.node_id == self_id {
            state.runtime.stop().await;
            continue;
        }
        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == target.node_id)
            .and_then(|p| p.gateway_url())
        else {
            warn!("peer {} nao encontrado para stop-inference", target.node_id);
            continue;
        };
        let url = format!("{base}/api/ui/stop-inference");
        let Ok(cliente) = state.peer_http.for_peer(&target.node_id) else {
            warn!("sem cliente para {} em stop-inference", target.node_id);
            continue;
        };
        let request = match state
            .authorize_control_request(
                cliente.post(&url).timeout(PEER_TIMEOUT),
                "POST",
                "/api/ui/stop-inference",
                &[],
            )
            .await
        {
            Ok(request) => request,
            Err(error) => {
                warn!(
                    "não foi possível assinar stop-inference para {}: {error}",
                    target.node_id
                );
                continue;
            }
        };
        match request.send().await {
            Ok(r) if r.status().is_success() => {
                info!("peer {} parou inferencia", target.node_id);
            }
            Ok(r) => {
                warn!(
                    "peer {} recusou stop-inference: HTTP {}",
                    target.node_id,
                    r.status()
                );
            }
            Err(e) => {
                warn!(
                    "peer {} inacessivel para stop-inference: {e}",
                    target.node_id
                );
            }
        }
    }

    // Brief settle: let the OS release ports.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: dispatch apply with the new configuration.
    let mut results = Vec::new();

    for (index, target) in req.targets.iter().enumerate() {
        let params = params_for(&req.params, target);
        let apply_req = ApplyRequest {
            source_model_path: target.source_model_path.clone(),
            base_name: req.base_name.clone(),
            model_label: req.model_label.clone(),
            deployment_name: deployment.clone(),
            start_layer: target.start_layer,
            end_layer: target.end_layer,
            total_layers: req.total_layers,
            peer_count: target.peer_count.unwrap_or(peer_count),
            peer_index: target.peer_index.unwrap_or(index as u32),
            primary: req.primary_node_id.as_deref() == Some(target.node_id.as_str()),
            tp: target.tp.clone(),
            skip_split: target.skip_split,
            deploy_id: req.deploy_id.clone(),
            part_sha256: target.part_sha256.clone(),
            merge: target.merge.clone(),
            use_mmproj: req.use_mmproj && target.use_mmproj,
            mmproj_path: target.mmproj_path.clone(),
            params,
        };

        if target.node_id == self_id {
            let resp = apply(ConnectionAuth, State(state.clone()), Json(apply_req)).await;
            results.push(json!({
                "node_id": target.node_id,
                "dispatched": resp.status().is_success(),
                "local": true,
            }));
            continue;
        }

        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == target.node_id)
            .and_then(|p| p.gateway_url())
        else {
            results.push(json!({
                "node_id": target.node_id,
                "dispatched": false,
                "error": "peer não encontrado ou sem porta de gateway anunciada",
            }));
            continue;
        };

        // The peer's /api/ui/apply is behind ConnectionAuth. Every node in a
        // cluster shares the same connection key, so forward ours — without it
        // the remote leg of a launch always came back 401.
        let url = format!("{base}/api/ui/apply");
        let cliente = match state.peer_http.for_peer(&target.node_id) {
            Ok(c) => c,
            Err(e) => {
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": false,
                    "error": format!("sem cliente para o peer: {e}"),
                }));
                continue;
            }
        };
        let request = match state
            .control_json_request(cliente.post(&url), "/api/ui/apply", &apply_req)
            .await
        {
            Ok(request) => request,
            Err(error) => {
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": false,
                    "error": format!("não foi possível assinar o apply: {error}"),
                }));
                continue;
            }
        };
        match request.send().await {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or(json!({}));
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": true,
                    "job_id": body.get("job_id"),
                }));
            }
            Ok(r) => {
                let code = r.status();
                let body = r.text().await.unwrap_or_default();
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": false,
                    "error": format!("HTTP {code}: {body}"),
                }));
            }
            Err(e) => results.push(json!({
                "node_id": target.node_id,
                "dispatched": false,
                "error": e.to_string(),
            })),
        }
    }

    Json(json!({ "results": results })).into_response()
}
