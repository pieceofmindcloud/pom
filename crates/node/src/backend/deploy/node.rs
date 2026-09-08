use super::*;
use crate::profile::{NodeProfile, ProfileAction};
// ── Somar ou tirar uma maquina de um modelo ja no ar ─────────────────────────
//
// Os dois caminhos tocam **uma** maquina. `deploy_endpoint` reimplanta o
// conjunto inteiro, e usar aquele caminho aqui pararia a inferencia de quem ja
// esta servindo - exatamente o que somar uma maquina existe para evitar.

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
    pub params: RuntimeParams,
}
#[derive(Debug, Deserialize)]
pub struct RemoveNodeRequest {
    pub group: String,
    pub target_node_id: String,
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

/// Faixas que este grupo tem no ar agora, uma por peer que o esta servindo.
pub(super) async fn live_ranges(state: &GatewayState, group: &str) -> Vec<(String, u32, u32)> {
    state
        .all_peers()
        .await
        .into_iter()
        .filter(|p| p.model == group && p.total_layers > 0)
        .map(|p| (p.node_id, p.start_layer, p.end_layer))
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

    // Re-varre em vez de confiar na tela: a previa pode ter um minuto, e uma
    // maquina que saiu nesse meio tempo receberia uma fatia.
    let machines = survey(&state, &req.group, req.kind, None, None)
        .await
        .machines;
    let Some(target) = machines.iter().find(|m| m.node_id == req.target_node_id) else {
        return bad(json!({ "error": "a maquina escolhida nao esta no cluster" }));
    };
    let Some(shard) = target.holds(req.start_layer, req.end_layer) else {
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

    let outras: Vec<(u32, u32)> = live_ranges(&state, &req.group)
        .await
        .into_iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (i, f))
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

    // `peer_index`/`peer_count` alimentam duas decisoes so: se este no exporta
    // o estado residual e se ele pula os embeddings. As duas perguntam "sou o
    // ultimo?", e a resposta vem da ordem das camadas no cluster - nao da
    // posicao numa lista de alvos que este caminho, por existir para tocar uma
    // maquina so, nunca vai montar.
    let (peer_index, peer_count) = split_position(&outras, (req.start_layer, req.end_layer));
    let target_node_id = req.target_node_id.clone();
    let response = crate::webui::launch(
        ConnectionAuth,
        State(state),
        Json(LaunchRequest {
            deploy_id: req.deploy_id.clone(),
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
                part_sha256: shard_sha256,
                merge: None,
                n_gpu_layers: None,
                offload_kqv: None,
                no_embeddings: None,
                // Um alvo por requisição, mas a posição é a do pipeline: sem
                // isto o nó somado se diz o último da divisão.
                peer_index: Some(peer_index),
                peer_count: Some(peer_count),
                tp: None,
                use_mmproj: false,
            }],
            // Quem e a entrada do pipeline nao muda por isto: so quem fica com
            // a camada 0 recebe o prompt, e o `launch` so marca `primary` no
            // alvo que ele recebe.
            primary_node_id: if req.start_layer == 0 {
                Some(target_node_id)
            } else {
                None
            },
            params: req.params,
        }),
    )
    .await;

    if !response.status().is_success() {
        let reason = refusal(response).await;
        return bad(json!({ "error": reason }));
    }

    Json(json!({
        "applied": true,
        "overlapping_layers": sobrepostas,
        "peer_index": peer_index,
        "peer_count": peer_count,
    }))
    .into_response()
}

pub(super) async fn remove_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<RemoveNodeRequest>,
) -> Response {
    let vivas = live_ranges(&state, &req.group).await;
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
    let key = state.auth.read().await.connection_key.clone();
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
        match cliente
            .post(format!("{base}/api/ui/unload"))
            .bearer_auth(&key)
            .timeout(PEER_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                return bad(json!({ "error": format!("o peer recusou: HTTP {}", r.status()) }))
            }
            Err(e) => return bad(json!({ "error": format!("peer inacessivel: {e}") })),
        }
    }

    Json(json!({
        "removed": true,
        "uncovered_layers": descobertas,
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
    key: &str,
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
    match cliente
        .post(format!("{base}/api/ui/unload"))
        .bearer_auth(key)
        .timeout(PEER_TIMEOUT)
        .send()
        .await
    {
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
    key: &str,
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
            let body: serde_json::Value = cliente
                .get(format!("{gateway}/api/transfer/jobs"))
                .bearer_auth(key)
                .timeout(PEER_TIMEOUT)
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
