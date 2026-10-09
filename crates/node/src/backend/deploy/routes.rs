use super::*;
pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/deploy/plan", post(plan_endpoint))
        .route("/api/ui/deploy", post(deploy_endpoint))
        .route(
            "/api/ui/deploy/node/options",
            post(add_node_options_endpoint),
        )
        .route("/api/ui/deploy/node", post(add_node_endpoint))
        .route("/api/ui/deploy/node/remove", post(remove_node_endpoint))
        .route("/api/ui/undeploy", post(undeploy_endpoint))
        .route(
            "/api/ui/deployments",
            get(list_deployments).post(receive_deployment),
        )
        .route(
            "/api/ui/deployments/:id/activate",
            post(activate_deployment),
        )
        .route("/api/ui/deployments/:id/redeploy", post(redeploy_endpoint))
}

// ── O registro das implantações ──────────────────────────────────────────────

/// A ativação é a versão corrente de um `deploy_id` no cluster. Quando dois
/// nós ainda discordam, o carimbo mais novo vence; a versão desempata cópias
/// produzidas no mesmo instante.
pub(super) fn deployment_is_newer(
    candidate: &crate::deployment::Deployment,
    current: &crate::deployment::Deployment,
) -> bool {
    candidate.activated_at > current.activated_at
        || (candidate.activated_at == current.activated_at
            && candidate.active_version > current.active_version)
}

pub(super) fn newest_deployment(
    records: impl IntoIterator<Item = crate::deployment::Deployment>,
) -> Option<crate::deployment::Deployment> {
    records.into_iter().fold(None, |current, candidate| {
        Some(match current {
            Some(current) if !deployment_is_newer(&candidate, &current) => current,
            _ => candidate,
        })
    })
}

/// Só as implantações que este nó guarda em disco, sem perguntar a ninguém.
///
/// É o que um peer responde quando o coordenador vem juntar a visão do cluster
/// (`?local=1`), e é o que impede a varredura de se chamar em círculo.
pub(super) fn local_deployments(
    self_id: &str,
    database: &crate::db::PomDb,
) -> anyhow::Result<serde_json::Value> {
    let all = database.deployments()?;
    Ok(json!({ "node_id": self_id, "deployments": all }))
}

#[derive(Debug, Default, Deserialize)]
pub struct ListDeploymentsQuery {
    /// Responder só com os registros deste nó, sem varrer o cluster.
    #[serde(default)]
    pub local: bool,
}

/// Todas as implantações do cluster, com o estado de cada uma.
///
/// O registro de uma implantação vive numa cópia por nó participante, então
/// **nenhuma máquina sozinha sabe a lista inteira**: esta rota pergunta a cada
/// peer e junta, que é o mesmo desenho de `survey`. Cópias do mesmo
/// `deploy_id` são deduplicadas ficando com a de `activated_at` mais recente -
/// um nó que estava fora do ar durante um `activate` tem a cópia velha.
///
/// Duas palavras que **não** são sinônimos, e a tela não pode confundir:
///
/// * `serving` - há peer vivo anunciando este `deploy_id`. É "está no ar".
/// * `is_default` - é esta que atende quem pede só o nome do modelo, sem
///   sufixo `@`. Ver `deployment::active`.
///
/// Uma implantação pode estar no ar sem ser a padrão do seu modelo.
pub(super) async fn list_deployments(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(q): Query<ListDeploymentsQuery>,
) -> Response {
    let self_id = state.network.node_id.clone();
    if q.local {
        return match local_deployments(&self_id, &state.database) {
            Ok(body) => Json(body).into_response(),
            Err(error) => {
                tracing::error!("could not read local deployments: {error:#}");
                bad(json!({ "error": "could not read local deployments" }))
            }
        };
    }

    let peers = state.all_peers().await;
    // Por `deploy_id`, ficando com a cópia mais recentemente ativada.
    let mut by_id: HashMap<String, crate::deployment::Deployment> = HashMap::new();
    let mut stored_by: HashMap<String, Vec<String>> = HashMap::new();

    let absorb = |who: &str,
                   records: Vec<crate::deployment::Deployment>,
                   by_id: &mut HashMap<String, crate::deployment::Deployment>,
                   stored_by: &mut HashMap<String, Vec<String>>| {
        for mut d in records {
            d.ensure_versions();
            stored_by
                .entry(d.deploy_id.clone())
                .or_default()
                .push(who.to_string());
            if by_id
                .get(&d.deploy_id)
                .map(|current| deployment_is_newer(&d, current))
                .unwrap_or(true)
            {
                by_id.insert(d.deploy_id.clone(), d);
            }
        }
    };

    let local = match state.database.deployments() {
        Ok(local) => local,
        Err(error) => {
            tracing::error!("could not read local deployments: {error:#}");
            return bad(json!({ "error": "could not read local deployments" }));
        }
    };
    absorb(&self_id, local, &mut by_id, &mut stored_by);

    let peer_state = state.clone();
    let searches = peers.iter().filter(|p| p.node_id != self_id).map(|peer| {
        let state = peer_state.clone();
        let node_id = peer.node_id.clone();
        let peer = peer.clone();
        async move {
            let resp = state
                .control_get(&peer, "/api/ui/deployments?local=true", PEER_TIMEOUT)
                .await
                .ok()?;
            let body: serde_json::Value = resp.json().await.ok()?;
            let records: Vec<crate::deployment::Deployment> =
                serde_json::from_value(body.get("deployments")?.clone()).ok()?;
            Some((node_id, records))
        }
    });

    for (node_id, records) in futures::future::join_all(searches)
        .await
        .into_iter()
        .flatten()
    {
        absorb(&node_id, records, &mut by_id, &mut stored_by);
    }

    let all: Vec<crate::deployment::Deployment> = {
        let mut v: Vec<_> = by_id.into_values().collect();
        v.sort_by(|a, b| a.deploy_id.cmp(&b.deploy_id));
        v
    };

    // "No ar" é o que os peers anunciam, não o que o registro diz: o registro
    // sobrevive a um nó cair, e mostrá-lo como servindo seria mentir para quem
    // está tentando entender por que a inferência parou.
    let announced: std::collections::BTreeSet<String> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.deploy_id.is_empty())
        .map(|p| p.deploy_id.clone())
        .collect();
    let lines: Vec<serde_json::Value> = all
        .iter()
        .map(|d| {
            let is_default = crate::deployment::active(&all, &d.model.label)
                .is_some_and(|a| a.deploy_id == d.deploy_id);
            let mut holders = stored_by.get(&d.deploy_id).cloned().unwrap_or_default();
            holders.sort();
            holders.dedup();
            json!({
                "deployment": d,
                "serving": announced.contains(&d.deploy_id),
                "idle": announced.contains(&d.deploy_id)
                    && peers.iter().any(|p| p.is_alive() && p.deploy_id == d.deploy_id)
                    && peers.iter().filter(|p| p.is_alive() && p.deploy_id == d.deploy_id).all(|p| p.idle),
                "is_default": is_default,
                "holders": holders,
            })
        })
        .collect();

    Json(json!({ "node_id": self_id, "deployments": lines })).into_response()
}

/// Recebe do coordenador o registro de uma implantação de que este nó faz parte.
///
/// Guardar é o que permite a este nó, depois de um reinício, dizer de que
/// conjunto ele fazia parte - e é o que faz a listagem do cluster sobreviver à
/// queda de quem coordenou.
pub(super) async fn receive_deployment(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(record): Json<crate::deployment::Deployment>,
) -> Response {
    let self_id = state.network.node_id.clone();
    // Um registro que não menciona esta máquina não é dela para guardar. A
    // exceção é uma atualização que ela já conhecia: depois de remover uma
    // máquina, o snapshot precisa chegar também ao peer removido para que ele
    // não anuncie uma versão antiga no próximo fan-out.
    let already_knew = state
        .database
        .deployment(&record.deploy_id)
        .ok()
        .flatten()
        .is_some();
    if record.node(&self_id).is_none() && !already_knew {
        return bad(json!({
            "error": "this node is not part of the deployment"
        }));
    }
    // Aceita mesmo incompleto - uma implantação que não fecha o modelo é um
    // estado real do cluster, e recusar o registro só faria a máquina esquecer
    // de que conjunto ela faz parte. Mas dizer em voz alta, porque uma
    // cobertura furada só se manifestaria muito depois, como "Gap at layer N"
    // na primeira requisição.
    if !record.covers_the_model() {
        warn!(
            "deployment {} does not cover the {} layers of {}",
            record.deploy_id, record.model.total_layers, record.model.label
        );
    }
    match state.database.save_deployment(&record) {
        Ok(_) => Json(json!({ "stored": true, "deploy_id": record.deploy_id })).into_response(),
        Err(e) => bad(json!({ "error": format!("{e:#}") })),
    }
}

/// Camadas que a faixa nova compartilha com as que ja estao no ar.
///
/// Nao e recusa: e o que a tela mostra ao operador para ele saber que esta
/// criando uma rota paralela em vez de preencher um buraco.
pub fn overlapping_layers(existing: &[(u32, u32)], new: (u32, u32)) -> Vec<u32> {
    (new.0..=new.1)
        .filter(|c| existing.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}

/// Posicao desta faixa na ordem das camadas, e quantas faixas ficam no total.
///
/// `peer_index`/`peer_count` nao sao a lista de alvos de uma implantacao: sao
/// a posicao no pipeline, que e a ordem das camadas. Quem soma um no de cada
/// vez precisa dizer isso explicitamente, senao o unico alvo da requisicao
/// vira "peer 1 de 1" e todo no somado se declara o ultimo da divisao.
///
/// Réplicas da mesma faixa ocupam o mesmo estágio lógico, mesmo quando entram
/// pelo endpoint incremental de adicionar uma máquina.
pub fn split_position(existing: &[(u32, u32)], new: (u32, u32)) -> (u32, u32) {
    let mut ranges: Vec<(u32, u32)> = existing.to_vec();
    ranges.push(new);
    ranges.sort();
    ranges.dedup();
    let index = ranges.iter().position(|f| *f == new).unwrap_or(0) as u32;
    (index, ranges.len() as u32)
}

/// Camadas que ficam sem ninguem depois de tirar esta maquina.
///
/// Vazio significa que outra maquina cobre a mesma faixa - a rede perde uma
/// rota paralela e segue servindo, e o aviso ao operador e ameno. Nao vazio
/// significa buraco, e ai a confirmacao e explicita.
pub fn uncovered_after_removal(remaining: &[(u32, u32)], removed: (u32, u32)) -> Vec<u32> {
    (removed.0..=removed.1)
        .filter(|c| !remaining.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}
