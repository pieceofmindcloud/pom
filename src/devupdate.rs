//! Atualizar e reiniciar um nó pela interface, em desenvolvimento.
//!
//! Isto é execução remota de código: quem tem a chave de conexão passa a poder
//! rodar um build e trocar o binário da máquina. Por isso o portão é uma
//! variável de ambiente, ligada por processo por quem é dono da máquina, e não
//! um campo de config — a config é reescrita pela própria interface, o que
//! deixaria o portão ligável pela rede.

/// Nome da variável que abre o recurso.
pub const ENV_GATE: &str = "RDMA_P2P_DEV_UPDATE";

/// Variável que diz com que features esta máquina compila.
pub const ENV_FEATURES: &str = "RDMA_P2P_BUILD_FEATURES";

/// Este nó aceita pedidos de atualização?
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENV_GATE).ok().as_deref())
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
            "atualização remota desligada neste nó. Suba o processo com \
             {ENV_GATE}=1 para habilitar."
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
/// porta de gateway fica de fora: não há como pedir nada a ele.
fn sibling_urls(my_hostname: &str, peers: &[(&str, &str, Option<u16>)]) -> Vec<String> {
    peers
        .iter()
        .filter(|(h, _, _)| *h == my_hostname)
        .filter_map(|(_, addr, port)| {
            let port = (*port)?;
            let host = addr.rsplit_once(':').map(|(h, _)| h)?;
            Some(format!("http://{host}:{port}"))
        })
        .collect()
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

async fn update_endpoint(
    _auth: crate::auth::ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    if let Some(r) = gate_rejection() {
        return refuse(r);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let job = crate::webui::new_job(&id, "update", "Atualizar esta máquina", "conferindo o repositório");
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
        if let Some(msg) = preflight(&branch, &status).mensagem() {
            return fail(&job_id, msg).await;
        }

        // ── Pull ─────────────────────────────────────────────────────────────
        step(&job_id, "puxando o código").await;
        if let Err(e) = git(&root, &["pull", "--ff-only", "origin", "main"]).await {
            return fail(&job_id, e).await;
        }

        // ── Build ────────────────────────────────────────────────────────────
        step(&job_id, "compilando").await;
        let features = std::env::var(ENV_FEATURES).ok();
        let args = build_args(features.as_deref());
        let out = tokio::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
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
                return fail(&job_id, format!("o build falhou. Ninguém reiniciou.\n{tail}")).await;
            }
            Err(e) => return fail(&job_id, format!("não deu para rodar cargo: {e}")).await,
        }

        // ── Reinícios ────────────────────────────────────────────────────────
        step(&job_id, "reiniciando os nós desta máquina").await;
        let me = crate::sysmon::sample().await.hostname;
        let peers = state.network.alive_peers().await;
        let tuples: Vec<(&str, &str, Option<u16>)> = peers
            .iter()
            .map(|p| (p.hostname.as_str(), p.addr.as_str(), p.gateway_port))
            .collect();
        let key = { state.auth.read().await.connection_key.clone() };
        for url in sibling_urls(&me, &tuples) {
            let _ = state
                .http
                .post(format!("{url}/api/ui/dev/restart"))
                .bearer_auth(&key)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
        }

        {
            let mut jobs = crate::webui::jobs().write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                j.state = "done";
                j.step = "pronto".into();
                j.message = "código atualizado; esta máquina está reiniciando".into();
                j.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }

        // Este nó por último, para o job chegar a ser marcado como pronto.
        if let Some(cfg) = state.runtime.config().await {
            let _ = crate::lastslice::save(&cfg);
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
    State(state): State<GatewayState>,
) -> Response {
    if let Some(r) = gate_rejection() {
        return refuse(r);
    }

    // Anota a fatia antes de sair, para voltar servindo o mesmo.
    if let Some(cfg) = state.runtime.config().await {
        if let Err(e) = crate::lastslice::save(&cfg) {
            tracing::warn!("não deu para anotar a fatia antes do reinício: {e}");
        }
    }

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
    BranchErrado(String),
    DesatachadoOuSemBranch,
    ArvoreSuja(Vec<String>),
}

/// Decide a partir da saída de `git rev-parse --abbrev-ref HEAD` e de
/// `git status --porcelain`.
pub fn preflight(branch: &str, status_porcelain: &str) -> Preflight {
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return Preflight::DesatachadoOuSemBranch;
    }
    if branch != "main" {
        return Preflight::BranchErrado(branch.to_string());
    }

    // `??` é arquivo não rastreado: o pull não o toca.
    let sujos: Vec<String> = status_porcelain
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("??"))
        .filter_map(|l| l.get(3..).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if !sujos.is_empty() {
        return Preflight::ArvoreSuja(sujos);
    }
    Preflight::Ok
}

impl Preflight {
    /// A mensagem que vai para a tela, ou `None` quando dá para seguir.
    pub fn mensagem(&self) -> Option<String> {
        match self {
            Preflight::Ok => None,
            Preflight::DesatachadoOuSemBranch => Some(
                "o repositório desta máquina não está num branch (HEAD destacado). \
                 A atualização só roda no main."
                    .into(),
            ),
            Preflight::BranchErrado(b) => Some(format!(
                "o repositório desta máquina está no branch '{b}', não no main. \
                 Alguém pode estar trabalhando aqui — a atualização não troca de branch."
            )),
            Preflight::ArvoreSuja(files) => Some(format!(
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
    fn so_o_valor_1_liga_o_portao() {
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some("true")));
        assert!(!enabled_from(Some("")));
        assert!(!enabled_from(None));
    }

    /// Com o portão fechado a rota não existe para efeitos práticos: responde
    /// 403 sem tocar em nada.
    #[test]
    fn portao_fechado_recusa_com_403() {
        // Num ambiente de teste a variável não está ligada, então há recusa.
        let r = gate_rejection().expect("sem a variável, tem que recusar");
        assert_eq!(r.0, axum::http::StatusCode::FORBIDDEN);
        assert!(
            r.1.contains(ENV_GATE),
            "a mensagem tem que dizer qual variável liga: {}",
            r.1
        );
    }

    /// Numa máquina onde alguém está codificando, o botão falha e explica, em
    /// vez de arrumar a casa alheia. Um `git pull --ff-only origin main` num
    /// branch de trabalho o avançaria para o main em silêncio.
    #[test]
    fn recusa_branch_diferente_de_main() {
        let p = preflight("fm/minha-feature", "");
        assert!(matches!(p, Preflight::BranchErrado(ref b) if b == "fm/minha-feature"));
        assert!(p.mensagem().unwrap().contains("fm/minha-feature"));
    }

    #[test]
    fn recusa_head_destacado() {
        // `git rev-parse --abbrev-ref HEAD` devolve literalmente "HEAD".
        assert!(matches!(preflight("HEAD", ""), Preflight::DesatachadoOuSemBranch));
        assert!(matches!(preflight("", ""), Preflight::DesatachadoOuSemBranch));
    }

    /// Árvore suja para antes de tocar em qualquer coisa, e diz quais arquivos.
    /// Um build de árvore suja não corresponde a commit nenhum, e o carimbo
    /// build_commit viraria mentira.
    #[test]
    fn recusa_arvore_suja_nomeando_os_arquivos() {
        let status = " M src/llm.rs\n M src/webui.rs\n";
        let p = preflight("main", status);
        match &p {
            Preflight::ArvoreSuja(files) => {
                assert_eq!(files, &vec!["src/llm.rs".to_string(), "src/webui.rs".to_string()]);
            }
            outro => panic!("esperava árvore suja, veio {outro:?}"),
        }
        assert!(p.mensagem().unwrap().contains("src/llm.rs"));
    }

    /// Arquivo não rastreado não conta: o pull não o toca, e se colidir com um
    /// arquivo do remoto o próprio git aborta sem sobrescrever.
    #[test]
    fn arquivo_nao_rastreado_nao_impede() {
        assert!(matches!(preflight("main", "?? gateway.rs\n"), Preflight::Ok));
        assert!(preflight("main", "").mensagem().is_none());
    }

    /// O comando de build precisa carregar as features da máquina. Sem isto a
    /// ksia perderia CUDA no primeiro clique — o mesmo engano já cometido à mão.
    #[test]
    fn o_build_leva_as_features_da_maquina() {
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

    /// Os irmãos são os peers da mesma máquina, e este nó não está na lista de
    /// peers — ele reinicia por último, por conta própria.
    #[test]
    fn irmaos_sao_os_peers_do_mesmo_hostname() {
        let alvos = sibling_urls("ksia", &[
            ("ksia", "[2804:d55::1]:42777", Some(8090)),
            ("ksia", "[2804:d55::1]:42778", Some(8094)),
            ("tiago-pc", "[2804:f30::9]:42777", Some(8080)),
            ("ksia", "[2804:d55::1]:42779", None),
        ]);
        assert_eq!(
            alvos,
            vec![
                "http://[2804:d55::1]:8090".to_string(),
                "http://[2804:d55::1]:8094".to_string(),
            ],
            "outra máquina fica de fora, e peer sem gateway_port não dá para chamar"
        );
    }
}
