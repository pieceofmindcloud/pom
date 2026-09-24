use super::*;
impl RecordPlan {
    pub(super) fn deploy_id(&self) -> &str {
        match self {
            RecordPlan::New { deploy_id } => deploy_id,
            RecordPlan::ReuseVersion(d) => &d.deploy_id,
        }
    }
}

/// Copy what has to be copied, then bind every peer to its range.
pub(super) async fn run(
    state: GatewayState,
    plan: Plan,
    params: RuntimeParams,
    identity: RequestedModelIdentity,
    record_plan: RecordPlan,
    use_mmproj: bool,
    job_id: &str,
) -> Result<(), String> {
    let deploy_id = record_plan.deploy_id().to_string();
    let deploy_id = deploy_id.as_str();
    prepare_existing_model(&state, &plan.group, &identity, job_id).await?;
    let key = state.auth.read().await.connection_key.clone();

    // One unit of movement: a slice that has to reach a target. A merged step
    // moves one file per part; a plain step moves its one slice.
    struct Pull {
        target_node_id: String,
        target_host: String,
        source_node_id: String,
        source_path: String,
        /// Hash da parte, quando a origem a tem endereçada por conteúdo.
        source_sha256: Option<String>,
        source_needs_split: bool,
        name: String,
        start_layer: u32,
        end_layer: u32,
    }
    let mut pulls: Vec<Pull> = Vec::new();
    if !plan.tp_targets.is_empty() {
        for target in &plan.tp_targets {
            if target.ready {
                continue;
            }
            let (Some(source_node), Some(source_path)) =
                (&target.source_node_id, &target.source_path)
            else {
                return Err(format!(
                    "o rank {}of{} não tem de onde vir",
                    target.rank + 1,
                    target.size
                ));
            };
            pulls.push(Pull {
                target_node_id: target.target_node_id.clone(),
                target_host: target.target_host.clone(),
                source_node_id: source_node.clone(),
                source_path: source_path.clone(),
                source_sha256: target.source_sha256.clone(),
                source_needs_split: false,
                name: target.name.clone(),
                start_layer: target.start_layer,
                end_layer: target.end_layer,
            });
        }
    } else {
        for step in &plan.steps {
            if step.merged {
                for part in &step.parts {
                    if part.ready {
                        continue;
                    }
                    let (Some(source_node), Some(source_path)) =
                        (&part.source_node_id, &part.source_path)
                    else {
                        return Err(format!(
                            "a fatia L{}-{} não tem de onde vir",
                            part.start_layer, part.end_layer
                        ));
                    };
                    pulls.push(Pull {
                        target_node_id: step.target_node_id.clone(),
                        target_host: step.target_host.clone(),
                        source_node_id: source_node.clone(),
                        source_path: source_path.clone(),
                        source_sha256: part.source_sha256.clone(),
                        source_needs_split: part.source_needs_split,
                        name: part.name.clone(),
                        start_layer: part.start_layer,
                        end_layer: part.end_layer,
                    });
                }
            } else if !step.ready {
                let (Some(source_node), Some(source_path)) =
                    (&step.source_node_id, &step.source_path)
                else {
                    return Err(format!(
                        "a fatia L{}-{} não tem de onde vir",
                        step.start_layer, step.end_layer
                    ));
                };
                pulls.push(Pull {
                    target_node_id: step.target_node_id.clone(),
                    target_host: step.target_host.clone(),
                    source_node_id: source_node.clone(),
                    source_path: source_path.clone(),
                    source_sha256: step.source_sha256.clone(),
                    source_needs_split: step.source_needs_split,
                    name: step.name.clone(),
                    start_layer: step.start_layer,
                    end_layer: step.end_layer,
                });
            }
        }
    }

    // Virtual ranges are backed by the proposer's whole GGUF. Materialize all
    // of them on that peer before any destination asks the transfer service to
    // pull a path. Grouping keeps one source read/manifest operation per
    // source model even when the selected pipeline has several ranges.
    let mut materializations: HashMap<(String, String, u32, String), Vec<SliceRange>> =
        HashMap::new();
    for pull in &pulls {
        if pull.source_needs_split {
            let ranges = materializations
                .entry((
                    pull.source_node_id.clone(),
                    pull.source_path.clone(),
                    plan.total_layers.ok_or_else(|| {
                        format!("as fatias de {} não dizem o total de camadas", plan.group)
                    })?,
                    plan.group.clone(),
                ))
                .or_default();
            if !ranges.iter().any(|range| {
                range.start_layer == pull.start_layer && range.end_layer == pull.end_layer
            }) {
                ranges.push(SliceRange {
                    start_layer: pull.start_layer,
                    end_layer: pull.end_layer,
                });
            }
        }
    }
    for ((source_node_id, source_path, total_layers, name), ranges) in materializations {
        set_job(
            job_id,
            "running",
            "fatiando modelo inteiro",
            &format!(
                "materializando {} faixa(s) na máquina que propôs o modelo",
                ranges.len()
            ),
        )
        .await;
        let response = materialize_slices_on_peer(
            &state,
            &source_node_id,
            &SliceRangesRequest {
                model_path: source_path.clone(),
                name: name.clone(),
                total_layers,
                ranges,
            },
        )
        .await?;
        for materialized in response.slices {
            let Some(pull) = pulls.iter_mut().find(|pull| {
                pull.source_needs_split
                    && pull.source_node_id == source_node_id
                    && pull.source_path == source_path
                    && pull.start_layer == materialized.start_layer
                    && pull.end_layer == materialized.end_layer
            }) else {
                return Err(format!(
                    "o fatiador devolveu uma faixa L{}-{} que não estava no plano",
                    materialized.start_layer, materialized.end_layer
                ));
            };
            pull.source_path = materialized.path;
            pull.source_sha256 = Some(materialized.sha256);
            pull.source_needs_split = false;
        }
    }
    if pulls.iter().any(|pull| pull.source_needs_split) {
        return Err("o fatiamento local não devolveu todas as faixas pedidas".to_string());
    }

    // ── Phase 1: transfers ───────────────────────────────────────────────────
    //
    // Each target pulls for itself, through the transfer path that already has
    // resume, MD5 checking and a disk-space guard. Nothing new moves bytes here.
    if !pulls.is_empty() {
        set_job(
            job_id,
            "running",
            "copiando fatias",
            &format!(
                "{} fatia(s), {:.1} GB no total",
                pulls.len(),
                plan.transfer_bytes as f64 / 1_073_741_824.0
            ),
        )
        .await;

        // (node_id, gateway, job id). O node_id entrou porque a sondagem do job
        // fala com o peer, e com o cluster em TLS ela precisa fixar o
        // certificado dele - uma URL sozinha não tem identidade a verificar.
        let mut watching: Vec<(String, String, String)> = Vec::new();
        for pull in &pulls {
            if pull.target_node_id == pull.source_node_id {
                continue;
            }
            let gateway = gateway_of(&state, &pull.target_node_id)
                .await
                .ok_or_else(|| format!("{} não anuncia porta de gateway", pull.target_host))?;

            let cliente = state
                .peer_http
                .for_peer(&pull.target_node_id)
                .map_err(|e| format!("{}: sem cliente para o peer: {e}", pull.target_host))?;
            let pull_body = json!({
                "node_id": pull.source_node_id,
                "sha256": pull.source_sha256,
                "model": plan.group,
                "remote_path": pull.source_path,
                "name": pull.name,
            });
            let request = state
                .control_json_request(
                    cliente.post(format!("{gateway}/api/transfer/pull")),
                    "/api/transfer/pull",
                    &pull_body,
                )
                .await
                .map_err(|e| format!("{} não aceitou puxar a fatia: {e}", pull.target_host))?;
            let body: serde_json::Value = request
                // Por hash sempre que a origem souber o dela. É o que faz a
                // fatia cair em `<shard_dir>/<modelo>/<sha>.gguf` com o `.pom`
                // ao lado, em vez de solta na raiz parecendo um modelo
                // inteiro. O pedido por caminho fica para a fatia sem hash -
                // um arquivo que ninguém endereçou por conteúdo ainda.
                .send()
                .await
                .map_err(|e| format!("{} não aceitou puxar a fatia: {e}", pull.target_host))?
                .json()
                .await
                .unwrap_or(json!({}));

            match body.get("job_id").and_then(|v| v.as_str()) {
                Some(tid) => {
                    info!(
                        "{} puxando L{}-{} de {}",
                        pull.target_host, pull.start_layer, pull.end_layer, pull.source_node_id
                    );
                    watching.push((pull.target_node_id.clone(), gateway, tid.to_string()));
                }
                None => {
                    return Err(format!(
                        "{} recusou a cópia: {}",
                        pull.target_host,
                        body.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("motivo não informado")
                    ));
                }
            }
        }

        wait_for_transfers(&state, &key, &watching, job_id).await?;
    }

    // ── Phase 2: bind every peer to its range ────────────────────────────────
    set_job(job_id, "running", "subindo os nós", "atribuindo as faixas").await;
    set_job_progress(job_id, 92.0).await;

    // Where each file ended up is the target's own business, so ask it rather
    // than assume a path on someone else's filesystem.
    let launch_selection = (plan.kind == DeployKind::Whole).then(|| {
        plan.steps
            .iter()
            .map(|step| SelectedSlice {
                start_layer: step.start_layer,
                end_layer: step.end_layer,
                target_node_id: step.target_node_id.clone(),
                n_gpu_layers: None,
                offload_kqv: None,
            })
            .collect::<Vec<_>>()
    });
    let machines = survey(
        &state,
        &plan.group,
        plan.kind,
        launch_selection.as_deref(),
        plan.tp.as_ref(),
    )
    .await
    .machines;
    let mut targets = launch_targets(
        &plan.steps,
        &machines,
        plan.kind,
        plan.tp.as_ref(),
        &plan.tp_targets,
    )?;
    if use_mmproj {
        let head = targets
            .iter_mut()
            .find(|target| target.start_layer == 0)
            .ok_or_else(|| {
                "a implantação com visão precisa de um alvo que comece na camada 0".to_string()
            })?;
        head.use_mmproj = true;
        head.mmproj_path = plan.mmproj_path.clone();
    }
    if params.no_embeddings {
        // O planner já validou atomicamente a topologia. Marcar cada alvo
        // explicitamente evita que um interruptor global seja reinterpretado
        // de forma diferente por um peer.
        for target in &mut targets {
            target.no_embeddings = Some(true);
        }
    }

    // The pipeline is the layer order, so the entry point is whoever holds
    // layer 0 — the same machine the screen marks "processa o prompt".
    let primary = plan
        .steps
        .iter()
        .find(|s| s.primary)
        .map(|s| s.target_node_id.clone());

    // `plan` refuses an unknown size before anything is copied, so this is a
    // fact by the time we get here rather than a default worth inventing.
    let total_layers = plan
        .total_layers
        .ok_or_else(|| format!("as fatias de {} não dizem o total de camadas", plan.group))?;

    // O registro da implantação nasce aqui, com os alvos já resolvidos: é o
    // último ponto em que se sabe, de uma vez só, quem recebeu qual faixa e com
    // que configuração. Depois do `launch` essa intenção se dissolvia em N
    // `NodeConfig` espalhados, e ninguém no cluster sabia mais dizer que
    // aqueles nós eram *a mesma* implantação.
    //
    // `New` monta a v1; `ReuseVersion` anexa (ou reativa) uma versão sob o
    // `deploy_id` que já existe, para reimplantar a mesma forma não cunhar um id
    // novo a cada vez.
    let registro = match record_plan {
        RecordPlan::New { .. } => build_record(
            deploy_id,
            &plan,
            &targets,
            &machines,
            total_layers,
            &params,
            &identity,
            &state.network.node_id,
        ),
        RecordPlan::ReuseVersion(mut existente) => {
            existente.ensure_versions();
            let nodes = build_version_nodes(&plan, &targets, &machines, &params);
            existente.upsert_version(params.clone(), nodes, state.network.node_id.as_str());
            *existente
        }
    };

    let response = launch(
        ConnectionAuth,
        State(state.clone()),
        Json(LaunchRequest {
            deploy_id: deploy_id.to_string(),
            base_name: plan.group.clone(),
            // `group` is the selected value from the deployment model picker.
            // Its name is the cluster identity, not a heuristic profile name.
            model_label: Some(plan.group.clone()),
            deployment_name: plan.deployment_name.clone(),
            total_layers,
            targets,
            primary_node_id: primary,
            params,
            use_mmproj,
        }),
    )
    .await;

    if !response.status().is_success() {
        return Err(refusal(response).await);
    }

    // Só depois do launch dar certo: um registro de implantação que não subiu
    // seria uma implantação fantasma na listagem, e a tela de desimplantar
    // ofereceria algo que não está no ar.
    publish_record(&state, &registro).await;
    Ok(())
}

/// Monta o registro a partir do que o plano e os alvos já dizem.
///
/// Nada aqui é recalculado: as faixas, a posição no pipeline e os ajustes por
/// máquina são exatamente os que foram para o `launch`. Guardar o **resultado**
/// é o que faz "com que configuração aquela máquina subiu?" ser uma leitura em
/// vez de uma reconstrução.
/// Os nós de uma versão: quem recebeu qual faixa e com que configuração
/// efetiva. Igual para a v1 de um deployment novo e para uma versão anexada a
/// um `deploy_id` reaproveitado.
pub(super) fn build_version_nodes(
    plan: &Plan,
    targets: &[LaunchTarget],
    machines: &[Machine],
    params: &RuntimeParams,
) -> Vec<crate::deployment::DeploymentNode> {
    let total_alvos = targets.len() as u32;
    targets
        .iter()
        .enumerate()
        .map(|(i, t)| crate::deployment::DeploymentNode {
            node_id: t.node_id.clone(),
            hostname: machines
                .iter()
                .find(|m| m.node_id == t.node_id)
                .map(|m| m.label())
                .unwrap_or_default(),
            start_layer: t.start_layer,
            end_layer: t.end_layer,
            peer_index: t.peer_index.unwrap_or(i as u32),
            peer_count: t.peer_count.unwrap_or(total_alvos),
            primary: plan
                .steps
                .iter()
                .any(|s| s.primary && s.target_node_id == t.node_id),
            part_sha256: t.part_sha256.clone(),
            source_model_path: (!t.source_model_path.is_empty())
                .then(|| t.source_model_path.clone()),
            tp_rank: t.tp.as_ref().map(|tp| tp.rank),
            // A mesma função que o `launch` usa para decidir o que cada máquina
            // recebe, para o registro não poder divergir do que subiu.
            config: crate::webui::params_for(params, t),
        })
        .collect()
}

/// O kind do registro a partir do kind do plano.
pub(super) fn record_kind(kind: DeployKind) -> crate::deployment::DeploymentKind {
    match kind {
        DeployKind::Shards => crate::deployment::DeploymentKind::Shards,
        DeployKind::Whole => crate::deployment::DeploymentKind::Whole,
        DeployKind::TensorParallel => crate::deployment::DeploymentKind::TensorParallel,
    }
}

/// Monta o registro de um deployment **novo** (v1), a partir do que o plano e
/// os alvos já dizem.
///
/// Nada aqui é recalculado: as faixas, a posição no pipeline e os ajustes por
/// máquina são exatamente os que foram para o `launch`. Guardar o **resultado**
/// é o que faz "com que configuração aquela máquina subiu?" ser uma leitura em
/// vez de uma reconstrução.
pub(super) fn build_record(
    deploy_id: &str,
    plan: &Plan,
    targets: &[LaunchTarget],
    machines: &[Machine],
    total_layers: u32,
    params: &RuntimeParams,
    identity: &RequestedModelIdentity,
    created_by: &str,
) -> crate::deployment::Deployment {
    let nodes = build_version_nodes(plan, targets, machines, params);

    crate::deployment::Deployment::new(
        deploy_id.to_string(),
        record_kind(plan.kind),
        crate::deployment::DeploymentModel {
            label: plan.group.clone(),
            total_layers,
            pom_id: match identity.proof.as_ref() {
                Some(crate::deployment::DeploymentIdentity::PomId { pom_id }) => {
                    Some(pom_id.clone())
                }
                _ => None,
            },
            identity: identity.proof.clone(),
        },
        created_by,
        nodes,
    )
    .with_params(params.clone())
}

/// Grava o registro neste nó e manda uma cópia para cada participante.
///
/// Cada nó guarda o próprio exemplar: não há dono único que, ao cair, leve a
/// implantação junto, e um nó que reinicia lê no disco de que implantação ele
/// fazia parte. Uma cópia que não chega não derruba a implantação - ela já está
/// no ar; o que se perde é aquele nó saber se explicar depois de um reinício.
pub(super) async fn publish_record(state: &GatewayState, registro: &crate::deployment::Deployment) {
    if let Err(e) = state.database.save_deployment(registro) {
        warn!(
            "não deu para anotar a implantação {}: {e:#}",
            registro.deploy_id
        );
    }

    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;

    // Atualizações de topologia também precisam alcançar quem saiu. O peer
    // só aceita a cópia se já conhecia o deployment, então fan-out não cria
    // registros em máquinas que nunca participaram.
    for peer in peers.iter().filter(|peer| peer.node_id != self_id) {
        let Some(base) = peer.gateway_url() else {
            warn!(
                "{} não anuncia gateway; sem cópia do registro",
                peer.node_id
            );
            continue;
        };
        let Ok(cliente) = state.peer_http.for_peer(&peer.node_id) else {
            continue;
        };
        let envio = match state
            .control_json_request(
                cliente.post(format!("{base}/api/ui/deployments")),
                "/api/ui/deployments",
                registro,
            )
            .await
        {
            Ok(request) => request.timeout(PEER_TIMEOUT).send().await,
            Err(error) => {
                warn!(
                    "{} não aceitou assinar o registro da implantação: {error}",
                    peer.node_id
                );
                continue;
            }
        };
        if let Err(e) = envio {
            warn!(
                "{} não aceitou o registro da implantação: {e}",
                peer.node_id
            );
        }
    }
}

// ── Mexer numa implantação que já está no ar ─────────────────────────────────

/// Acha o registro de uma implantação, aqui ou em quem tiver.
///
/// Nem todo nó guarda todo registro - só os participantes guardam -, então o
/// coordenador de um `redeploy` pode perfeitamente não ter a cópia.
pub(super) async fn find_record(
    state: &GatewayState,
    deploy_id: &str,
) -> Option<crate::deployment::Deployment> {
    let local = state.database.deployment(deploy_id).ok().flatten();

    let self_id = state.network.node_id.clone();
    let buscas = state.all_peers().await.into_iter().filter_map(|peer| {
        if peer.node_id == self_id {
            return None;
        }
        let http = state.peer_http.for_peer(&peer.node_id).ok()?;
        let base = peer.gateway_url()?;
        let peer_state = state.clone();
        let alvo = deploy_id.to_string();
        Some(async move {
            let request = peer_state
                .control_get_request(
                    http.get(format!("{base}/api/ui/deployments?local=true")),
                    "/api/ui/deployments",
                )
                .await
                .ok()?;
            let body: serde_json::Value = request
                .timeout(PEER_TIMEOUT)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            let registros: Vec<crate::deployment::Deployment> =
                serde_json::from_value(body.get("deployments")?.clone()).ok()?;
            registros
                .into_iter()
                .find(|d| d.deploy_id == alvo)
                .map(|mut d| {
                    d.ensure_versions();
                    d
                })
        })
    });

    let remotas = futures::future::join_all(buscas)
        .await
        .into_iter()
        .flatten();
    newest_deployment(local.into_iter().chain(remotas))
}

/// Torna esta a implantação que atende quem pede só o nome do modelo.
///
/// Mexe no `activated_at` e reespalha. Como a escolha da padrão é **derivada**
/// (a de ativação mais recente), não há eleição nem quórum: basta o carimbo
/// novo chegar, e quem receber converge sozinho. Ver `deployment::active`.
pub(super) async fn activate_deployment(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
) -> Response {
    if !crate::deployment::is_deploy_id(&id) {
        return bad(json!({ "error": format!("deploy_id inválido: {id}") }));
    }
    let Some(mut registro) = find_record(&state, &id).await else {
        return bad(json!({ "error": format!("implantação {id} desconhecida") }));
    };

    registro.activated_at = chrono::Utc::now().to_rfc3339();
    publish_record(&state, &registro).await;

    Json(json!({
        "activated": true,
        "deploy_id": registro.deploy_id,
        "model": registro.model.label,
    }))
    .into_response()
}

/// Ajuste por máquina, para os dois botões que são de verdade por máquina.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PerNodeParams {
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    #[serde(default)]
    pub offload_kqv: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RedeployRequest {
    /// Parâmetros novos do deployment inteiro. Ausente exige `version`.
    /// Presentes e iguais aos de uma versão existente -> aquela versão volta a
    /// ser a ativa; diferentes -> versão nova anexada ao mesmo `deploy_id`.
    #[serde(default)]
    pub params: Option<RuntimeParams>,
    /// Reimplanta uma versão já registrada, sem anexar outra. Mutuamente
    /// exclusivo com `params` na prática; se os dois vierem, `version` vence.
    #[serde(default)]
    pub version: Option<u32>,
    /// Ajustes por máquina, por `node_id`. Ausente = todo mundo com os mesmos.
    #[serde(default)]
    pub per_node: HashMap<String, PerNodeParams>,
}

impl RedeployRequest {
    /// Resolve o pedido contra o registro: escolhe/cria a versão, muta o
    /// registro para refleti-la e devolve os parâmetros base de cada máquina.
    pub(super) fn resolve(
        &self,
        registro: &mut crate::deployment::Deployment,
        autor: &str,
    ) -> Result<RuntimeParams, String> {
        registro.ensure_versions();
        if let Some(v) = self.version {
            registro.set_active_version(v).map_err(|e| e.to_string())?;
            let active_version = registro.active_version;
            let (params, nodes) = {
                let version = registro
                    .versions
                    .iter_mut()
                    .find(|ver| ver.version == active_version)
                    .expect("set_active_version garantiu a versão ativa");
                (version.params.clone(), version.nodes.clone())
            };
            registro.nodes = nodes;
            return Ok(params);
        }
        let Some(params) = self.params.clone() else {
            return Err("informe `version` ou `params`".to_string());
        };
        let mut nodes = registro.nodes.clone();
        for n in &mut nodes {
            let mut cfg = params.clone();
            if let Some(adj) = self.per_node.get(&n.node_id) {
                if let Some(g) = adj.n_gpu_layers {
                    cfg.n_gpu_layers = g;
                }
                if let Some(k) = adj.offload_kqv {
                    cfg.offload_kqv = k;
                }
            }
            n.config = cfg;
        }
        registro.upsert_version(params.clone(), nodes, autor);
        Ok(params)
    }
}

/// O que aconteceu com **uma** máquina numa reconfiguração.
#[derive(Debug, Serialize)]
pub struct RedeployOutcome {
    pub node_id: String,
    pub hostname: String,
    pub reconfigured: bool,
    pub error: Option<String>,
}

/// Reimplanta um `deploy_id` que já existe: `version` reativa uma versão
/// registrada, `params` reimplanta anexando - ou reativando - uma versão.
///
/// A mesma implantação, o mesmo `deploy_id`, as mesmas máquinas e as mesmas
/// faixas. Por isso **não** re-planeja: re-planejar cunharia uma implantação
/// nova, quebrando todo cliente preso a `modelo@deploy_id`.
///
/// Descarrega toda máquina que ainda anuncia este `deploy_id` e sobe cada uma
/// de novo, uma de cada vez, esperando cada uma voltar a servir antes da
/// próxima. Funciona mesmo se uma máquina não estava servindo nada (o bug
/// antigo): a identidade da fatia vem do registro mais a fotografia do que
/// cada nó rodava, não de um `NodeConfig` vivo. Tensor Parallelism ainda usa
/// o caminho antigo (`reconfigure` no lugar). O registro é reescrito
/// **depois** de os nós aceitarem.
pub(super) async fn redeploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Json(req): Json<RedeployRequest>,
) -> Response {
    if !crate::deployment::is_deploy_id(&id) {
        return bad(json!({ "error": format!("deploy_id inválido: {id}") }));
    }
    let Some(mut registro) = find_record(&state, &id).await else {
        return bad(json!({ "error": format!("implantação {id} desconhecida") }));
    };
    registro.ensure_versions();
    if req.version.is_none() && req.params.is_none() {
        return bad(json!({ "error": "informe `version` ou `params`" }));
    }
    if let Some(v) = req.version {
        if !registro.versions.iter().any(|ver| ver.version == v) {
            let disponiveis: Vec<u32> = registro.versions.iter().map(|ver| ver.version).collect();
            return bad(json!({
                "error": format!("versão {v} não existe (há {disponiveis:?})")
            }));
        }
    }
    info!(
        "redeploy pedido para {} ({}), {} nó(s), versão={:?}",
        id,
        registro.model.label,
        registro.nodes.len(),
        req.version
    );

    let job_id = uuid::Uuid::new_v4().to_string();
    let titulo = format!(
        "Reconfigurar {} em {} {}",
        registro.model.label,
        registro.nodes.len(),
        if registro.nodes.len() == 1 {
            "nó"
        } else {
            "nós"
        }
    );
    jobs().write().await.insert(
        job_id.clone(),
        new_job(&job_id, "redeploy", titulo, "recarregando"),
    );

    let id_job = job_id.clone();
    tokio::spawn(async move {
        match run_redeploy(state, registro, req, &id_job).await {
            Ok(falhas) if falhas == 0 => {
                set_job(&id_job, "done", "pronto", "as máquinas recarregaram").await
            }
            Ok(falhas) => {
                set_job(
                    &id_job,
                    "failed",
                    "parcial",
                    &format!("{falhas} máquina(s) não recarregaram"),
                )
                .await
            }
            Err(e) => {
                warn!("reconfiguração falhou: {e}");
                set_job(&id_job, "failed", "falhou", &e).await
            }
        }
    });

    Json(json!({ "job_id": job_id, "deploy_id": id })).into_response()
}

pub(super) async fn run_redeploy(
    state: GatewayState,
    mut registro: crate::deployment::Deployment,
    req: RedeployRequest,
    job_id: &str,
) -> Result<usize, String> {
    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;
    let total = registro.nodes.len().max(1);

    info!(
        "redeploy {} ({}): {} nó(s), kind={:?}",
        registro.deploy_id,
        registro.model.label,
        registro.nodes.len(),
        registro.kind
    );

    // Escolhe/cria a versão pedida e muta `registro` para refleti-la. `base` é
    // o `RuntimeParams` do deployment inteiro dessa versão; os ajustes por
    // máquina de `req.per_node` entram por cima na hora do reconfigure.
    let base = req.resolve(&mut registro, &self_id)?;

    // Re-deploy does not build a new plan, but changing the wire precision is
    // still a cluster-wide protocol change. Probe every participant before
    // touching the first process, otherwise a later incompatible peer could
    // leave one part of the same deployment on f8 and another on f16.
    validate_redeploy_wire_dtype(
        &state,
        &peers,
        &registro.nodes,
        &self_id,
        base.hidden_state_dtype,
    )
    .await?;

    // Tensor parallelism: a coletiva não sobrevive a um unload e o registro
    // não guarda o `TpLaunch` inteiro, só o `tp_rank` de cada nó. Mantém o
    // caminho antigo (reconfigure no lugar), que exige o rank já servindo.
    if registro.kind == crate::deployment::DeploymentKind::TensorParallel {
        return run_redeploy_in_place(&state, &peers, &mut registro, &req, &self_id, &key, job_id)
            .await;
    }

    // ── 1. Fotografa a identidade viva de cada nó antes de derrubar nada ──────
    //
    // O registro tem faixa, posição e `part_sha256`, mas não o caminho do
    // arquivo nem o nome operacional da implantação. Esses vêm do `NodeConfig`
    // vivo, lido agora, enquanto ainda dá.
    let mut identidade: HashMap<String, NodeIdentitySnapshot> = HashMap::new();
    for no in &registro.nodes {
        let Some(peer) = peers.iter().find(|p| p.node_id == no.node_id) else {
            continue;
        };
        if let Ok(snapshot) = peer_node_snapshot(&state, peer).await {
            let config = snapshot.get("config");
            identidade.insert(
                no.node_id.clone(),
                NodeIdentitySnapshot {
                    model_path: config
                        .and_then(|c| c.get("model_path"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    mmproj_path: config
                        .and_then(|c| c.get("mmproj_path"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    model_label: config
                        .and_then(|c| c.get("model_label"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    deployment_name: config
                        .and_then(|c| c.get("deployment_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    single_peer: config
                        .and_then(|c| c.get("single_peer"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(no.peer_count <= 1),
                    no_embeddings: config
                        .and_then(|c| c.get("no_embeddings"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                },
            );
        }
    }

    // ── 2. Descarrega toda máquina que anuncia este deploy_id ────────────────
    let alvo = UndeployTarget::Deployment {
        deploy_id: registro.deploy_id.clone(),
        label: registro.model.label.clone(),
    };
    let servindo: Vec<PeerInfo> = peers
        .iter()
        .filter(|p| p.total_layers > 0 && alvo.matches(p))
        .cloned()
        .collect();
    if servindo.is_empty() {
        info!(
            "redeploy {}: nada servindo, indo direto para a subida",
            registro.deploy_id
        );
    } else {
        set_job(
            job_id,
            "running",
            "descarregando",
            &format!("{} máquina(s)", servindo.len()),
        )
        .await;
        let unloads = futures::future::join_all(servindo.iter().map(|peer| {
            let state = state.clone();
            let key = key.clone();
            let self_id = self_id.clone();
            async move {
                (
                    peer.hostname.clone(),
                    release_one(&state, peer, &key, &self_id).await,
                )
            }
        }))
        .await;
        for (host, resultado) in unloads {
            match resultado {
                Ok(()) => info!("redeploy: {host} descarregada"),
                Err(e) => warn!("redeploy: {host} não descarregou: {e}"),
            }
        }
    }

    // ── 3. Sobe uma máquina de cada vez, esperando cada uma voltar ───────────
    //
    // Sequencial de propósito: recarregar o modelo é o pico de memória do nó,
    // e mandar o cluster inteiro fazer isso junto é o caminho mais curto para
    // várias quedas por falta de VRAM ao mesmo tempo.
    let mut resultados: Vec<RedeployOutcome> = Vec::new();
    let mut aceitos: HashMap<String, RuntimeParams> = HashMap::new();
    for (i, no) in registro.nodes.iter().enumerate() {
        set_job(
            job_id,
            "running",
            "subindo",
            &format!("{} ({}/{})", no.hostname, i + 1, total),
        )
        .await;
        set_job_progress(job_id, 100.0 * i as f64 / total as f64).await;

        // `no.config` já é a configuração efetiva da versão ativa (os
        // parâmetros do deployment com os ajustes por máquina embutidos), então
        // uma reimplantação de versão devolve a cada máquina exatamente o que
        // ela rodava. `req.per_node` re-aplica por cima o que a tela mexeu.
        let mut params = no.config.clone();
        if let Some(ajuste) = req.per_node.get(&no.node_id) {
            if let Some(g) = ajuste.n_gpu_layers {
                params.n_gpu_layers = g;
            }
            if let Some(k) = ajuste.offload_kqv {
                params.offload_kqv = k;
            }
        }

        let snap = identidade.get(&no.node_id);
        let slice = ReconfigureSlice {
            model_path: snap
                .and_then(|s| s.model_path.clone())
                .or_else(|| no.source_model_path.clone()),
            mmproj_path: snap.and_then(|s| s.mmproj_path.clone()),
            part_sha256: no.part_sha256.clone(),
            model_label: snap
                .and_then(|s| s.model_label.clone())
                .unwrap_or_else(|| registro.model.label.clone()),
            deployment_name: snap
                .map(|s| s.deployment_name.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| registro.model.label.clone()),
            deploy_id: registro.deploy_id.clone(),
            start_layer: no.start_layer,
            end_layer: no.end_layer,
            total_layers: registro.model.total_layers,
            peer_count: no.peer_count,
            peer_index: no.peer_index,
            primary: no.primary,
            single_peer: snap.map(|s| s.single_peer).unwrap_or(no.peer_count <= 1),
            no_embeddings: Some(snap.map(|s| s.no_embeddings).unwrap_or(false)),
            tp: None,
        };

        match relaunch_one(&state, &peers, no, &self_id, &key, &params, slice).await {
            None => {
                info!("redeploy: {} subiu", no.hostname);
                aceitos.insert(no.node_id.clone(), params);
                resultados.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: true,
                    error: None,
                });
            }
            Some(e) => {
                warn!("redeploy: {} não subiu: {e}", no.hostname);
                resultados.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: false,
                    error: Some(e),
                });
            }
        }
    }

    finish_redeploy(&state, &mut registro, aceitos, resultados, job_id).await
}

/// A fotografia do que um nó estava rodando, tirada antes do unload.
pub(super) struct NodeIdentitySnapshot {
    pub(super) model_path: Option<String>,
    pub(super) mmproj_path: Option<String>,
    pub(super) model_label: Option<String>,
    pub(super) deployment_name: String,
    pub(super) single_peer: bool,
    pub(super) no_embeddings: bool,
}

/// Grava os parâmetros aceitos no registro, publica e monta o resultado do job.
pub(super) async fn finish_redeploy(
    state: &GatewayState,
    registro: &mut crate::deployment::Deployment,
    mut aceitos: HashMap<String, RuntimeParams>,
    resultados: Vec<RedeployOutcome>,
    job_id: &str,
) -> Result<usize, String> {
    // Só o que a máquina de fato aceitou entra no registro. Uma que recusou
    // continua descrita com a configuração antiga - o registro tem que dizer o
    // que **é**, não o que se pediu.
    for no in registro.nodes.iter_mut() {
        if let Some(p) = aceitos.remove(&no.node_id) {
            no.config = p;
        }
    }
    // O que cada máquina de fato aceitou entra também no snapshot da versão
    // ativa, para o histórico não divergir da lista que o runtime lê.
    registro.commit_active_nodes();
    publish_record(state, registro).await;

    let falhas = resultados.iter().filter(|r| !r.reconfigured).count();
    set_job_progress(job_id, 100.0).await;
    {
        let mut map = jobs().write().await;
        if let Some(j) = map.get_mut(job_id) {
            j.outputs = resultados
                .iter()
                .map(|r| match &r.error {
                    Some(e) => format!("{}: {e}", r.hostname),
                    None => format!("{}: recarregada", r.hostname),
                })
                .collect();
        }
    }
    Ok(falhas)
}

/// Sobe **uma** máquina para a faixa que ela já tem, com os parâmetros novos,
/// e espera ela voltar a servir antes de devolver.
pub(super) async fn relaunch_one(
    state: &GatewayState,
    peers: &[PeerInfo],
    no: &crate::deployment::DeploymentNode,
    self_id: &str,
    _key: &str,
    params: &RuntimeParams,
    slice: ReconfigureSlice,
) -> Option<String> {
    if no.node_id == self_id {
        if let Err(e) =
            crate::webui::reconfigure_local(state.clone(), params.clone(), Some(slice)).await
        {
            return Some(e);
        }
    } else {
        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == no.node_id)
            .and_then(|p| p.gateway_url())
        else {
            return Some("peer não encontrado ou sem porta de gateway anunciada".to_string());
        };
        let Ok(cliente) = state.peer_http.for_peer(&no.node_id) else {
            return Some("sem cliente para o peer".to_string());
        };
        let corpo = json!({ "params": params, "slice": slice });
        let request = match state
            .control_json_request(
                cliente.post(format!("{base}/api/ui/reconfigure")),
                "/api/ui/reconfigure",
                &corpo,
            )
            .await
        {
            Ok(request) => request,
            Err(error) => return Some(format!("não foi possível assinar a chamada: {error}")),
        };
        match request
            // Recarregar pesos é minutos, não segundos.
            .timeout(REDEPLOY_READY_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                let status = r.status();
                let detalhe = r.text().await.unwrap_or_default();
                return Some(format!("a máquina recusou: HTTP {status} {detalhe}"));
            }
            Err(e) => return Some(format!("máquina inacessível: {e}")),
        }
    }

    // A chamada só dispara a carga; espera a máquina reportar `ready`.
    if let Some(peer) = peers.iter().find(|p| p.node_id == no.node_id) {
        if let Err(e) =
            wait_for_peer_serving_with_timeout(state, peer, REDEPLOY_READY_TIMEOUT).await
        {
            return Some(e);
        }
    }
    None
}

/// O `redeploy` antigo: reconfigura cada nó no lugar, sem unload. Só o Tensor
/// Parallelism ainda usa este caminho - ver `run_redeploy`.
pub(super) async fn run_redeploy_in_place(
    state: &GatewayState,
    peers: &[PeerInfo],
    registro: &mut crate::deployment::Deployment,
    req: &RedeployRequest,
    self_id: &str,
    key: &str,
    job_id: &str,
) -> Result<usize, String> {
    let total = registro.nodes.len().max(1);
    let mut resultados: Vec<RedeployOutcome> = Vec::new();
    let mut aceitos: HashMap<String, RuntimeParams> = HashMap::new();

    for (i, no) in registro.nodes.iter().enumerate() {
        set_job(
            job_id,
            "running",
            "recarregando",
            &format!("{} ({}/{})", no.hostname, i + 1, total),
        )
        .await;
        set_job_progress(job_id, 100.0 * i as f64 / total as f64).await;

        // Ver `run_redeploy`: `no.config` é a configuração efetiva da versão.
        let mut params = no.config.clone();
        if let Some(ajuste) = req.per_node.get(&no.node_id) {
            if let Some(g) = ajuste.n_gpu_layers {
                params.n_gpu_layers = g;
            }
            if let Some(k) = ajuste.offload_kqv {
                params.offload_kqv = k;
            }
        }

        match reconfigure_one(state, peers, &no.node_id, self_id, key, &params).await {
            None => {
                info!("redeploy (tp): {} recarregada", no.hostname);
                aceitos.insert(no.node_id.clone(), params);
                resultados.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: true,
                    error: None,
                });
            }
            Some(e) => {
                warn!("redeploy (tp): {} não recarregou: {e}", no.hostname);
                resultados.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: false,
                    error: Some(e),
                });
            }
        }
    }

    finish_redeploy(state, registro, aceitos, resultados, job_id).await
}

pub(super) async fn validate_redeploy_wire_dtype(
    state: &GatewayState,
    peers: &[PeerInfo],
    nodes: &[crate::deployment::DeploymentNode],
    self_id: &str,
    dtype: crate::wire::WireDtype,
) -> Result<(), String> {
    let Some(required_capability) = crate::pipeline::wire_dtype_capability(dtype) else {
        return Ok(());
    };

    let mut blocked = Vec::new();
    for node in nodes {
        if node.node_id == self_id {
            continue;
        }
        let Some(peer) = peers.iter().find(|peer| peer.node_id == node.node_id) else {
            blocked.push(format!("{}: peer não encontrado", node.hostname));
            continue;
        };
        let Some(base) = peer.gateway_url() else {
            blocked.push(format!("{}: peer sem gateway anunciado", node.hostname));
            continue;
        };
        let Ok(client) = state.peer_http.for_peer(&node.node_id) else {
            blocked.push(format!("{}: cliente do peer indisponível", node.hostname));
            continue;
        };
        let request = match state
            .control_get_request(client.get(format!("{base}/api/ui/node")), "/api/ui/node")
            .await
        {
            Ok(request) => request,
            Err(error) => {
                blocked.push(format!(
                    "{}: não foi possível assinar /api/ui/node: {error}",
                    node.hostname
                ));
                continue;
            }
        };
        let response = match request.timeout(PEER_TIMEOUT).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                blocked.push(format!(
                    "{}: /api/ui/node respondeu HTTP {}",
                    node.hostname,
                    response.status()
                ));
                continue;
            }
            Err(error) => {
                blocked.push(format!(
                    "{}: não foi possível consultar /api/ui/node: {error}",
                    node.hostname
                ));
                continue;
            }
        };
        let body = match response.json::<serde_json::Value>().await {
            Ok(body) => body,
            Err(error) => {
                blocked.push(format!(
                    "{}: resposta de /api/ui/node inválida: {error}",
                    node.hostname
                ));
                continue;
            }
        };
        let supports_dtype = body
            .get("inference_frame_capabilities")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|capabilities| {
                capabilities
                    .iter()
                    .any(|capability| capability.as_str() == Some(required_capability))
            });
        if !supports_dtype {
            blocked.push(format!(
                "{}: não anuncia {}",
                node.hostname, required_capability
            ));
        }
    }

    if blocked.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "A precisão {} do estado oculto foi solicitada, mas a negociação falhou: {}",
            dtype.as_str(),
            blocked.join("; ")
        ))
    }
}

/// Manda **uma** máquina recarregar com outros parâmetros.
pub(super) async fn reconfigure_one(
    state: &GatewayState,
    peers: &[PeerInfo],
    node_id: &str,
    self_id: &str,
    _key: &str,
    params: &RuntimeParams,
) -> Option<String> {
    let corpo = json!({ "params": params });

    if node_id == self_id {
        let resposta = crate::webui::reconfigure_local(state.clone(), params.clone(), None).await;
        return resposta.err();
    }

    let base = peers
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.gateway_url())?;
    let cliente = state.peer_http.for_peer(node_id).ok()?;
    let request = state
        .control_json_request(
            cliente.post(format!("{base}/api/ui/reconfigure")),
            "/api/ui/reconfigure",
            &corpo,
        )
        .await
        .ok()?;
    match request
        // Recarregar pesos é minutos, não segundos: o `PEER_TIMEOUT` de uma
        // chamada de controle mataria a conexão no meio de um carregamento que
        // vai terminar bem.
        .timeout(Duration::from_secs(1800))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => None,
        Ok(r) => {
            let status = r.status();
            let detalhe = r.text().await.unwrap_or_default();
            Some(format!("a máquina recusou: HTTP {status} {detalhe}"))
        }
        Err(e) => Some(format!("máquina inacessível: {e}")),
    }
}
