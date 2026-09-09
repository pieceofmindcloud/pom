use super::*;
use crate::profile::{NodeProfile, ProfileAction};
// ── Somar ou tirar uma maquina de um modelo ja no ar ─────────────────────────
//
// Os dois caminhos tocam **uma** maquina. `deploy_endpoint` reimplanta o
// conjunto inteiro, e usar aquele caminho aqui pararia a inferencia de quem ja
// esta servindo - exatamente o que somar uma maquina existe para evitar.

#[derive(Debug, Deserialize)]
pub struct AddNodeOptionsRequest {
    pub group: String,
    #[serde(default)]
    pub kind: DeployKind,
    pub target_node_id: String,
}

#[derive(Debug, Serialize)]
pub struct AddNodeOption {
    pub kind: DeployKind,
    pub start_layer: u32,
    pub end_layer: u32,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct AddNodeOptions {
    pub node_id: String,
    pub options: Vec<AddNodeOption>,
    pub cache_types: serde_json::Value,
    pub defaults: AddNodeDefaults,
}

#[derive(Debug, Serialize)]
pub struct AddNodeDefaults {
    pub n_gpu_layers: u32,
    pub offload_kqv: bool,
}

/// The dialog may add either an actual slice or a whole model. Query both
/// catalog views so the options are always backed by the exact `HeldKind` the
/// add endpoint will validate. Tensor parallelism has its own catalog.
pub(crate) fn add_node_option_survey_kinds(kind: DeployKind) -> Vec<DeployKind> {
    match kind {
        DeployKind::TensorParallel => vec![DeployKind::TensorParallel],
        DeployKind::Shards | DeployKind::Whole => {
            vec![DeployKind::Shards, DeployKind::Whole]
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AddNodeRequest {
    pub group: String,
    /// A implantação que ganha a máquina. Vazio enquanto a tela não manda:
    /// aí o nó entra sem registro, servindo a faixa, e a implantação segue
    /// sendo identificada pelo rótulo como antes.
    #[serde(default)]
    pub deploy_id: String,
    /// Which catalog the range is looked up in. A whole model already in
    /// service takes a second machine the same way a slice does — the added
    /// node becomes a parallel route over the same layers.
    #[serde(default)]
    pub kind: DeployKind,
    pub target_node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub params: Option<RuntimeParams>,
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    #[serde(default)]
    pub offload_kqv: Option<bool>,
    /// Valores de instalação do destino, aplicados antes do novo processo
    /// subir. Eles são opcionais para clientes antigos, que mantêm a config
    /// já persistida no peer.
    #[serde(default)]
    pub cache_type_k: Option<crate::config::KvCacheType>,
    #[serde(default)]
    pub cache_type_v: Option<crate::config::KvCacheType>,
}
#[derive(Debug, Deserialize)]
pub struct RemoveNodeRequest {
    pub group: String,
    pub target_node_id: String,
    /// A implantação exata evita misturar duas implantações do mesmo modelo.
    #[serde(default)]
    pub deploy_id: String,
}

const ADD_NODE_ADMIN_ONLY: &str =
    "somente o administrador pode adicionar uma máquina à implantação";
const UNDEPLOY_ADMIN_ONLY: &str = "somente o administrador pode desimplantar a implantação";

pub(super) fn deployment_authority(
    profile: NodeProfile,
    refusal: &'static str,
) -> Result<(), String> {
    profile
        .allows(ProfileAction::ManageDeployment)
        .then_some(())
        .ok_or_else(|| refusal.to_string())
}

/// Encontra a implantação ativa que corresponde ao modelo, inclusive quando
/// o painel ainda não carregou a tela de histórico e não consegue mandar o id.
pub(super) async fn record_for_group(
    state: &GatewayState,
    group: &str,
) -> Option<crate::deployment::Deployment> {
    let ids = state
        .all_peers()
        .await
        .into_iter()
        .filter(|peer| peer.model == group && !peer.deploy_id.is_empty())
        .map(|peer| peer.deploy_id)
        .collect::<std::collections::BTreeSet<_>>();
    let records = futures::future::join_all(ids.iter().map(|id| find_record(state, id)))
        .await
        .into_iter()
        .flatten();
    newest_deployment(records)
}

async fn target_json(
    state: &GatewayState,
    target_node_id: &str,
    path: &str,
) -> Result<serde_json::Value, String> {
    if target_node_id == state.network.node_id {
        let runtime = state.runtime.config().await;
        return Ok(match path {
            "node" => json!({
                "config": runtime,
            }),
            "cache-types" => {
                let cfg = state.auth.read().await;
                json!({
                    "cache_type_k": cfg.cache_type_k,
                    "cache_type_v": cfg.cache_type_v,
                    "choices": crate::config::KvCacheType::ALL
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>(),
                })
            }
            _ => json!({}),
        });
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == target_node_id)
        .ok_or_else(|| "a máquina escolhida não está no cluster".to_string())?;
    let base = peer
        .gateway_url()
        .ok_or_else(|| "a máquina escolhida não anuncia porta de gateway".to_string())?;
    let client = state
        .peer_http
        .for_peer(target_node_id)
        .map_err(|e| format!("sem cliente para o peer: {e}"))?;
    let key = state.auth.read().await.connection_key.clone();
    client
        .get(format!("{base}/api/ui/{path}"))
        .bearer_auth(key)
        .timeout(PEER_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("peer inacessível: {e}"))?
        .json()
        .await
        .map_err(|e| format!("resposta inválida do peer: {e}"))
}

async fn apply_target_cache_types(
    state: &GatewayState,
    target_node_id: &str,
    requested_k: Option<crate::config::KvCacheType>,
    requested_v: Option<crate::config::KvCacheType>,
) -> Result<(), String> {
    if requested_k.is_none() && requested_v.is_none() {
        return Ok(());
    }
    let current = target_json(state, target_node_id, "cache-types").await?;
    let current_k = current
        .get("cache_type_k")
        .cloned()
        .ok_or_else(|| "a máquina não informou o tipo de cache K".to_string())
        .and_then(|value| {
            serde_json::from_value(value).map_err(|e| format!("cache K inválido: {e}"))
        })?;
    let current_v = current
        .get("cache_type_v")
        .cloned()
        .ok_or_else(|| "a máquina não informou o tipo de cache V".to_string())
        .and_then(|value| {
            serde_json::from_value(value).map_err(|e| format!("cache V inválido: {e}"))
        })?;
    let cache_type_k = requested_k.unwrap_or(current_k);
    let cache_type_v = requested_v.unwrap_or(current_v);

    if target_node_id == state.network.node_id {
        let mut cfg = state.auth.write().await;
        cfg.cache_type_k = cache_type_k;
        cfg.cache_type_v = cache_type_v;
        cfg.save()
            .map_err(|e| format!("não deu para gravar pom.json: {e}"))?;
        return Ok(());
    }

    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == target_node_id)
        .ok_or_else(|| "a máquina escolhida não está no cluster".to_string())?;
    let base = peer
        .gateway_url()
        .ok_or_else(|| "a máquina escolhida não anuncia porta de gateway".to_string())?;
    let client = state
        .peer_http
        .for_peer(target_node_id)
        .map_err(|e| format!("sem cliente para o peer: {e}"))?;
    let key = state.auth.read().await.connection_key.clone();
    let response = client
        .post(format!("{base}/api/ui/cache-types"))
        .bearer_auth(key)
        .timeout(PEER_TIMEOUT)
        .json(&json!({ "cache_type_k": cache_type_k, "cache_type_v": cache_type_v }))
        .send()
        .await
        .map_err(|e| format!("peer inacessível: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "a máquina recusou a configuração do cache: HTTP {}",
            response.status()
        ))
    }
}

pub(super) async fn add_node_options_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AddNodeOptionsRequest>,
) -> Response {
    let mut held = Vec::new();
    let mut target_seen = false;
    for survey_kind in add_node_option_survey_kinds(req.kind) {
        let survey = survey(&state, &req.group, survey_kind, None, None).await;
        if let Some(target) = survey
            .machines
            .iter()
            .find(|machine| machine.node_id == req.target_node_id)
        {
            target_seen = true;
            held.extend(target.held.iter().map(|held| AddNodeOption {
                kind: match held.kind {
                    HeldKind::Whole => DeployKind::Whole,
                    HeldKind::Shard => DeployKind::Shards,
                },
                start_layer: held.start_layer,
                end_layer: held.end_layer,
                label: held.name.clone(),
            }));
        }
    }
    if !target_seen {
        return bad(json!({ "error": "a máquina escolhida não está no cluster" }));
    }

    let mut options = Vec::new();
    for option in held {
        if !options.iter().any(|existing: &AddNodeOption| {
            existing.kind == option.kind
                && existing.start_layer == option.start_layer
                && existing.end_layer == option.end_layer
                && existing.label == option.label
        }) {
            options.push(option);
        }
    }
    options.sort_by_key(|option| {
        (
            option.start_layer,
            option.end_layer,
            matches!(option.kind, DeployKind::Shards),
        )
    });

    let node = target_json(&state, &req.target_node_id, "node").await;
    let defaults = node
        .as_ref()
        .ok()
        .and_then(|body| body.get("config"))
        .and_then(|config| config.as_object())
        .map(|config| AddNodeDefaults {
            n_gpu_layers: config
                .get("n_gpu_layers")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_else(|| RuntimeParams::default().n_gpu_layers),
            offload_kqv: config
                .get("offload_kqv")
                .and_then(|value| value.as_bool())
                .unwrap_or_else(|| RuntimeParams::default().offload_kqv),
        })
        .unwrap_or_else(|| AddNodeDefaults {
            n_gpu_layers: RuntimeParams::default().n_gpu_layers,
            offload_kqv: RuntimeParams::default().offload_kqv,
        });
    let cache_types = match target_json(&state, &req.target_node_id, "cache-types").await {
        Ok(value) => value,
        Err(error) => return bad(json!({ "error": error })),
    };

    Json(AddNodeOptions {
        node_id: req.target_node_id,
        options,
        cache_types,
        defaults,
    })
    .into_response()
}

/// Faixas que este grupo tem no ar agora, uma por peer que o esta servindo.
pub(super) async fn live_ranges(
    state: &GatewayState,
    group: &str,
    deploy_id: Option<&str>,
) -> Vec<(String, u32, u32)> {
    state
        .all_peers()
        .await
        .into_iter()
        .filter(|p| {
            p.model == group
                && p.total_layers > 0
                && deploy_id.is_none_or(|id| id.is_empty() || p.deploy_id == id)
        })
        .map(|p| (p.node_id, p.start_layer, p.end_layer))
        .collect()
}

/// Faixas que pertencem à cabeça fatiada de um deployment misto.
///
/// O registro preserva a diferença entre o GGUF inteiro e uma fatia mesmo
/// quando ambos cobrem todas as camadas. Só o primeiro sai desta lista: a
/// posição do novo shard não pode contar o modelo inteiro como um estágio do
/// pipeline fatiado.
fn sliced_route_ranges(
    record: Option<&crate::deployment::Deployment>,
    live: &[(String, u32, u32)],
    total_layers: u32,
) -> Vec<(u32, u32)> {
    live.iter()
        .filter(|(node_id, start, end)| {
            !record.is_some_and(|deployment| {
                deployment.node(node_id).is_some_and(|node| {
                    deployment.kind == crate::deployment::DeploymentKind::Whole
                        || (deployment.kind == crate::deployment::DeploymentKind::Mixed
                            && *start == 0
                            && end.saturating_add(1) == total_layers
                            && node.part_sha256.is_none()
                            && node.peer_count == 1)
                })
            })
        })
        .map(|(_, start, end)| (*start, *end))
        .collect()
}

pub(super) async fn add_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AddNodeRequest>,
) -> Response {
    let profile = state.auth.read().await.profile;
    if let Err(e) = deployment_authority(profile, ADD_NODE_ADMIN_ONLY) {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(json!({ "error": e })),
        )
            .into_response();
    }

    if req.start_layer > req.end_layer {
        return bad(json!({ "error": "faixa de camadas invertida" }));
    }

    if !req.deploy_id.is_empty() && !crate::deployment::is_deploy_id(&req.deploy_id) {
        return bad(json!({ "error": format!("deploy_id inválido: {}", req.deploy_id) }));
    }

    let existing_record = if req.deploy_id.is_empty() {
        record_for_group(&state, &req.group).await
    } else {
        find_record(&state, &req.deploy_id).await
    };
    let deployment_id = existing_record
        .as_ref()
        .map(|record| record.deploy_id.clone())
        .unwrap_or_else(|| req.deploy_id.clone());
    if !req.deploy_id.is_empty() && existing_record.is_none() {
        return bad(json!({
            "error": format!("implantação {} desconhecida", req.deploy_id)
        }));
    }
    let params = req
        .params
        .clone()
        .or_else(|| {
            existing_record
                .as_ref()
                .and_then(|record| record.active().map(|version| version.params.clone()))
        })
        .unwrap_or_default();

    // Re-varre em vez de confiar na tela: a previa pode ter um minuto, e uma
    // maquina que saiu nesse meio tempo receberia uma fatia.
    let machines = survey(&state, &req.group, req.kind, None, None)
        .await
        .machines;
    let Some(target) = machines.iter().find(|m| m.node_id == req.target_node_id) else {
        return bad(json!({ "error": "a maquina escolhida nao esta no cluster" }));
    };
    let requested_kind = match req.kind {
        DeployKind::Whole => Some(HeldKind::Whole),
        _ => Some(HeldKind::Shard),
    };
    let Some(shard) = target.holds_kind(req.start_layer, req.end_layer, requested_kind) else {
        return bad(json!({
            "error": format!(
                "{} nao tem {}; mande para la antes",
                target.label(),
                req.kind.unit(&req.group, req.start_layer, req.end_layer)
            )
        }));
    };
    let source_model_path = shard.path.clone();
    let shard_sha256 = shard.sha256.clone();

    let live = live_ranges(
        &state,
        &req.group,
        (!deployment_id.is_empty()).then_some(deployment_id.as_str()),
    )
    .await;
    let outras: Vec<(u32, u32)> = live
        .iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (*i, *f))
        .collect();
    let sobrepostas = overlapping_layers(&outras, (req.start_layer, req.end_layer));

    let candidatos = candidates(&machines);
    let Some(total_layers) = model_size(&candidatos).value() else {
        return bad(json!({
            "error": format!("as fatias de {} nao dizem o total de camadas", req.group)
        }));
    };
    if req.end_layer >= total_layers {
        return bad(json!({
            "error": format!(
                "{} passa do fim de um modelo de {total_layers} camadas",
                req.kind.unit(&req.group, req.start_layer, req.end_layer)
            )
        }));
    }

    if let Err(error) = apply_target_cache_types(
        &state,
        &req.target_node_id,
        req.cache_type_k,
        req.cache_type_v,
    )
    .await
    {
        return bad(json!({ "error": error }));
    }

    // `peer_index`/`peer_count` alimentam duas decisoes so: se este no exporta
    // o estado residual e se ele pula os embeddings. As duas perguntam "sou o
    // ultimo?", e a resposta vem da ordem das camadas no cluster - nao da
    // posicao numa lista de alvos que este caminho, por existir para tocar uma
    // maquina so, nunca vai montar.
    let pipeline_ranges = sliced_route_ranges(existing_record.as_ref(), &live, total_layers);
    let (peer_index, peer_count) = if req.kind == DeployKind::Whole {
        (0, 1)
    } else {
        split_position(&pipeline_ranges, (req.start_layer, req.end_layer))
    };
    let target_node_id = req.target_node_id.clone();
    let response = crate::webui::launch(
        ConnectionAuth,
        State(state.clone()),
        Json(LaunchRequest {
            deploy_id: deployment_id.clone(),
            base_name: req.group.clone(),
            model_label: Some(req.group.clone()),
            deployment_name: req.group.clone(),
            total_layers,
            use_mmproj: false,
            // Um alvo so: o `launch` para a inferencia dos alvos que recebe, e
            // esta lista tem exatamente a maquina que esta entrando.
            targets: vec![LaunchTarget {
                node_id: target_node_id.clone(),
                source_model_path,
                start_layer: req.start_layer,
                end_layer: req.end_layer,
                skip_split: true,
                part_sha256: shard_sha256.clone(),
                merge: None,
                n_gpu_layers: req.n_gpu_layers,
                offload_kqv: req.offload_kqv,
                no_embeddings: None,
                // Um alvo por requisição, mas a posição é a do pipeline: sem
                // isto o nó somado se diz o último da divisão.
                peer_index: Some(peer_index),
                peer_count: Some(peer_count),
                tp: None,
                mmproj_path: None,
                use_mmproj: false,
            }],
            // Quem e a entrada do pipeline nao muda por isto: so quem fica com
            // a camada 0 recebe o prompt, e o `launch` so marca `primary` no
            // alvo que ele recebe.
            primary_node_id: if req.start_layer == 0 {
                Some(target_node_id.clone())
            } else {
                None
            },
            params: params.clone(),
        }),
    )
    .await;

    if !response.status().is_success() {
        let reason = refusal(response).await;
        return bad(json!({ "error": reason }));
    }

    let (version, kind) = if let Some(mut record) = existing_record {
        let incoming_kind = record_kind(req.kind);
        if record.kind != incoming_kind && record.kind != crate::deployment::DeploymentKind::Mixed {
            record.kind = crate::deployment::DeploymentKind::Mixed;
        }
        let mut nodes = record.nodes.clone();
        nodes.retain(|node| node.node_id != target_node_id);
        let target = LaunchTarget {
            node_id: target_node_id.clone(),
            source_model_path: shard.path.clone(),
            start_layer: req.start_layer,
            end_layer: req.end_layer,
            skip_split: true,
            part_sha256: shard_sha256.clone(),
            merge: None,
            n_gpu_layers: req.n_gpu_layers,
            offload_kqv: req.offload_kqv,
            no_embeddings: None,
            peer_index: Some(peer_index),
            peer_count: Some(peer_count),
            tp: None,
            mmproj_path: None,
            use_mmproj: false,
        };
        let machines = survey(&state, &req.group, req.kind, None, None)
            .await
            .machines;
        let hostname = machines
            .iter()
            .find(|machine| machine.node_id == target_node_id)
            .map(Machine::label)
            .unwrap_or_default();
        nodes.push(crate::deployment::DeploymentNode {
            node_id: target_node_id.clone(),
            hostname,
            start_layer: req.start_layer,
            end_layer: req.end_layer,
            peer_index,
            peer_count,
            primary: req.start_layer == 0,
            part_sha256: target.part_sha256.clone(),
            source_model_path: (!target.source_model_path.is_empty())
                .then(|| target.source_model_path.clone()),
            tp_rank: None,
            config: crate::webui::params_for(&params, &target),
        });
        let version = record.append_version(params, nodes, state.network.node_id.as_str());
        publish_record(&state, &record).await;
        (Some(version), record.kind)
    } else {
        (None, record_kind(req.kind))
    };

    Json(json!({
        "applied": true,
        "overlapping_layers": sobrepostas,
        "peer_index": peer_index,
        "peer_count": peer_count,
        "deploy_id": (!deployment_id.is_empty()).then_some(deployment_id),
        "version": version,
        "kind": kind,
    }))
    .into_response()
}

pub(super) async fn remove_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<RemoveNodeRequest>,
) -> Response {
    if !req.deploy_id.is_empty() && !crate::deployment::is_deploy_id(&req.deploy_id) {
        return bad(json!({ "error": format!("deploy_id inválido: {}", req.deploy_id) }));
    }
    let record = if req.deploy_id.is_empty() {
        record_for_group(&state, &req.group).await
    } else {
        find_record(&state, &req.deploy_id).await
    };
    let deployment_id = record
        .as_ref()
        .map(|deployment| deployment.deploy_id.as_str())
        .or_else(|| (!req.deploy_id.is_empty()).then_some(req.deploy_id.as_str()));
    let vivas = live_ranges(&state, &req.group, deployment_id).await;
    let Some((_, inicio, fim)) = vivas
        .iter()
        .find(|(id, _, _)| id == &req.target_node_id)
        .cloned()
    else {
        return bad(json!({ "error": "essa maquina nao esta servindo este modelo" }));
    };
    let restantes: Vec<(u32, u32)> = vivas
        .iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (*i, *f))
        .collect();
    let descobertas = uncovered_after_removal(&restantes, (inicio, fim));

    // Quem libera a memoria e o dono dela, como no botao "Liberar fatia": este
    // no so encaminha o pedido.
    if req.target_node_id == state.network.node_id {
        state.runtime.stop().await;
    } else {
        let Some(base) = gateway_of(&state, &req.target_node_id).await else {
            return bad(json!({ "error": "peer sem porta de gateway anunciada" }));
        };
        let cliente = match state.peer_http.for_peer(&req.target_node_id) {
            Ok(c) => c,
            Err(e) => return bad(json!({ "error": format!("sem cliente para o peer: {e}") })),
        };
        let request = match state
            .authorize_control_request(
                cliente
                    .post(format!("{base}/api/ui/unload"))
                    .timeout(PEER_TIMEOUT),
                "POST",
                "/api/ui/unload",
                &[],
            )
            .await
        {
            Ok(request) => request,
            Err(error) => {
                return bad(json!({ "error": format!("não foi possível assinar: {error}") }))
            }
        };
        match request.send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                return bad(json!({ "error": format!("o peer recusou: HTTP {}", r.status()) }))
            }
            Err(e) => return bad(json!({ "error": format!("peer inacessivel: {e}") })),
        }
    }

    let (deploy_id, version, serving) = if let Some(mut record) = record {
        if record.node(&req.target_node_id).is_some() {
            let params = record
                .active()
                .map(|active| active.params.clone())
                .unwrap_or_default();
            let nodes = record
                .nodes
                .iter()
                .filter(|node| node.node_id != req.target_node_id)
                .cloned()
                .collect();
            let version = record.append_version(params, nodes, state.network.node_id.as_str());
            let serving = !record.nodes.is_empty();
            publish_record(&state, &record).await;
            (Some(record.deploy_id), Some(version), serving)
        } else {
            (None, None, !vivas.is_empty())
        }
    } else {
        (None, None, !vivas.is_empty())
    };

    Json(json!({
        "removed": true,
        "uncovered_layers": descobertas,
        "deploy_id": deploy_id,
        "version": version,
        "serving": serving,
    }))
    .into_response()
}

// ── Tirando a pipeline inteira do ar ─────────────────────────────────────────
//
// O contrário de "Implantar na rede". Remover uma máquina do modelo já existia
// (`/api/ui/deploy/node/remove`), mas desfazer a implantação inteira era
// clicar máquina por máquina, na ordem certa, torcendo para não esquecer
// nenhuma — e uma esquecida é RAM e VRAM presas numa fatia que não serve mais
// para nada, porque o pipeline só planeja com a cobertura fechada.
//
// **O que isto NÃO faz: mexer no disco.** Nenhuma fatia é apagada. Desimplantar
// é devolver a memória; os arquivos continuam onde estão e a mesma implantação
// volta com um clique em vez de com 17 minutos de transferência por máquina.
// Apagar fatias é o que a tela de Modelos faz, com o seu próprio pedido de
// confirmação, e é uma decisão diferente desta.

/// O perfil do nó que recebeu o pedido decide se a pipeline inteira pode sair
/// do ar. O descarregamento encaminhado aos peers continua sendo a operação
/// local de cada máquina e não passa por este portão.

#[derive(Debug, Deserialize)]
pub struct UndeployRequest {
    /// Modelo cuja pipeline sai do ar. Vazio significa "a que estiver no ar",
    /// que é o caso do painel: ele mostra uma pipeline de cada vez.
    #[serde(default)]
    pub group: String,
    /// A implantação exata que sai do ar.
    ///
    /// É a saída para o beco em que `resolve_group` batia: com duas
    /// implantações no ar - inclusive **do mesmo modelo**, que agora é
    /// possível - o rótulo não distingue, e o id sim.
    #[serde(default)]
    pub deploy_id: String,
}

/// O que aconteceu com **uma** máquina. Uma linha por máquina, sempre — inclusive
/// para a que não respondeu.
#[derive(Debug, Serialize)]
pub struct UndeployOutcome {
    pub node_id: String,
    pub hostname: String,
    pub start_layer: u32,
    pub end_layer: u32,
    /// `true` só quando a máquina confirmou. Um peer calado é `false` com o
    /// motivo em `error`; contá-lo como sucesso seria dizer que a memória foi
    /// liberada sem ninguém ter dito isso.
    pub released: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Descarrega o modelo de todas as máquinas que servem `group`.
///
/// Responde 200 com o relatório mesmo quando alguma máquina falha: metade
/// desimplantada é o estado real do cluster, e escondê-lo atrás de um erro
/// deixaria o operador sem saber em quais máquinas ainda há memória presa.
/// Só pedido inválido (nenhuma pipeline no ar, ou ambiguidade sobre qual) é 400.
pub(super) async fn undeploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<UndeployRequest>,
) -> Response {
    {
        let profile = state.auth.read().await.profile;
        if let Err(e) = deployment_authority(profile, UNDEPLOY_ADMIN_ONLY) {
            return (
                axum::http::StatusCode::FORBIDDEN,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    }

    let peers = state.all_peers().await;
    let alvo = match resolve_target(&req.deploy_id, &req.group, &peers) {
        Ok(a) => a,
        Err(e) => return bad(json!({ "error": e })),
    };

    // Re-varre em vez de confiar na tela, como o `deploy`: uma máquina que saiu
    // no meio tempo não pode virar uma linha de falha inventada.
    let serving: Vec<PeerInfo> = peers
        .into_iter()
        .filter(|p| p.total_layers > 0 && alvo.matches(p))
        .collect();
    if serving.is_empty() {
        return bad(json!({
            "error": format!("nenhuma máquina está servindo {}", alvo.describe())
        }));
    }

    let group = alvo.label();
    info!(
        "desimplantando {} de {} máquina(s)",
        alvo.describe(),
        serving.len()
    );

    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();

    // Em paralelo: com onze peers e o prazo de um controle por peer, em série a
    // tela ficaria quase um minuto sem resposta.
    let mut results: Vec<UndeployOutcome> =
        futures::future::join_all(serving.into_iter().map(|peer| {
            let state = state.clone();
            let key = key.clone();
            let self_id = self_id.clone();
            async move {
                let error = release_one(&state, &peer, &key, &self_id).await.err();
                UndeployOutcome {
                    node_id: peer.node_id.clone(),
                    hostname: if peer.hostname.is_empty() {
                        peer.node_id[..12.min(peer.node_id.len())].to_string()
                    } else {
                        peer.hostname.clone()
                    },
                    start_layer: peer.start_layer,
                    end_layer: peer.end_layer,
                    released: error.is_none(),
                    error,
                }
            }
        }))
        .await;
    results.sort_by_key(|r| r.start_layer);

    let released = results.iter().filter(|r| r.released).count();
    let failed = results.len() - released;
    if failed > 0 {
        warn!("{failed} máquina(s) não confirmaram a desimplantação de {group}");
    }

    // O registro só é esquecido depois: um registro apagado com máquinas ainda
    // servindo deixaria memória presa sem nada que a nomeasse. Neste nó basta
    // apagar o arquivo; os outros participantes apagam o seu ao serem
    // descarregados, e um que não respondeu guarda o dele - que é o certo, já
    // que ele ainda está no ar.
    if let UndeployTarget::Deployment { deploy_id, .. } = &alvo {
        if let Err(e) = state.database.remove_deployment(deploy_id) {
            warn!("não deu para esquecer a implantação {deploy_id}: {e:#}");
        }
    }

    Json(json!({
        "group": group,
        "deploy_id": match &alvo {
            UndeployTarget::Deployment { deploy_id, .. } => Some(deploy_id.clone()),
            UndeployTarget::Label(_) => None,
        },
        "released": released,
        "failed": failed,
        "results": results,
    }))
    .into_response()
}

/// Qual pipeline o pedido quer derrubar.
///
/// Vazio e uma só no ar é a intenção óbvia. Vazio com duas é ambiguidade que
/// esta função se recusa a resolver por conta própria: derrubar a errada custa
/// uma reimplantação inteira.
pub(super) fn resolve_group(requested: &str, peers: &[PeerInfo]) -> Result<String, String> {
    if !requested.is_empty() {
        return Ok(requested.to_string());
    }
    let mut models: Vec<String> = peers
        .iter()
        .filter(|p| p.total_layers > 0 && !p.model.is_empty())
        .map(|p| p.model.clone())
        .collect();
    models.sort();
    models.dedup();
    match models.len() {
        0 => Err("nenhuma pipeline no ar para desimplantar".to_string()),
        1 => Ok(models.remove(0)),
        _ => Err(format!(
            "há mais de uma pipeline no ar ({}); diga qual desimplantar",
            models.join(", ")
        )),
    }
}

/// O que uma desimplantação vai derrubar: uma implantação, ou tudo que serve um
/// rótulo.
pub(super) enum UndeployTarget {
    /// Escolha exata. É o que distingue duas implantações do mesmo modelo.
    Deployment { deploy_id: String, label: String },
    /// Todo mundo que serve este rótulo, que é o comportamento de sempre e o
    /// único possível com peers que não anunciam implantação.
    Label(String),
}

impl UndeployTarget {
    pub(super) fn matches(&self, p: &PeerInfo) -> bool {
        match self {
            UndeployTarget::Deployment { deploy_id, .. } => &p.deploy_id == deploy_id,
            UndeployTarget::Label(l) => &p.model == l,
        }
    }
    pub(super) fn label(&self) -> String {
        match self {
            UndeployTarget::Deployment { label, .. } => label.clone(),
            UndeployTarget::Label(l) => l.clone(),
        }
    }
    pub(super) fn describe(&self) -> String {
        match self {
            UndeployTarget::Deployment { deploy_id, label } if label.is_empty() => {
                format!("a implantação {deploy_id}")
            }
            UndeployTarget::Deployment { deploy_id, label } => {
                format!("{label} (implantação {deploy_id})")
            }
            UndeployTarget::Label(l) => l.clone(),
        }
    }
}

/// Qual implantação o pedido quer derrubar.
///
/// Com `deploy_id`, não há o que deduzir - é aquela, e o rótulo serve só para a
/// mensagem. Sem ele, cai na regra antiga sobre rótulos, que continua sendo a
/// certa num cluster onde cada modelo está no ar uma vez só.
pub(super) fn resolve_target(
    deploy_id: &str,
    group: &str,
    peers: &[PeerInfo],
) -> Result<UndeployTarget, String> {
    if !deploy_id.is_empty() {
        if !crate::deployment::is_deploy_id(deploy_id) {
            return Err(format!("deploy_id inválido: {deploy_id}"));
        }
        let label = peers
            .iter()
            .find(|p| p.deploy_id == deploy_id)
            .map(|p| p.model.clone())
            .unwrap_or_else(|| group.to_string());
        return Ok(UndeployTarget::Deployment {
            deploy_id: deploy_id.to_string(),
            label,
        });
    }
    resolve_group(group, peers).map(UndeployTarget::Label)
}

/// Libera a memória de **uma** máquina, pela mesma porta do botão que já existe.
///
/// Quem mata o processo de inferência é sempre o dono da memória: este nó só
/// encaminha, exatamente como `remove_node_endpoint`.
pub(super) async fn release_one(
    state: &GatewayState,
    peer: &PeerInfo,
    _key: &str,
    self_id: &str,
) -> Result<(), String> {
    if peer.node_id == self_id {
        state.runtime.stop().await;
        return Ok(());
    }
    let base = peer
        .gateway_url()
        .ok_or_else(|| "peer sem porta de gateway anunciada".to_string())?;
    let cliente = state
        .peer_http
        .for_peer(&peer.node_id)
        .map_err(|e| format!("sem cliente para a máquina: {e}"))?;
    let request = state
        .authorize_control_request(
            cliente
                .post(format!("{base}/api/ui/unload"))
                .timeout(PEER_TIMEOUT),
            "POST",
            "/api/ui/unload",
            &[],
        )
        .await
        .map_err(|error| format!("não foi possível assinar a liberação: {error}"))?;
    match request.send().await {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("a máquina recusou: HTTP {}", r.status())),
        Err(e) => Err(format!("máquina inacessível: {e}")),
    }
}

pub(super) fn bad(body: serde_json::Value) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// The reason a refused launch gives, not just its status code.
///
/// `launch` answers `{"error": "..."}` and the operator only ever saw
/// "HTTP 400", which is the same dead end this whole screen exists to avoid.
pub(super) async fn refusal(response: Response) -> String {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        });
    match body {
        Some(reason) => format!("o launch recusou: {reason}"),
        None => format!("o launch recusou: HTTP {status}"),
    }
}

pub(super) async fn gateway_of(state: &GatewayState, node_id: &str) -> Option<String> {
    state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.gateway_url())
}

/// Poll the machines doing the copying until they are all finished.
///
/// The bar tracks bytes rather than file count, because the slices are wildly
/// unequal — a set of 2 GB and 9,6 GB would otherwise sit at 50% for most of the
/// wait and then finish in a blink.
pub(super) async fn wait_for_transfers(
    state: &GatewayState,
    _key: &str,
    watching: &[(String, String, String)],
    job_id: &str,
) -> Result<(), String> {
    loop {
        let mut done = 0usize;
        let mut fraction_sum = 0.0;

        for (node_id, gateway, id) in watching {
            let cliente = state
                .peer_http
                .for_peer(node_id)
                .map_err(|e| format!("sem cliente para {gateway}: {e}"))?;
            let request = state
                .control_get_request(
                    cliente
                        .get(format!("{gateway}/api/transfer/jobs"))
                        .timeout(PEER_TIMEOUT),
                    "/api/transfer/jobs",
                )
                .await
                .map_err(|e| format!("não foi possível assinar a consulta de {gateway}: {e}"))?;
            let body: serde_json::Value = request
                .send()
                .await
                .map_err(|e| format!("perdi contato com {gateway}: {e}"))?
                .json()
                .await
                .unwrap_or(json!({}));

            let job = body
                .get("jobs")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .find(|j| j.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));

            let Some(job) = job else {
                // A job that vanished from the list finished and was pruned.
                done += 1;
                fraction_sum += 1.0;
                continue;
            };

            match job.get("state").and_then(|v| v.as_str()) {
                Some("done") => {
                    done += 1;
                    fraction_sum += 1.0;
                }
                Some("failed") => {
                    return Err(format!(
                        "a cópia falhou: {}",
                        job.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("sem detalhe")
                    ))
                }
                _ => {
                    let total = job
                        .get("total_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let got = job
                        .get("received_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    if total > 0.0 {
                        fraction_sum += (got / total).clamp(0.0, 1.0);
                    }
                }
            }
        }

        // Phase 1 owns the first 90% of the bar; the launch is the rest.
        let pct = fraction_sum / watching.len().max(1) as f64 * 90.0;
        set_job_progress(job_id, pct).await;

        if done == watching.len() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
