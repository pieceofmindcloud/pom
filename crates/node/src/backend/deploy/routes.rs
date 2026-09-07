use super::*;
pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/deploy/plan", post(plan_endpoint))
        .route("/api/ui/deploy", post(deploy_endpoint))
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
    let todas = database.deployments()?;
    Ok(json!({ "node_id": self_id, "deployments": todas }))
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
                tracing::error!("não foi possível ler implantações locais: {error:#}");
                bad(json!({ "error": "não foi possível ler implantações locais" }))
            }
        };
    }

    let peers = state.all_peers().await;
    let key = state.auth.read().await.connection_key.clone();

    // Por `deploy_id`, ficando com a cópia mais recentemente ativada.
    let mut por_id: HashMap<String, crate::deployment::Deployment> = HashMap::new();
    let mut guardado_por: HashMap<String, Vec<String>> = HashMap::new();

    let absorve = |quem: &str,
                   registros: Vec<crate::deployment::Deployment>,
                   por_id: &mut HashMap<String, crate::deployment::Deployment>,
                   guardado_por: &mut HashMap<String, Vec<String>>| {
        for mut d in registros {
            d.ensure_versions();
            guardado_por
                .entry(d.deploy_id.clone())
                .or_default()
                .push(quem.to_string());
            if por_id
                .get(&d.deploy_id)
                .map(|atual| deployment_is_newer(&d, atual))
                .unwrap_or(true)
            {
                por_id.insert(d.deploy_id.clone(), d);
            }
        }
    };

    let local = match state.database.deployments() {
        Ok(local) => local,
        Err(error) => {
            tracing::error!("não foi possível ler implantações locais: {error:#}");
            return bad(json!({ "error": "não foi possível ler implantações locais" }));
        }
    };
    absorve(&self_id, local, &mut por_id, &mut guardado_por);

    let buscas = peers.iter().filter(|p| p.node_id != self_id).map(|peer| {
        let http = state.peer_http.for_peer(&peer.node_id);
        let key = key.clone();
        let node_id = peer.node_id.clone();
        let base = peer.gateway_url();
        async move {
            let base = base?;
            let resp = http
                .ok()?
                .get(format!("{base}/api/ui/deployments?local=true"))
                .bearer_auth(&key)
                .timeout(PEER_TIMEOUT)
                .send()
                .await
                .ok()?;
            let body: serde_json::Value = resp.json().await.ok()?;
            let registros: Vec<crate::deployment::Deployment> =
                serde_json::from_value(body.get("deployments")?.clone()).ok()?;
            Some((node_id, registros))
        }
    });

    for (node_id, registros) in futures::future::join_all(buscas)
        .await
        .into_iter()
        .flatten()
    {
        absorve(&node_id, registros, &mut por_id, &mut guardado_por);
    }

    let todas: Vec<crate::deployment::Deployment> = {
        let mut v: Vec<_> = por_id.into_values().collect();
        v.sort_by(|a, b| a.deploy_id.cmp(&b.deploy_id));
        v
    };

    // "No ar" é o que os peers anunciam, não o que o registro diz: o registro
    // sobrevive a um nó cair, e mostrá-lo como servindo seria mentir para quem
    // está tentando entender por que a inferência parou.
    let anunciados: std::collections::BTreeSet<String> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.deploy_id.is_empty())
        .map(|p| p.deploy_id.clone())
        .collect();

    let linhas: Vec<serde_json::Value> = todas
        .iter()
        .map(|d| {
            let padrao = crate::deployment::active(&todas, &d.model.label)
                .is_some_and(|a| a.deploy_id == d.deploy_id);
            let mut holders = guardado_por.get(&d.deploy_id).cloned().unwrap_or_default();
            holders.sort();
            holders.dedup();
            json!({
                "deployment": d,
                "serving": anunciados.contains(&d.deploy_id),
                "is_default": padrao,
                "holders": holders,
            })
        })
        .collect();

    Json(json!({ "node_id": self_id, "deployments": linhas })).into_response()
}

/// Recebe do coordenador o registro de uma implantação de que este nó faz parte.
///
/// Guardar é o que permite a este nó, depois de um reinício, dizer de que
/// conjunto ele fazia parte - e é o que faz a listagem do cluster sobreviver à
/// queda de quem coordenou.
pub(super) async fn receive_deployment(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(registro): Json<crate::deployment::Deployment>,
) -> Response {
    let self_id = state.network.node_id.clone();
    // Um registro que não menciona esta máquina não é dela para guardar. Sem
    // isto, qualquer peer autenticado encheria o disco dos outros de registros
    // de implantações que não os incluem.
    if registro.node(&self_id).is_none() {
        return bad(json!({
            "error": "este nó não faz parte da implantação"
        }));
    }
    // Aceita mesmo incompleto - uma implantação que não fecha o modelo é um
    // estado real do cluster, e recusar o registro só faria a máquina esquecer
    // de que conjunto ela faz parte. Mas dizer em voz alta, porque uma
    // cobertura furada só se manifestaria muito depois, como "Gap at layer N"
    // na primeira requisição.
    if !registro.covers_the_model() {
        warn!(
            "implantação {} não cobre as {} camadas de {}",
            registro.deploy_id, registro.model.total_layers, registro.model.label
        );
    }
    match state.database.save_deployment(&registro) {
        Ok(_) => Json(json!({ "stored": true, "deploy_id": registro.deploy_id })).into_response(),
        Err(e) => bad(json!({ "error": format!("{e:#}") })),
    }
}

/// Camadas que a faixa nova compartilha com as que ja estao no ar.
///
/// Nao e recusa: e o que a tela mostra ao operador para ele saber que esta
/// criando uma rota paralela em vez de preencher um buraco.
pub fn overlapping_layers(existentes: &[(u32, u32)], nova: (u32, u32)) -> Vec<u32> {
    (nova.0..=nova.1)
        .filter(|c| existentes.iter().any(|(i, f)| c >= i && c <= f))
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
pub fn split_position(existentes: &[(u32, u32)], nova: (u32, u32)) -> (u32, u32) {
    let mut faixas: Vec<(u32, u32)> = existentes.to_vec();
    faixas.push(nova);
    faixas.sort();
    faixas.dedup();
    let index = faixas.iter().position(|f| *f == nova).unwrap_or(0) as u32;
    (index, faixas.len() as u32)
}

/// Camadas que ficam sem ninguem depois de tirar esta maquina.
///
/// Vazio significa que outra maquina cobre a mesma faixa - a rede perde uma
/// rota paralela e segue servindo, e o aviso ao operador e ameno. Nao vazio
/// significa buraco, e ai a confirmacao e explicita.
pub fn uncovered_after_removal(restantes: &[(u32, u32)], removida: (u32, u32)) -> Vec<u32> {
    (removida.0..=removida.1)
        .filter(|c| !restantes.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}

