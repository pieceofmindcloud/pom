# Atualização remota de um nó pela interface — plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Um botão no card de cada peer que faz aquela máquina puxar o código, recompilar e reiniciar os seus nós, cada um preservando o próprio ambiente e voltando a servir a mesma fatia.

**Architecture:** Um módulo novo, `src/devupdate.rs`, concentra o portão, as checagens do git, o build e o disparo dos reinícios. Duas rotas: `/api/ui/dev/update` (atualiza a máquina) e `/api/ui/dev/restart` (o nó reinicia a si mesmo — é o único jeito de preservar `RDMA_P2P_CONFIG` e `RDMA_P2P_NODE_KEY` de cada processo). O progresso reusa o sistema de jobs já existente em `webui.rs`.

**Tech Stack:** Rust, axum, tokio, `std::process::Command`; React + TypeScript na interface.

**Spec:** `docs/superpowers/specs/2026-08-10-atualizacao-remota-dev-design.md`

## Global Constraints

- Portão: variável de ambiente `RDMA_P2P_DEV_UPDATE=1`. Qualquer outro valor, ou ausência, desliga. Vale em debug e em release.
- Features do build: lidas de `RDMA_P2P_BUILD_FEATURES` (ex.: `cuda`, `metal`). Vazio ou ausente = build sem features.
- Comandos permitidos, e só estes: `git status`, `git rev-parse`, `git pull --ff-only origin main`, `cargo build --release`. Proibidos: `reset`, `checkout`, `clean`, `stash`, troca de branch.
- Branch: só `main`. HEAD destacado recusa.
- Autenticação das rotas novas: `ConnectionAuth` (chave de conexão como Bearer), igual a `apply`/`launch`.
- Mensagens de erro em português, como o resto da interface.
- Nada de `unwrap()` em caminho de requisição.
- Toda tarefa termina com `cargo test --release` verde e um commit.

---

### Task 1: Preservar o `argv` no respawn

O respawn de hoje (`webui::save_setup`) repassa só `--gateway-port`. Um nó subido com `--advertise-addr` ou `--refresh 3600` volta diferente do que era. Todo restart deste plano depende de consertar isso primeiro.

**Files:**
- Create: `src/respawn.rs`
- Modify: `src/main.rs` (declarar o módulo; capturar o argv no início de `main`)
- Modify: `src/webui.rs` (o bloco "Trigger restart" em `save_setup` passa a usar o helper)

**Interfaces:**
- Produces: `respawn::remember_argv(args: Vec<String>)`, `respawn::original_argv() -> Vec<String>`, `respawn::respawn_self() -> std::io::Error` (só retorna se o spawn falhar; em caso de sucesso o processo sai).

- [ ] **Step 1: Escrever o teste que falha**

Em `src/respawn.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// O respawn precisa devolver o processo do jeito que ele subiu. O caminho
    /// antigo repassava só --gateway-port, então um nó com --advertise-addr
    /// voltava sem ele e os peers passavam a guardar um endereço temporário.
    #[test]
    fn argv_lembrado_volta_igual_sem_o_programa() {
        remember_argv(vec![
            "./target/release/rdma-p2p".into(),
            "--refresh".into(),
            "3600".into(),
            "--advertise-addr".into(),
            "[2804:d55::1]:42777".into(),
        ]);
        assert_eq!(
            original_argv(),
            vec![
                "--refresh".to_string(),
                "3600".to_string(),
                "--advertise-addr".to_string(),
                "[2804:d55::1]:42777".to_string(),
            ],
            "o argv0 é o executável e volta por current_exe, não pela lista"
        );
    }

    /// Um nó subido sem argumento nenhum volta sem argumento nenhum — e a
    /// lista vazia não pode virar um argumento vazio na linha de comando.
    #[test]
    fn argv_so_com_o_programa_nao_gera_argumento() {
        assert!(argv_rest(vec!["./target/release/rdma-p2p".to_string()]).is_empty());
        assert_eq!(
            argv_rest(vec!["rdma-p2p".into(), "--no-discovery".into()]),
            vec!["--no-discovery".to_string()]
        );
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test --release respawn:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function remember_argv`.

- [ ] **Step 3: Implementar**

`src/respawn.rs` inteiro:

```rust
//! Reiniciar este processo como ele subiu.
//!
//! O respawn ingênuo — `Command::new(current_exe())` e nada mais — perde os
//! argumentos de linha de comando. Um nó subido com `--advertise-addr` volta
//! sem ele, e os peers passam a guardar um endereço temporário que expira.
//!
//! O ambiente não precisa de cuidado: `Command` o herda por padrão, e é isso
//! que preserva `RDMA_P2P_CONFIG` e `RDMA_P2P_NODE_KEY` de cada nó de uma
//! máquina com vários.

use std::sync::OnceLock;

static ARGV: OnceLock<Vec<String>> = OnceLock::new();

/// Guarda o `argv` do arranque. Chamada uma vez, no início de `main`.
pub fn remember_argv(args: Vec<String>) {
    let _ = ARGV.set(argv_rest(args));
}

/// Os argumentos sem o `argv[0]`. Separada de `remember_argv` porque o
/// `OnceLock` só aceita um valor por processo e um teste não pode desfazê-lo.
fn argv_rest(args: Vec<String>) -> Vec<String> {
    args.into_iter().skip(1).collect()
}

/// Os argumentos do arranque, sem o `argv[0]`.
pub fn original_argv() -> Vec<String> {
    ARGV.get().cloned().unwrap_or_default()
}

/// Sobe uma cópia deste processo e sai.
///
/// Só retorna quando o spawn falha — aí o chamador segue vivo e reporta, em vez
/// de derrubar um nó que não conseguiu se substituir.
pub fn respawn_self() -> std::io::Error {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::process::Command::new(exe)
        .args(original_argv())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}
```

- [ ] **Step 4: Ligar no `main` e no `save_setup`**

Em `src/main.rs`, declarar o módulo junto dos outros (`mod respawn;`) e, como **primeira** linha de `main()`:

```rust
    respawn::remember_argv(std::env::args().collect());
```

Em `src/webui.rs`, o bloco `// Trigger restart:` de `save_setup` passa a ser:

```rust
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                let e = crate::respawn::respawn_self();
                tracing::error!("respawn falhou, o nó segue no ar: {e}");
            });
```

Remover a variável `gateway_port` que servia só ao respawn antigo, se ela ficar sem uso.

- [ ] **Step 5: Rodar os testes**

Run: `cargo test --release 2>&1 | grep -E "test result|error"`
Expected: PASS, sem erro de compilação.

- [ ] **Step 6: Commit**

```bash
git add src/respawn.rs src/main.rs src/webui.rs
git commit -m "fix(respawn): o reinicio perdia os argumentos de linha de comando

O respawn do save_setup repassava so --gateway-port, entao um no subido
com --advertise-addr voltava sem ele e os peers passavam a guardar um
endereco temporario que expira. O argv do arranque agora e lembrado e
reexecutado igual. O ambiente ja era herdado pelo Command, que e o que
preserva RDMA_P2P_CONFIG e RDMA_P2P_NODE_KEY de cada no."
```

---

### Task 2: O portão

**Files:**
- Create: `src/devupdate.rs`
- Modify: `src/main.rs` (`mod devupdate;`)
- Modify: `src/webui.rs` (campo `dev_update` em `NodeInfo` e em `node_info`)

**Interfaces:**
- Consumes: nada.
- Produces: `devupdate::enabled() -> bool`.

- [ ] **Step 1: Escrever o teste que falha**

Em `src/devupdate.rs`:

```rust
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
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test --release devupdate:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function enabled_from`.

- [ ] **Step 3: Implementar**

Início de `src/devupdate.rs`:

```rust
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
```

E em `src/main.rs`, junto dos outros: `mod devupdate;`.

- [ ] **Step 4: Expor no `/api/ui/node`**

Em `src/webui.rs`, no struct `NodeInfo`, logo depois de `build_commit`:

```rust
    /// Este nó aceita atualizar-se pela interface. A tela só desenha o botão
    /// quando isto é verdade, para nunca oferecer o que vai dar 403.
    pub dev_update: bool,
```

E no corpo de `node_info`, junto de `build_commit`:

```rust
        dev_update: crate::devupdate::enabled(),
```

- [ ] **Step 5: Rodar os testes**

Run: `cargo test --release 2>&1 | grep -E "test result|error"`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/devupdate.rs src/main.rs src/webui.rs
git commit -m "feat(devupdate): portao por variavel de ambiente

RDMA_P2P_DEV_UPDATE=1 abre a atualizacao remota, por processo. So o valor
1 liga: um 'true' escrito por engano nao abre execucao remota de codigo.
/api/ui/node passa a dizer dev_update, para a tela nunca oferecer o que
vai dar 403."
```

---

### Task 3: Guardar e restaurar a fatia servida

Reiniciar esvazia o nó — a atribuição de modelo vive só em memória. Sem isto, o botão sempre exigiria reimplantar à mão.

**Files:**
- Create: `src/lastslice.rs`
- Modify: `src/main.rs` (`mod lastslice;` e a restauração no arranque)

**Interfaces:**
- Consumes: `runtime::NodeConfig` (já existe).
- Produces: `lastslice::path() -> PathBuf`, `lastslice::save(&NodeConfig) -> Result<()>`, `lastslice::take() -> Option<NodeConfig>`.

- [ ] **Step 1: Escrever o teste que falha**

Em `src/lastslice.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Dois nós na mesma máquina rodam com RDMA_P2P_CONFIG diferentes. Se o
    /// arquivo de fatia tivesse nome fixo, um sobrescreveria o do outro e a
    /// máquina voltaria com as duas fatias trocadas.
    #[test]
    fn o_caminho_deriva_da_config_de_cada_no() {
        assert_eq!(
            path_for(std::path::Path::new("/repo/rdma-p2p.json")),
            std::path::PathBuf::from("/repo/rdma-p2p.json.last-slice.json")
        );
        assert_eq!(
            path_for(std::path::Path::new("/repo/rdma-p2p-cpu.json")),
            std::path::PathBuf::from("/repo/rdma-p2p-cpu.json.last-slice.json")
        );
        assert_ne!(
            path_for(std::path::Path::new("/repo/rdma-p2p.json")),
            path_for(std::path::Path::new("/repo/rdma-p2p-cpu.json"))
        );
    }

    /// `take` é consumo: um arquivo órfão de um restart que deu errado não pode
    /// ficar ressuscitando fatia velha a cada arranque.
    #[test]
    fn take_le_uma_vez_e_apaga() {
        let dir = std::env::temp_dir().join(format!("rdma-lastslice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("cfg.json.last-slice.json");
        std::fs::write(&f, sample_config_json()).unwrap();

        let first = take_from(&f);
        assert!(first.is_some(), "a primeira leitura devolve a fatia");
        assert!(!f.exists(), "e apaga o arquivo");
        assert!(take_from(&f).is_none(), "a segunda não devolve nada");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn sample_config_json() -> String {
        serde_json::to_string(&crate::runtime::NodeConfig {
            model_path: "/models/m_0_7.gguf".into(),
            model_label: "qwen35".into(),
            start_layer: 0,
            end_layer: 7,
            total_layers: 40,
            peer_count: 5,
            peer_index: 0,
            primary: false,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            n_batch: 512,
            max_sessions: 1,
            prefill_chunk_tokens: 512,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
        })
        .unwrap()
    }
}
```

> Se o `NodeConfig` tiver ganhado campos, complete o literal acima com os que o compilador cobrar — o teste não deve usar `..Default::default()`, porque o ponto dele é que a fatia volta inteira.

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test --release lastslice:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function path_for`.

- [ ] **Step 3: Implementar**

`src/lastslice.rs`:

```rust
//! A fatia que este nó servia, guardada para sobreviver a um reinício.
//!
//! A atribuição de modelo vive só em memória (é decisão de projeto: quem manda
//! é o plano de implantação, não um arquivo esquecido no disco). Um reinício
//! pedido pela interface, porém, não é uma mudança de intenção — é a mesma
//! intenção com um binário novo. Então a fatia é anotada antes de reiniciar e
//! consumida no arranque seguinte.

use crate::runtime::NodeConfig;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Onde este nó anota a sua fatia.
///
/// Deriva do caminho da config porque dois nós da mesma máquina rodam com
/// `RDMA_P2P_CONFIG` diferentes; um nome fixo faria um sobrescrever o outro.
pub fn path() -> PathBuf {
    path_for(&crate::config::Config::config_path())
}

fn path_for(config_path: &Path) -> PathBuf {
    let mut s = config_path.as_os_str().to_os_string();
    s.push(".last-slice.json");
    PathBuf::from(s)
}

/// Anota a fatia servida agora.
pub fn save(cfg: &NodeConfig) -> Result<()> {
    let p = path();
    std::fs::write(&p, serde_json::to_string_pretty(cfg)?)
        .with_context(|| format!("não deu para gravar {}", p.display()))
}

/// Lê a fatia anotada e apaga o arquivo. Devolve `None` quando não há nada.
pub fn take() -> Option<NodeConfig> {
    take_from(&path())
}

fn take_from(p: &Path) -> Option<NodeConfig> {
    let raw = std::fs::read_to_string(p).ok()?;
    // Apaga antes de aplicar: um arquivo que não parseia, ou uma fatia que o
    // nó não consegue mais servir, não pode reaparecer a cada arranque.
    let _ = std::fs::remove_file(p);
    serde_json::from_str(&raw).ok()
}
```

Em `src/main.rs`: `mod lastslice;`.

- [ ] **Step 4: Restaurar no arranque**

Em `src/main.rs`, depois de o runtime existir e antes do laço principal (junto do ponto onde hoje o nó fica em `start_gateway_only`), acrescentar:

```rust
    // Uma fatia anotada é um reinício pedido pela interface, não uma intenção
    // nova: o nó volta servindo o que servia.
    if let Some(cfg) = lastslice::take() {
        let label = cfg.model_label.clone();
        let (a, b) = (cfg.start_layer, cfg.end_layer);
        match runtime.apply(cfg).await {
            Ok(()) => info!("↩️  fatia restaurada após reinício: {label} {a}-{b}"),
            Err(e) => warn!("não deu para restaurar a fatia anotada: {e}"),
        }
    }
```

- [ ] **Step 5: Rodar os testes**

Run: `cargo test --release 2>&1 | grep -E "test result|error"`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/lastslice.rs src/main.rs
git commit -m "feat(lastslice): o no volta servindo a fatia que servia

A atribuicao de modelo vive so em memoria, entao reiniciar esvazia o no.
Um reinicio pedido pela interface nao e mudanca de intencao, e a mesma
intencao com binario novo: a fatia e anotada antes e consumida no
arranque. O caminho deriva da config de cada no, senao dois nos da mesma
maquina sobrescreveriam um ao outro. take() apaga ao ler, para um arquivo
orfao nao ressuscitar fatia velha para sempre."
```

---

### Task 4: A rota `dev/restart`

O nó reinicia **a si mesmo** — é o que preserva o ambiente de cada processo.

**Files:**
- Modify: `src/devupdate.rs` (handler `restart_endpoint` e `router`)
- Modify: `src/main.rs` (montar o router novo junto dos outros)

**Interfaces:**
- Consumes: `devupdate::enabled()`, `respawn::respawn_self()`, `lastslice::save()`, `GatewayState`.
- Produces: `POST /api/ui/dev/restart` → `{"restarting": true}` ou 403.

- [ ] **Step 1: Escrever o teste que falha**

Em `src/devupdate.rs`, dentro de `mod tests`:

```rust
    /// Com o portão fechado a rota não existe para efeitos práticos: responde
    /// 403 sem tocar em nada.
    #[test]
    fn portao_fechado_recusa_com_403() {
        assert_eq!(gate_rejection().is_some(), !enabled());
        // Num ambiente de teste a variável não está ligada, então há recusa.
        let r = gate_rejection().expect("sem a variável, tem que recusar");
        assert_eq!(r.0, axum::http::StatusCode::FORBIDDEN);
        assert!(
            r.1.contains(ENV_GATE),
            "a mensagem tem que dizer qual variável liga: {}",
            r.1
        );
    }
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test --release devupdate:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function gate_rejection`.

- [ ] **Step 3: Implementar**

Acrescentar a `src/devupdate.rs`:

```rust
use crate::gateway::GatewayState;
use axum::{extract::State, http::StatusCode, response::{IntoResponse, Response}, routing::post, Json, Router};
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

pub fn router() -> Router<GatewayState> {
    Router::new().route("/api/ui/dev/restart", post(restart_endpoint))
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
        // Folga para a resposta HTTP sair antes de o processo morrer.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let e = crate::respawn::respawn_self();
        tracing::error!("respawn falhou, o nó segue no ar: {e}");
    });

    Json(json!({ "restarting": true })).into_response()
}
```

- [ ] **Step 4: Montar o router**

Em `src/main.rs`, onde os outros routers são unidos ao app do gateway, acrescentar `.merge(devupdate::router())`. Procure a linha que já faz `.merge(deploy::router())` (ou equivalente) e ponha ao lado.

- [ ] **Step 5: Rodar os testes**

Run: `cargo test --release 2>&1 | grep -E "test result|error"`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/devupdate.rs src/main.rs
git commit -m "feat(devupdate): rota para o no reiniciar a si mesmo

Nenhum no consegue respawnar o irmao: nao conhece o RDMA_P2P_CONFIG nem o
RDMA_P2P_NODE_KEY dele. Cada um se reinicia herdando o proprio ambiente,
anotando antes a fatia que servia."
```

---

### Task 5: As checagens antes de tocar no repositório

Funções puras sobre a saída do git, testáveis sem repositório nenhum.

**Files:**
- Modify: `src/devupdate.rs`

**Interfaces:**
- Produces: `enum Preflight { Ok, BranchErrado(String), DesatachadoOuSemBranch, ArvoreSuja(Vec<String>) }` e `fn preflight(branch: &str, status_porcelain: &str) -> Preflight`, `fn Preflight::mensagem(&self) -> Option<String>`.

- [ ] **Step 1: Escrever o teste que falha**

```rust
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
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test --release devupdate:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type Preflight`.

- [ ] **Step 3: Implementar**

```rust
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
```

- [ ] **Step 4: Rodar os testes**

Run: `cargo test --release devupdate:: 2>&1 | grep -E "test result|error"`
Expected: PASS, 5 testes.

- [ ] **Step 5: Commit**

```bash
git add src/devupdate.rs
git commit -m "feat(devupdate): checagens antes de tocar no repositorio

Branch diferente de main, HEAD destacado e arvore suja recusam com a
mensagem dizendo o que houve. Arquivo nao rastreado nao impede: o pull
nao o toca. Numa maquina onde alguem esta codificando, o botao falha e
explica em vez de arrumar a casa alheia."
```

---

### Task 6: A rota `dev/update`

**Files:**
- Modify: `src/devupdate.rs`

**Interfaces:**
- Consumes: `preflight`, `gate_rejection`, `webui::{jobs, new_job}`, `PeerInfo::{hostname, gateway_port}`.
- Produces: `POST /api/ui/dev/update` → `{"job_id": "..."}`.

- [ ] **Step 1: Escrever o teste que falha**

```rust
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
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test --release devupdate:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function build_args`.

- [ ] **Step 3: Implementar os auxiliares**

```rust
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
```

- [ ] **Step 4: Implementar a rota**

```rust
use std::process::Stdio;
use tokio::process::Command as TokioCommand;

async fn git(root: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let out = TokioCommand::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
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
```

E o handler, acrescentando `.route("/api/ui/dev/update", post(update_endpoint))` ao `router()`:

```rust
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

        let fail = |msg: String| async {
            let mut jobs = crate::webui::jobs().write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                j.state = "failed";
                j.message = msg;
                j.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
        };

        // ── Checagens ────────────────────────────────────────────────────────
        let branch = match git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
            Ok(b) => b,
            Err(e) => return fail(e).await,
        };
        let status = match git(&root, &["status", "--porcelain"]).await {
            Ok(s) => s,
            Err(e) => return fail(e).await,
        };
        if let Some(msg) = preflight(&branch, &status).mensagem() {
            return fail(msg).await;
        }

        // ── Pull ─────────────────────────────────────────────────────────────
        step(&job_id, "puxando o código").await;
        if let Err(e) = git(&root, &["pull", "--ff-only", "origin", "main"]).await {
            return fail(e).await;
        }

        // ── Build ────────────────────────────────────────────────────────────
        step(&job_id, "compilando").await;
        let features = std::env::var(ENV_FEATURES).ok();
        let args = build_args(features.as_deref());
        let out = TokioCommand::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(&args)
            .current_dir(&root)
            .stdin(Stdio::null())
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let tail: String = err.lines().rev().take(30).collect::<Vec<_>>()
                    .into_iter().rev().collect::<Vec<_>>().join("\n");
                return fail(format!("o build falhou. Ninguém reiniciou.\n{tail}")).await;
            }
            Err(e) => return fail(format!("não deu para rodar cargo: {e}")).await,
        }

        // ── Reinícios ────────────────────────────────────────────────────────
        step(&job_id, "reiniciando os nós desta máquina").await;
        let me = state.hostname().await;
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
```

> `state.hostname()` pode não existir: use `crate::sysmon::sample().await.hostname`, que é a fonte que o `node_info` já usa. Ajuste a linha se o compilador cobrar. `state.http` é o cliente que o gateway já mantém; se o campo tiver outro nome, use o mesmo que `deploy.rs` usa para falar com peers.

- [ ] **Step 5: Rodar os testes**

Run: `cargo test --release 2>&1 | grep -E "test result|error"`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/devupdate.rs
git commit -m "feat(devupdate): rota que atualiza e reinicia a maquina

Confere branch e arvore, puxa com --ff-only, compila com as features da
maquina (RDMA_P2P_BUILD_FEATURES, sem o qual a ksia perderia CUDA) e so
entao manda cada no da maquina reiniciar-se, este por ultimo. Qualquer
falha termina o job em failed e ninguem reinicia: a maquina fica como
estava, servindo o binario que ja esta em memoria."
```

---

### Task 7: O botão no card

**Files:**
- Modify: `web-ui/src/lib/types.ts` (campos `build_commit` e `dev_update` em `NodeInfo`)
- Modify: `web-ui/src/lib/api.ts` (chamada `devUpdate`)
- Modify: `web-ui/src/routes/Dashboard.tsx` (`PeerDetailCard`, a partir da linha 110)

**Interfaces:**
- Consumes: `GET /api/ui/node` (`dev_update`, `build_commit`), `POST /api/ui/dev/update`.

- [ ] **Step 1: Tipos e chamada**

Em `web-ui/src/lib/types.ts`, no tipo `NodeInfo`:

```ts
  /** Commit de que o binário foi construído. */
  build_commit: string;
  /** Este nó aceita atualizar-se pela interface (RDMA_P2P_DEV_UPDATE=1). */
  dev_update: boolean;
```

Em `web-ui/src/lib/api.ts`, ao lado das outras chamadas autenticadas:

```ts
export function devUpdate(baseUrl: string) {
  return post<{ job_id: string }>(`${baseUrl}/api/ui/dev/update`, {});
}
```

> Use o mesmo helper de POST autenticado que `apply`/`launch` já usam neste arquivo; não invente um caminho novo de autenticação.

- [ ] **Step 2: O botão**

Em `PeerDetailCard`, no rodapé do card (junto de onde `ReleaseSlice` aparece), acrescentar:

```tsx
{n?.dev_update && (
  <Confirm
    title="Atualizar esta máquina?"
    body={
      <>
        <p>
          Puxa o código do <code>main</code>, recompila e reinicia todos os nós
          de <strong>{n.hostname}</strong>.
        </p>
        <p className="mt-2 text-xs text-muted">
          Para se houver alterações não commitadas ou se o repositório estiver em
          outro branch. Um <code>cargo build</code> aberto nessa máquina será
          serializado com este, e o binário dela também é substituído.
        </p>
      </>
    }
    onConfirm={() => devUpdate(peerBaseUrl(peer))}
  >
    <Button tone="neutral">Atualizar e reiniciar</Button>
  </Confirm>
)}
```

E, ao lado do `build_commit` no cabeçalho do card, mostrar o commit em fonte monoespaçada para o operador ver quem está desatualizado sem abrir nada:

```tsx
{n?.build_commit && (
  <span className="font-mono text-[0.62rem] text-muted">{n.build_commit}</span>
)}
```

> `Confirm` e `Button` já existem em `web-ui/src/components/`. `peerBaseUrl` é o helper que o `Dashboard` já usa para falar com um peer; se tiver outro nome, use o mesmo dos botões existentes.

- [ ] **Step 3: Construir a interface**

Run: `cargo build --release 2>&1 | grep -E "error|warning: unused|Finished"`
Expected: `Finished` — o `build.rs` reconstrói o bundle e falha se o TypeScript não compilar.

- [ ] **Step 4: Conferir na tela**

Run: `RDMA_P2P_DEV_UPDATE=1 ./target/release/rdma-p2p` e abrir <http://localhost:8080/>.
Expected: o card deste nó mostra o commit e o botão. Num nó subido sem a variável, o botão não aparece.

- [ ] **Step 5: Commit**

```bash
git add web-ui/src/lib/types.ts web-ui/src/lib/api.ts web-ui/src/routes/Dashboard.tsx
git commit -m "feat(ui): botao de atualizar e reiniciar no card do peer

So aparece quando o peer diz dev_update, para a tela nunca oferecer o que
vai dar 403. O card passa a mostrar o commit do binario, que e como se ve
quem esta desatualizado sem abrir a maquina."
```

---

### Task 8: Documentar no CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Escrever a seção**

Acrescentar, depois da seção "Interface web, API OpenAI e monitor":

```markdown
### Atualizar um nó pela própria interface (só em desenvolvimento)

`src/devupdate.rs`. O card de cada peer ganha "Atualizar e reiniciar" quando
aquele processo subiu com `RDMA_P2P_DEV_UPDATE=1` — variável de ambiente, por
processo, porque a config é reescrita pela interface e o portão ficaria ligável
pela rede. `RDMA_P2P_BUILD_FEATURES=cuda` (ou `metal`) diz com que features a
máquina compila; sem isso a ksia perde a GPU no primeiro clique.

A sequência recusa branch fora do `main`, HEAD destacado e árvore suja, e só
então faz `git pull --ff-only` e `cargo build --release`. **Nenhum comando
destrutivo entra nisso** — não há `reset`, `checkout`, `clean` nem `stash` —
então numa máquina onde alguém está codificando o botão falha e explica.

Cada nó reinicia **a si mesmo** (`/api/ui/dev/restart`): nenhum consegue
respawnar o irmão, porque não conhece o `RDMA_P2P_CONFIG` nem o
`RDMA_P2P_NODE_KEY` dele. A fatia servida é anotada em
`<config>.last-slice.json` e reaplicada no arranque, então a máquina volta
servindo o mesmo.

`/api/ui/node` devolve `build_commit`, carimbado pelo `build.rs`, com sufixo
`-sujo` quando a árvore tinha alterações. É como se descobre um peer
desatualizado sem shell na máquina — o defeito de 2026-08-10 levou uma tarde
por falta disso.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: atualizacao remota de um no pela interface"
```

---

## Auto-revisão

**Cobertura do spec:** portão (Task 2), duas rotas (Tasks 4 e 6), sequência com as recusas (Task 5 e 6), features do build (Task 6), irmãos por hostname (Task 6), fatia restaurada (Task 3), correção do argv (Task 1), jobs (Task 6), interface (Task 7), documentação (Task 8). O lock de arquivo do spec virou serialização pelo próprio lock do cargo — dois builds simultâneos no mesmo diretório já se serializam, e um lock nosso só acrescentaria um arquivo para limpar depois de um processo morto na hora errada. É a única simplificação em relação ao spec.

**Placeholders:** nenhum "TBD"/"TODO". Os três pontos marcados com `>` são avisos de nome de símbolo que o compilador confirma (`state.http`, `peerBaseUrl`, campos de `NodeConfig`), não trabalho por definir.

**Consistência de tipos:** `enabled()`/`enabled_from()`, `gate_rejection()`, `preflight()`/`Preflight::mensagem()`, `build_args()`, `sibling_urls()`, `lastslice::{save,take,path_for}`, `respawn::{remember_argv,original_argv,respawn_self}` — usados com os mesmos nomes e assinaturas em todas as tarefas.
