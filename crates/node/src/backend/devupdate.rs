//! Atualizar e reiniciar um nó pela interface, em desenvolvimento.
//!
//! Isto é execução remota de código: quem tem a chave de conexão passa a poder
//! rodar um build e trocar o binário da máquina. Por isso o portão é uma
//! variável de ambiente, ligada por processo por quem é dono da máquina, e não
//! um campo de config — a config é reescrita pela própria interface, o que
//! deixaria o portão ligável pela rede.

/// Nome da variável que abre o recurso (`POM_DEV_UPDATE`), derivado da marca.
pub fn env_gate() -> String {
    crate::brand::env_var("DEV_UPDATE")
}

/// Variável que diz com que features esta máquina compila.
pub fn env_features() -> String {
    crate::brand::env_var("BUILD_FEATURES")
}

/// Este nó aceita pedidos de atualização?
pub fn enabled() -> bool {
    enabled_from(crate::brand::env("DEV_UPDATE").ok().as_deref())
}

fn enabled_from(v: Option<&str>) -> bool {
    v == Some("1")
}

use crate::gateway::GatewayState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::json;

/// A recusa do portão, ou `None` quando ele está aberto.
fn gate_rejection() -> Option<(StatusCode, String)> {
    if enabled() {
        return None;
    }
    Some((
        StatusCode::FORBIDDEN,
        format!(
            "atualização remota desligada neste nó. Suba o processo com {}=1 para habilitar.",
            env_gate()
        ),
    ))
}

fn refuse(r: (StatusCode, String)) -> Response {
    (r.0, Json(json!({ "error": r.1 }))).into_response()
}

/// Folga entre a resposta HTTP sair e o processo morrer.
///
/// O corpo é um JSON minúsculo; numa LAN, escrevê-lo na conexão leva
/// microssegundos. 300ms é folga generosa para isso mesmo passando por um
/// proxy reverso local, sem ser tão longo que o operador sinta demora no
/// reinício. Os dois lados do erro são assimétricos: passar deste valor só
/// custa ao operador esperar um pouco mais; ficar aquém dele corta a conexão
/// do cliente antes do byte chegar, e o pedido parece ter falhado quando na
/// verdade funcionou.
const RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

pub fn router() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/dev/restart", post(restart_endpoint))
        .route("/api/ui/dev/update", post(update_endpoint))
}

/// Argumentos do cargo, com as features desta máquina.
fn build_args(features: Option<&str>) -> Vec<String> {
    let mut v = vec!["build".to_string(), "--release".to_string()];
    match features.map(str::trim) {
        Some(f) if !f.is_empty() => {
            v.push("--features".into());
            v.push(f.to_string());
        }
        _ => {}
    }
    v
}

/// URLs de gateway dos peers que rodam na mesma máquina que este nó.
///
/// A entrada é `(hostname, addr, gateway_port)` de cada peer vivo. Peer sem
/// porta de gateway fica de fora: não há como pedir nada a ele. Hostname
/// repetido não basta — duas VMs chamadas `ubuntu` não são a mesma máquina —
/// então `same_host` também precisa aprovar o endereço; quem decide a lógica
/// de rede fica de fora desta função para ela continuar testável sem sockets.
///
/// A URL é montada com `network::ip_host`, a mesma função por trás de
/// `PeerInfo::gateway_url()` — não uma reimplementação com `rsplit_once`, que
/// quebra em IPv6 sem colchetes.
///
/// Devolve `(node_id, url)`: o irmão está na mesma máquina, mas o POST sai pelo
/// endereço da interface e não pelo loopback, então num cluster em TLS ele
/// precisa do certificado daquele nó fixado como qualquer outra chamada entre
/// peers. Ver `serve::plain_allowed`.
fn sibling_urls(
    my_hostname: &str,
    peers: &[(&str, &str, Option<u16>, &str)],
    same_host: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    peers
        .iter()
        .filter(|(h, _, _, _)| *h == my_hostname)
        .filter(|(_, addr, _, _)| {
            // `0.0.0.0`/`::` é o sentinela "endereço ainda não reescrito"
            // (`Network::announce_addr`) de um peer chegado por libp2p; sem
            // este descarte explícito o POST iria para este próprio nó, que
            // se reiniciaria antes de o job ser marcado como `done`.
            let host = crate::network::ip_host(addr);
            host != "0.0.0.0" && host != "::" && host != "[::]" && same_host(addr)
        })
        .filter_map(|(_, addr, port, node_id)| {
            let port = (*port)?;
            Some((
                node_id.to_string(),
                crate::peer_client::peer_base_url(
                    &crate::network::ip_host(addr),
                    port,
                    crate::peer_client::tls_enabled(),
                ),
            ))
        })
        .collect()
}

/// Este endereço pertence à máquina onde este processo está rodando?
///
/// Conecta um socket UDP (sem tráfego, só fixa a rota) ao IP do peer e
/// compara o endereço local resultante com o do peer: se o peer é este
/// próprio host, o roteamento do SO devolve o mesmo IP. Endereço que não
/// resolve para um IP válido nunca é a mesma máquina.
fn same_machine(addr: &str) -> bool {
    let host = crate::network::ip_host(addr);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => crate::transfer::local_ip_towards(ip) == host,
        Err(_) => false,
    }
}

async fn git(root: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("não deu para rodar git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} falhou:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Corpo opcional de `POST /api/ui/dev/update`.
///
/// `Option<Json<T>>` em vez de `Json<T>`: um corpo vazio (o botão da tela hoje
/// manda `{}`, mas um `curl` sem `-d` nenhum manda corpo zero) faria o
/// extractor `Json<T>` falhar antes mesmo de o handler rodar, e a mensagem de
/// erro do axum não é a que este endpoint quer mostrar. Com `Option`, corpo
/// ausente e corpo `{}` chegam ambos como `node_id: None` — executa aqui.
#[derive(Debug, Default, serde::Deserialize)]
struct UpdateRequest {
    node_id: Option<String>,
}

/// Este pedido é para repassar a outro peer, ou para executar aqui?
///
/// Função pura para ficar testável sem subir rede: sem `node_id`, ou com o
/// `node_id` deste próprio nó, executa aqui; qualquer outro id repassa.
fn should_forward(requested_node_id: Option<&str>, my_node_id: &str) -> bool {
    matches!(requested_node_id, Some(id) if id != my_node_id)
}

async fn update_endpoint(
    _auth: crate::auth::ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<UpdateRequest>>,
) -> Response {
    let requested_node_id = body.and_then(|Json(b)| b.node_id);

    // O portão protege a máquina que vai rodar o build, e repassar não roda
    // nada aqui. Checá-lo antes da decisão de repasse impedia o painel de um nó
    // comum de atualizar um peer que tem o portão aberto — e a tela desenha o
    // botão a partir do `dev_update` **do alvo**, então o botão aparecia e este
    // nó respondia 403. Quem repassa não precisa da permissão; quem executa,
    // sim, e é o destino que a verifica.
    if let Some(target) = requested_node_id.filter(|id| should_forward(Some(id), &state.network.node_id))
    {
        let peers = state.network.alive_peers().await;
        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == target)
            .and_then(|p| p.gateway_url())
        else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": format!(
                        "peer {target} não está anunciando gateway ou não é mais visto na rede"
                    )
                })),
            )
                .into_response();
        };
        let url = format!("{base}/api/ui/dev/update");
        let client = match state.peer_http.for_peer(&target) {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("sem cliente para o peer {target}: {e}") })),
                )
                    .into_response()
            }
        };
        let request = match state
            .authorize_control_request(
                client.post(&url).timeout(crate::webui::PEER_TIMEOUT),
                "POST",
                "/api/ui/dev/update",
                &[],
            )
            .await
        {
            Ok(request) => request,
            Err(error) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": error.to_string() })),
                )
                    .into_response()
            }
        };
        return match request.send().await {
            Ok(resp) => {
                let status = resp.status();
                let peer_body = resp.text().await.unwrap_or_else(|e| {
                    format!("{{\"error\":\"resposta do peer {target} sem corpo legível: {e}\"}}")
                });
                (status, peer_body).into_response()
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": format!("não deu para repassar a atualização para {target}: {e}")
                })),
            )
                .into_response(),
        };
    }

    // Daqui para baixo é execução nesta máquina, e aí sim o portão manda.
    if let Some(r) = gate_rejection() {
        return refuse(r);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let job = crate::webui::new_job(
        &id,
        "update",
        "Atualizar esta máquina",
        "conferindo o repositório",
    );
    crate::webui::jobs().write().await.insert(id.clone(), job);

    let job_id = id.clone();
    tokio::spawn(async move {
        let root = crate::config::project_root();

        async fn fail(job_id: &str, msg: String) {
            let mut jobs = crate::webui::jobs().write().await;
            if let Some(j) = jobs.get_mut(job_id) {
                j.state = "failed";
                j.message = msg;
                j.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }

        // ── Checagens ────────────────────────────────────────────────────────
        let branch = match git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
            Ok(b) => b,
            Err(e) => return fail(&job_id, e).await,
        };
        let status = match git(&root, &["status", "--porcelain"]).await {
            Ok(s) => s,
            Err(e) => return fail(&job_id, e).await,
        };
        if let Some(msg) = preflight(&branch, &status).message() {
            return fail(&job_id, msg).await;
        }

        // ── Pull ─────────────────────────────────────────────────────────────
        step(&job_id, "puxando o código").await;
        if let Err(e) = git(&root, &["pull", "--ff-only", "origin", "main"]).await {
            return fail(&job_id, e).await;
        }

        // ── Build ────────────────────────────────────────────────────────────
        step(&job_id, "compilando").await;
        let features = std::env::var(env_features()).ok();
        let args = build_args(features.as_deref());
        let out =
            tokio::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
                .args(&args)
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .output()
                .await;
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let tail: String = err
                    .lines()
                    .rev()
                    .take(30)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                return fail(
                    &job_id,
                    format!("o build falhou. Ninguém reiniciou.\n{tail}"),
                )
                .await;
            }
            Err(e) => return fail(&job_id, format!("não deu para rodar cargo: {e}")).await,
        }

        // ── Reinícios ────────────────────────────────────────────────────────
        step(&job_id, "reiniciando os nós desta máquina").await;
        let me = crate::sysmon::sample().await.hostname;
        let peers = state.network.alive_peers().await;
        let tuples: Vec<(&str, &str, Option<u16>, &str)> = peers
            .iter()
            .map(|p| {
                (
                    p.hostname.as_str(),
                    p.addr.as_str(),
                    p.gateway_port,
                    p.node_id.as_str(),
                )
            })
            .collect();
        let targets = sibling_urls(&me, &tuples, same_machine);

        // Máquina de outra pessoa: um irmão que não responde não pode
        // desaparecer em silêncio, ou o operador fica com um nó rodando
        // binário velho e nenhuma pista. Mas nenhum irmão respondendo não
        // impede este nó de reiniciar — cada máquina se anota por conta
        // própria.
        let mut restarted = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for (node_id, url) in &targets {
            let Ok(client) = state.peer_http.for_peer(node_id) else {
                failed.push(url.clone());
                continue;
            };
            let Ok(request) = state
                .authorize_control_request(
                    client
                        .post(format!("{url}/api/ui/dev/restart"))
                        .timeout(std::time::Duration::from_secs(10)),
                    "POST",
                    "/api/ui/dev/restart",
                    &[],
                )
                .await
            else {
                failed.push(url.clone());
                continue;
            };
            match request.send().await {
                Ok(resp) if resp.status().is_success() => restarted += 1,
                Ok(resp) => {
                    tracing::warn!("irmão {url} recusou o reinício: {}", resp.status());
                    failed.push(url.clone());
                }
                Err(e) => {
                    tracing::warn!("não deu para reiniciar o irmão {url}: {e}");
                    failed.push(url.clone());
                }
            }
        }

        let siblings_msg = if targets.is_empty() {
            String::new()
        } else if failed.is_empty() {
            format!(" {restarted} irmão(s) desta máquina também reiniciaram.")
        } else {
            format!(
                " {restarted} irmão(s) desta máquina reiniciaram, {} falharam: {}.",
                failed.len(),
                failed.join(", ")
            )
        };

        {
            let mut jobs = crate::webui::jobs().write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                j.state = "done";
                j.step = "pronto".into();
                j.message =
                    format!("código atualizado; esta máquina está reiniciando.{siblings_msg}");
                j.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }

        // Este nó por último, para o job chegar a ser marcado como pronto.
        //
        // Nada a anotar aqui: quem mantém a anotação é o runtime, e ela já
        // reflete a verdade deste nó (`Pending` desde o apply, `Serving` desde
        // que o `/health` respondeu). Reescrevê-la neste ponto só poderia
        // mentir - carimbar de servindo uma fatia que nunca carregou é o laço
        // infinito de 2026-08-15 entrando por outra porta.
        tokio::time::sleep(RESTART_DELAY).await;
        let e = crate::respawn::respawn_self();
        tracing::error!("respawn falhou, o nó segue no ar: {e}");
    });

    Json(json!({ "job_id": id })).into_response()
}

async fn step(job_id: &str, s: &str) {
    let mut jobs = crate::webui::jobs().write().await;
    if let Some(j) = jobs.get_mut(job_id) {
        j.step = s.to_string();
    }
}

async fn restart_endpoint(
    _auth: crate::auth::ConnectionAuth,
    State(_state): State<GatewayState>,
) -> Response {
    if let Some(r) = gate_rejection() {
        return refuse(r);
    }

    // A fatia não é anotada aqui: o runtime já a mantém anotada, com o estado
    // que ela de fato alcançou. Um nó servindo volta servindo; um nó cuja
    // fatia nunca carregou volta vazio, dizendo por quê.

    tokio::spawn(async {
        tokio::time::sleep(RESTART_DELAY).await;
        let e = crate::respawn::respawn_self();
        tracing::error!("respawn falhou, o nó segue no ar: {e}");
    });

    Json(json!({ "restarting": true })).into_response()
}

/// O que impede esta máquina de ser atualizada agora.
#[derive(Debug, PartialEq)]
pub enum Preflight {
    Ok,
    WrongBranch(String),
    DetachedOrNoBranch,
    DirtyTree(Vec<String>),
}

/// Decide a partir da saída de `git rev-parse --abbrev-ref HEAD` e de
/// `git status --porcelain`.
pub fn preflight(branch: &str, status_porcelain: &str) -> Preflight {
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return Preflight::DetachedOrNoBranch;
    }
    if branch != "main" {
        return Preflight::WrongBranch(branch.to_string());
    }

    // `??` é arquivo não rastreado: o pull não o toca.
    let dirty: Vec<String> = status_porcelain
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("??"))
        .filter_map(|l| l.get(3..).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if !dirty.is_empty() {
        return Preflight::DirtyTree(dirty);
    }
    Preflight::Ok
}

impl Preflight {
    /// A mensagem que vai para a tela, ou `None` quando dá para seguir.
    pub fn message(&self) -> Option<String> {
        match self {
            Preflight::Ok => None,
            Preflight::DetachedOrNoBranch => Some(
                "o repositório desta máquina não está num branch (HEAD destacado). \
                 A atualização só roda no main."
                    .into(),
            ),
            Preflight::WrongBranch(b) => Some(format!(
                "o repositório desta máquina está no branch '{b}', não no main. \
                 Alguém pode estar trabalhando aqui — a atualização não troca de branch."
            )),
            Preflight::DirtyTree(files) => Some(format!(
                "há alterações não commitadas nesta máquina: {}. \
                 A atualização não descarta trabalho, então ela para aqui.",
                files.join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O portão é por processo e não é ligável pela rede — diferente da config,
    /// que a própria interface reescreve. Só "1" liga: um "true" ou "yes" que
    /// alguém escreveu achando que ligava não deve abrir execução remota.
    #[test]
    fn only_the_value_1_opens_the_gate() {
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some("true")));
        assert!(!enabled_from(Some("")));
        assert!(!enabled_from(None));
    }

    /// Com o portão fechado a rota não existe para efeitos práticos: responde
    /// 403 sem tocar em nada.
    #[test]
    fn a_closed_gate_refuses_with_403() {
        // Num ambiente de teste a variável não está ligada, então há recusa.
        let r = gate_rejection().expect("sem a variável, tem que recusar");
        assert_eq!(r.0, axum::http::StatusCode::FORBIDDEN);
        assert!(
            r.1.contains(&env_gate()),
            "a mensagem tem que dizer qual variável liga: {}",
            r.1
        );
    }

    /// Numa máquina onde alguém está codificando, o botão falha e explica, em
    /// vez de arrumar a casa alheia. Um `git pull --ff-only origin main` num
    /// branch de trabalho o avançaria para o main em silêncio.
    #[test]
    fn refuses_a_branch_other_than_main() {
        let p = preflight("fm/minha-feature", "");
        assert!(matches!(p, Preflight::WrongBranch(ref b) if b == "fm/minha-feature"));
        assert!(p.message().unwrap().contains("fm/minha-feature"));
    }

    #[test]
    fn refuses_a_detached_head() {
        // `git rev-parse --abbrev-ref HEAD` devolve literalmente "HEAD".
        assert!(matches!(
            preflight("HEAD", ""),
            Preflight::DetachedOrNoBranch
        ));
        assert!(matches!(
            preflight("", ""),
            Preflight::DetachedOrNoBranch
        ));
    }

    /// Árvore suja para antes de tocar em qualquer coisa, e diz quais arquivos.
    /// Um build de árvore suja não corresponde a commit nenhum, e o carimbo
    /// build_commit viraria mentira.
    #[test]
    fn refuses_a_dirty_tree_naming_the_files() {
        let status = " M src/llm.rs\n M src/webui.rs\n";
        let p = preflight("main", status);
        match &p {
            Preflight::DirtyTree(files) => {
                assert_eq!(
                    files,
                    &vec!["src/llm.rs".to_string(), "src/webui.rs".to_string()]
                );
            }
            other => panic!("esperava árvore suja, veio {other:?}"),
        }
        assert!(p.message().unwrap().contains("src/llm.rs"));
    }

    /// Arquivo não rastreado não conta: o pull não o toca, e se colidir com um
    /// arquivo do remoto o próprio git aborta sem sobrescrever.
    #[test]
    fn an_untracked_file_does_not_block() {
        assert!(matches!(
            preflight("main", "?? gateway.rs\n"),
            Preflight::Ok
        ));
        assert!(preflight("main", "").message().is_none());
    }

    /// O comando de build precisa carregar as features da máquina. Sem isto a
    /// ksia perderia CUDA no primeiro clique — o mesmo engano já cometido à mão.
    #[test]
    fn the_build_carries_the_machine_features() {
        assert_eq!(build_args(None), vec!["build", "--release"]);
        assert_eq!(build_args(Some("")), vec!["build", "--release"]);
        assert_eq!(
            build_args(Some("cuda")),
            vec!["build", "--release", "--features", "cuda"]
        );
        assert_eq!(
            build_args(Some("metal")),
            vec!["build", "--release", "--features", "metal"]
        );
    }

    /// Sem `node_id`, com o `node_id` deste nó, e com o de outro peer: as três
    /// respostas que decidem entre executar aqui e repassar.
    #[test]
    fn should_forward_only_with_another_peer_node_id() {
        assert!(!should_forward(None, "meu-id"), "sem node_id, executa aqui");
        assert!(
            !should_forward(Some("meu-id"), "meu-id"),
            "node_id igual ao meu, executa aqui"
        );
        assert!(
            should_forward(Some("outro-id"), "meu-id"),
            "node_id de outro peer, repassa"
        );
    }

    /// Os irmãos são os peers da mesma máquina, e este nó não está na lista de
    /// peers — ele reinicia por último, por conta própria.
    #[test]
    fn siblings_are_the_peers_with_the_same_hostname() {
        let targets = sibling_urls(
            "ksia",
            &[
                ("ksia", "[2804:d55::1]:42777", Some(8090), "id-a"),
                ("ksia", "[2804:d55::1]:42778", Some(8094), "id-b"),
                ("tiago-pc", "[2804:f30::9]:42777", Some(8080), "id-c"),
                ("ksia", "[2804:d55::1]:42779", None, "id-d"),
            ],
            |_| true,
        );
        assert_eq!(
            targets,
            vec![
                ("id-a".to_string(), "http://[2804:d55::1]:8090".to_string()),
                ("id-b".to_string(), "http://[2804:d55::1]:8094".to_string()),
            ],
            "outra máquina fica de fora, e peer sem gateway_port não dá para chamar"
        );
    }

    /// Hostname repetido não basta: duas VMs chamadas `ksia` não são a mesma
    /// máquina. `same_host` é quem decide isso de verdade; aqui simulamos a
    /// recusa para um dos dois endereços.
    #[test]
    fn the_same_hostname_on_a_different_machine_is_left_out() {
        let targets = sibling_urls(
            "ksia",
            &[
                ("ksia", "[2804:d55::1]:42777", Some(8090), "id-a"),
                ("ksia", "203.0.113.9:42777", Some(8091), "id-b"),
            ],
            |addr| addr.starts_with("[2804:d55::1]"),
        );
        assert_eq!(
            targets,
            vec![("id-a".to_string(), "http://[2804:d55::1]:8090".to_string())],
            "a VM em 203.0.113.9 tem o mesmo hostname mas não é esta máquina"
        );
    }

    /// `0.0.0.0`/`::` é o sentinela de um peer chegado por libp2p cujo
    /// endereço ainda não foi reescrito com a origem real. Batendo como
    /// irmão, o POST iria para este próprio nó — que se reiniciaria antes do
    /// job ser marcado como `done`. Por isso o descarte é explícito e não
    /// depende de `same_host` aprovar ou não.
    #[test]
    fn address_0_0_0_0_is_left_out() {
        let targets = sibling_urls(
            "ksia",
            &[
                ("ksia", "0.0.0.0:42777", Some(8090), "id-a"),
                ("ksia", "[::]:42778", Some(8091), "id-b"),
            ],
            |_| true,
        );
        assert!(
            targets.is_empty(),
            "sentinela não reescrito não pode virar alvo: {targets:?}"
        );
    }
}
