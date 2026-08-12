# Transferências simétricas e exclusão de modelos — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Os dois lados de uma transferência a veem e podem cancelá-la, e a interface pode apagar modelos e fatias do disco.

**Architecture:** O receptor continua cunhando o `job_id` e passa a carregá-lo no pedido (`?job=&peer=`); o remetente registra o mesmo id como job de direção `sending` e conta os bytes que ele próprio serve. Cancelar é uma rota simétrica que cada lado chama no outro. Um portão único `ensure_gguf` cobre servir, enviar e apagar.

**Tech Stack:** Rust, axum, futures 0.3 (`stream::unfold`), reqwest, React, TypeScript, vitest.

Spec: `docs/superpowers/specs/2026-08-12-transferencias-simetricas-e-exclusao-design.md`

## Global Constraints

- Só arquivos GGUF podem ser servidos, enviados ou apagados. Regra em conjunção: os 4 primeiros bytes têm que ser `GGUF` **e**, se o arquivo tiver extensão, ela tem que ser `gguf` (sem diferenciar maiúsculas). Blob do Ollama (`blobs/sha256-…`, sem extensão) passa.
- Cancelar **apaga** o `.part`. Nunca deixar arquivo pela metade com nome final.
- `?job=` e `?peer=` são **opcionais**: um peer desatualizado continua transferindo como hoje, só sem visão do lado remetente.
- Nenhuma URL vem do outro lado da rede. O peer manda `node_id`, e quem recebe resolve para URL pela tabela de peers (`state.all_peers()`).
- Mensagens de usuário em português, como o resto do arquivo.
- `cargo test -p node` tem que passar ao fim de cada tarefa.

---

### Task 1: O portão GGUF

**Files:**
- Modify: `crates/node/src/backend/transfer.rs`
- Test: `crates/node/src/backend/transfer.rs` (módulo `tests` no fim)

**Interfaces:**
- Produces: `fn ensure_gguf(path: &Path) -> Result<(), String>` — usado pelas tarefas 2 e 4.

- [ ] **Step 1: Write the failing test**

No módulo `mod tests` existente em `crates/node/src/backend/transfer.rs`:

```rust
/// A regra é conjunção: assinatura no cabeçalho E, se houver extensão, `.gguf`.
/// Blob do Ollama não tem extensão nenhuma e precisa passar.
#[test]
fn only_real_gguf_files_pass_the_gate() {
    let dir = std::env::temp_dir().join(format!("rdma-gguf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let ok = dir.join("fatia.gguf");
    std::fs::write(&ok, b"GGUF\x03\x00\x00\x00").unwrap();
    assert!(ensure_gguf(&ok).is_ok());

    // Como o Ollama guarda: sem extensão, conteúdo é o que manda.
    let blob = dir.join("sha256-abc123");
    std::fs::write(&blob, b"GGUF\x03\x00\x00\x00").unwrap();
    assert!(ensure_gguf(&blob).is_ok());

    // Nome mente sobre o conteúdo.
    let fake = dir.join("mentira.gguf");
    std::fs::write(&fake, b"NOTG____").unwrap();
    assert!(ensure_gguf(&fake).is_err());

    // Conteúdo certo, extensão errada: ainda é não.
    let wrong_ext = dir.join("modelo.txt");
    std::fs::write(&wrong_ext, b"GGUF\x03\x00\x00\x00").unwrap();
    assert!(ensure_gguf(&wrong_ext).is_err());

    // Maiúsculas não importam.
    let upper = dir.join("FATIA.GGUF");
    std::fs::write(&upper, b"GGUF\x03\x00\x00\x00").unwrap();
    assert!(ensure_gguf(&upper).is_ok());

    // Menor que a assinatura.
    let tiny = dir.join("curto.gguf");
    std::fs::write(&tiny, b"GG").unwrap();
    assert!(ensure_gguf(&tiny).is_err());

    assert!(ensure_gguf(&dir).is_err(), "diretório não é arquivo");

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p node only_real_gguf_files_pass_the_gate`
Expected: FAIL — `cannot find function ensure_gguf in this scope`

- [ ] **Step 3: Write the implementation**

Adicione perto de `safe_model_path` em `crates/node/src/backend/transfer.rs`:

```rust
/// Assinatura de um arquivo GGUF: os 4 primeiros bytes.
const GGUF_MAGIC: &[u8; 4] = b"GGUF";

/// O único tipo de arquivo que pode sair desta máquina ou ser apagado por ela.
///
/// Conjunção de propósito: o conteúdo tem que ser GGUF, e o nome não pode
/// mentir sobre isso. Só o cabeçalho deixaria passar um `notas.txt` renomeado
/// por engano; só a extensão barraria todo o store do Ollama, que guarda os
/// modelos como `blobs/sha256-<hash>`, sem extensão nenhuma.
fn ensure_gguf(path: &Path) -> Result<(), String> {
    use std::io::Read;

    if !path.is_file() {
        return Err(format!("não é um arquivo: {}", path.display()));
    }

    if let Some(ext) = path.extension() {
        if !ext.eq_ignore_ascii_case("gguf") {
            return Err(format!(
                "só arquivos GGUF: {} tem extensão .{}",
                path.display(),
                ext.to_string_lossy()
            ));
        }
    }

    let mut head = [0u8; 4];
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("não deu para abrir {}: {e}", path.display()))?;
    file.read_exact(&mut head)
        .map_err(|_| format!("{} é curto demais para ser um GGUF", path.display()))?;
    if &head != GGUF_MAGIC {
        return Err(format!(
            "{} não é um GGUF (assinatura {head:02x?})",
            path.display()
        ));
    }
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p node only_real_gguf_files_pass_the_gate`
Expected: PASS

- [ ] **Step 5: Wire the gate into the two paths where bytes leave the machine**

Em `local_send_path`, troque o corpo depois do `canonicalize`:

```rust
fn local_send_path(requested: &str) -> Result<PathBuf, String> {
    let full = Path::new(requested)
        .canonicalize()
        .map_err(|_| format!("arquivo não encontrado: {requested}"))?;
    if !full.is_file() {
        return Err(format!("não é um arquivo: {requested}"));
    }
    ensure_gguf(&full)?;
    Ok(full)
}
```

Em `servable_path`, aplique o portão ao resultado dos dois caminhos (concessão e path), para que o `grant` também fique coberto:

```rust
async fn servable_path(state: &GatewayState, q: &PathQuery) -> Result<PathBuf, String> {
    let path = if let Some(id) = q.grant.as_deref() {
        grant_path(state, id).await?
    } else {
        let requested = q
            .path
            .as_deref()
            .ok_or_else(|| "informe ?grant= ou ?path=".to_string())?;
        safe_model_path(&state.shard_dir().await, requested)?
    };
    ensure_gguf(&path)?;
    Ok(path)
}
```

- [ ] **Step 6: Fix the existing tests that use fake files**

Os testes `a_peer_cannot_ask_for_files_outside_the_models_dir`, `an_absolute_path_inside_the_models_dir_is_allowed`, `the_operator_may_send_a_file_from_anywhere` e `a_directory_is_not_sendable` escrevem `b"x"` como conteúdo. Troque **todos** os `std::fs::write(&f, b"x")` desses testes por:

```rust
std::fs::write(&f, b"GGUF\x03\x00\x00\x00").unwrap();
```

(`safe_model_path` não chama o portão, então os dois primeiros passariam de qualquer forma; a troca mantém os arquivos de teste coerentes com o que o código agora exige.)

Adicione um teste novo, no mesmo módulo:

```rust
/// O que o operador escolhe na própria tela ainda precisa ser um GGUF.
#[test]
fn the_operator_cannot_send_a_non_gguf() {
    let dir = std::env::temp_dir().join(format!("rdma-xfer-nogguf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("anotacoes.txt");
    std::fs::write(&f, b"nada a ver").unwrap();

    assert!(local_send_path(f.to_str().unwrap()).is_err());

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 7: Run the whole suite**

Run: `cargo test -p node`
Expected: PASS, sem testes falhando

- [ ] **Step 8: Commit**

```bash
git add crates/node/src/backend/transfer.rs
git commit -m "feat(transfer): so GGUF pode sair desta maquina

Conjuncao de proposito: a assinatura no cabecalho garante o conteudo, e a
extensao impede que o nome minta. Blob do Ollama nao tem extensao e passa
pelo cabecalho, que e como aquele store guarda os modelos.

O portao cobre tambem o caminho do grant, por onde um arquivo fora de
models/ podia sair sem nenhuma checagem de tipo."
```

---

### Task 2: Um job com dois lados

**Files:**
- Modify: `crates/node/src/backend/transfer.rs`
- Test: `crates/node/src/backend/transfer.rs`

**Interfaces:**
- Consumes: `ensure_gguf` (Task 1).
- Produces: `TransferJob { id, name, direction, peer_host, path, state, bytes_done, bytes_total, bytes_per_sec, eta_secs, detail }`; `TransferState::start_job`, `TransferState::progress`; constantes `TRANSFER_SENDING`/`TRANSFER_RECEIVING`.

- [ ] **Step 1: Write the failing test**

```rust
/// O remetente precisa aparecer na própria lista, com direção own e o mesmo id
/// que o receptor cunhou - é o id compartilhado que torna o cancelamento
/// simétrico possível.
#[tokio::test]
async fn a_served_download_registers_a_sending_job() {
    let transfers = TransferState::default();

    transfers
        .start_job(
            "job-1",
            "fatia_0_15.gguf",
            TRANSFER_SENDING,
            "ksia",
            "/models/fatia_0_15.gguf",
            4_096,
        )
        .await;

    let jobs = transfers.jobs.read().await;
    let job = jobs.get("job-1").expect("job registrado");
    assert_eq!(job.direction, TRANSFER_SENDING);
    assert_eq!(job.peer_host, "ksia");
    assert_eq!(job.bytes_total, 4_096);
    assert_eq!(job.state, TRANSFER_RUNNING);
    assert_eq!(job.path, "/models/fatia_0_15.gguf");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p node a_served_download_registers_a_sending_job`
Expected: FAIL — `no method named start_job` / `no field direction`

- [ ] **Step 3: Extend `TransferJob` and add the constants**

Em `crates/node/src/backend/transfer.rs`, junto das constantes `TRANSFER_*`:

```rust
const TRANSFER_SENDING: &str = "sending";
const TRANSFER_RECEIVING: &str = "receiving";
```

Substitua a struct:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct TransferJob {
    pub id: String,
    pub name: String,
    /// `sending` ou `receiving`. O mesmo id existe nas duas máquinas, com
    /// direções opostas — é assim que os dois lados falam da mesma
    /// transferência sem inventar um segundo vocabulário.
    pub direction: &'static str,
    /// A outra máquina. Substitui `from_host`, que mentia na direção de envio.
    pub peer_host: String,
    /// Arquivo local envolvido. A exclusão precisa saber o que está em trânsito.
    pub path: String,
    pub state: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_per_sec: f64,
    pub eta_secs: Option<u64>,
    pub detail: String,
}
```

- [ ] **Step 4: Add the two state helpers**

No `impl TransferState` (crie o bloco se não existir, logo abaixo da struct):

```rust
impl TransferState {
    /// Registra uma transferência em curso. Chamado pelos dois lados, com o
    /// mesmo id.
    pub async fn start_job(
        &self,
        id: &str,
        name: &str,
        direction: &'static str,
        peer_host: &str,
        path: &str,
        bytes_total: u64,
    ) {
        self.jobs.write().await.insert(
            id.to_string(),
            TransferJob {
                id: id.to_string(),
                name: name.to_string(),
                direction,
                peer_host: peer_host.to_string(),
                path: path.to_string(),
                state: TRANSFER_RUNNING.into(),
                bytes_done: 0,
                bytes_total,
                bytes_per_sec: 0.0,
                eta_secs: None,
                detail: String::new(),
            },
        );
    }

    /// Progresso, com a mesma aritmética nos dois lados.
    pub async fn progress(&self, id: &str, done: u64, moved: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f64().max(0.001);
        let rate = moved as f64 / secs;
        let mut jobs = self.jobs.write().await;
        if let Some(j) = jobs.get_mut(id) {
            j.bytes_done = done;
            j.bytes_per_sec = rate;
            j.eta_secs = if rate > 1.0 {
                Some(((j.bytes_total.saturating_sub(done)) as f64 / rate) as u64)
            } else {
                None
            };
        }
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p node a_served_download_registers_a_sending_job`
Expected: PASS

- [ ] **Step 6: Make the receiver mint the id before asking, and carry it**

Em `spawn_download`, o `job_id` hoje nasce depois do manifesto. Mova-o para o topo da função e acrescente-o ao localizador. Substitua o início de `spawn_download`, da linha do `let key` até o `set_job(&state, job).await;`, por:

```rust
    let key = state.auth.read().await.connection_key.clone();

    // O id nasce aqui e viaja no pedido: o outro lado registra o mesmo id, e é
    // isso que faz "cancelar" valer para a transferência inteira, dos dois
    // lados, em vez de só para a metade local.
    let job_id = uuid::Uuid::new_v4().simple().to_string();

    let mut locator: Vec<(&str, String)> = match &grant {
        Some(g) => vec![("grant", g.clone())],
        None => vec![("path", remote_path.clone())],
    };
    locator.push(("job", job_id.clone()));
    locator.push(("peer", state.network.node_id.clone()));

    let manifest: serde_json::Value = state
        .http
        .get(format!("{base_url}/api/transfer/manifest"))
        .bearer_auth(&key)
        .query(&locator)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("não deu para falar com {from_host}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("manifesto inválido de {from_host}: {e}"))?;

    let total = manifest["size_bytes"].as_u64().unwrap_or(0);
    let md5 = manifest["md5"].as_str().unwrap_or_default().to_string();
    if total == 0 || md5.is_empty() {
        return Err(format!("{from_host} não soube descrever {name}"));
    }

    let free_mb = sysmon_disk_free_mb().await;
    let needed_mb = total / 1_048_576 + DISK_HEADROOM_MB;
    if free_mb > 0 && free_mb < needed_mb {
        return Err(format!(
            "espaço insuficiente: {name} precisa de ~{needed_mb} MB (com folga) e só há {free_mb} MB livres"
        ));
    }

    let dir = state.shard_dir().await.join("recebidos");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("não deu para criar {}: {e}", dir.display()))?;

    let dest_path = dir.join(&name);
    state
        .transfers
        .start_job(
            &job_id,
            &name,
            TRANSFER_RECEIVING,
            &from_host,
            &dest_path.to_string_lossy(),
            total,
        )
        .await;
```

Remova a construção manual do `TransferJob` e a chamada `set_job(&state, job).await;` que ficavam ali. Se `set_job` deixar de ter chamadores, apague-a.

- [ ] **Step 7: Extend `PathQuery` and register the sending job in `download`**

Acrescente os dois campos:

```rust
#[derive(Debug, Deserialize)]
pub struct PathQuery {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub grant: Option<String>,
    /// Id cunhado pelo receptor. Ausente quando o peer é de uma versão antiga:
    /// a transferência funciona igual, só sem visão deste lado.
    #[serde(default)]
    pub job: Option<String>,
    /// `node_id` do receptor, resolvido para URL pela tabela de peers.
    #[serde(default)]
    pub peer: Option<String>,
}
```

No handler `download`, logo depois de resolver `total` (`let total = meta.len();`) e antes de abrir o arquivo, registre o job de envio:

```rust
    // O nome do peer sai da tabela local, nunca do pedido: quem chega só diz
    // qual é o seu node_id.
    let peer_host = match q.peer.as_deref() {
        Some(node_id) => state
            .all_peers()
            .await
            .iter()
            .find(|p| p.node_id == node_id)
            .map(|p| {
                if p.hostname.is_empty() {
                    p.node_id[..12.min(p.node_id.len())].to_string()
                } else {
                    p.hostname.clone()
                }
            })
            .unwrap_or_else(|| node_id[..12.min(node_id.len())].to_string()),
        None => "desconhecido".to_string(),
    };
    if let Some(job_id) = q.job.as_deref() {
        state
            .transfers
            .start_job(
                job_id,
                path.file_name().and_then(|n| n.to_str()).unwrap_or("fatia.gguf"),
                TRANSFER_SENDING,
                &peer_host,
                &path.to_string_lossy(),
                total,
            )
            .await;
    }
```

- [ ] **Step 8: Count the bytes being served**

Substitua as duas linhas `let stream = ...; let body = ...;` do handler `download` por:

```rust
    let body = match q.job.clone() {
        Some(job_id) => Body::from_stream(served_stream(
            state.clone(),
            job_id,
            ReaderStream::with_capacity(file, CHUNK),
            start,
        )),
        None => Body::from_stream(ReaderStream::with_capacity(file, CHUNK)),
    };
```

Acrescente `use tokio_util::io::ReaderStream;` no topo se ainda não estiver lá, e adicione a função abaixo do handler:

```rust
/// Envolve o stream do arquivo para contar o que já saiu e para parar cedo
/// quando o job é cancelado.
///
/// O remetente é quem transmite os bytes, então ele já sabe o progresso: não
/// há relato de volta para se perder, e o número continua correto mesmo se a
/// tela do receptor estiver fechada.
fn served_stream(
    state: GatewayState,
    job_id: String,
    inner: ReaderStream<tokio::fs::File>,
    already: u64,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> {
    use futures::StreamExt;

    let started = Instant::now();
    futures::stream::unfold(
        (inner, state, job_id, already, started, Instant::now()),
        |(mut inner, state, job_id, mut done, started, mut last)| async move {
            if state.transfers.is_cancelled(&job_id).await {
                // Terminar o corpo aqui corta a transferência: o receptor vê o
                // stream acabar antes do Content-Length.
                return None;
            }
            let chunk = inner.next().await?;
            if let Ok(ref b) = chunk {
                done += b.len() as u64;
                if last.elapsed() >= Duration::from_millis(500) {
                    last = Instant::now();
                    state
                        .transfers
                        .progress(&job_id, done, done.saturating_sub(already), started.elapsed())
                        .await;
                }
            }
            Some((chunk, (inner, state, job_id, done, started, last)))
        },
    )
}
```

`is_cancelled` vem na Task 3; por enquanto adicione o stub no `impl TransferState` para o código compilar:

```rust
    pub async fn is_cancelled(&self, _id: &str) -> bool {
        false
    }
```

- [ ] **Step 9: Update the receiver loop to use the shared helpers**

Em `run_download`, troque o bloco de atualização de progresso (o `if last_report.elapsed() >= ...`) por:

```rust
        if last_report.elapsed() >= Duration::from_millis(500) {
            last_report = Instant::now();
            state
                .transfers
                .progress(job_id, done, done.saturating_sub(already), started.elapsed())
                .await;
        }
```

- [ ] **Step 10: Build and run the suite**

Run: `cargo test -p node`
Expected: PASS. Se o compilador reclamar de `from_host` em algum ponto, é o campo renomeado para `peer_host` — corrija no local apontado.

- [ ] **Step 11: Commit**

```bash
git add crates/node/src/backend/transfer.rs
git commit -m "feat(transfer): os dois lados enxergam a mesma transferencia

O job_id passa a nascer antes do manifesto e viajar no pedido, entao o
remetente registra o mesmo id com direcao sending e conta os bytes que ele
proprio serve - sem relato de volta que possa se perder, e correto mesmo
com a tela do receptor fechada.

Um puxar iniciado pelo peer tambem passa a aparecer na maquina que serve,
onde antes era invisivel.

job e peer sao opcionais: peer de versao antiga transfere igual."
```

---

### Task 3: Cancelar dos dois lados

**Files:**
- Modify: `crates/node/src/backend/transfer.rs`
- Test: `crates/node/src/backend/transfer.rs`

**Interfaces:**
- Consumes: `TransferState::start_job` (Task 2).
- Produces: rota `POST /api/transfer/jobs/:id/cancel`; `TransferState::cancel`, `TransferState::is_cancelled`; constante `TRANSFER_CANCELLED`.

- [ ] **Step 1: Write the failing test**

```rust
/// Cancelar é idempotente e não depende de o job existir: o aviso do outro
/// lado pode chegar antes de a transferência ter sido registrada aqui.
#[tokio::test]
async fn cancelling_marks_the_job_and_holds_for_a_job_not_yet_known() {
    let transfers = TransferState::default();
    transfers
        .start_job("job-1", "f.gguf", TRANSFER_RECEIVING, "ksia", "/m/f.gguf", 10)
        .await;

    transfers.cancel("job-1", "macbook").await;
    assert!(transfers.is_cancelled("job-1").await);
    {
        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").unwrap();
        assert_eq!(job.state, TRANSFER_CANCELLED);
        assert!(job.detail.contains("macbook"), "detail: {}", job.detail);
    }

    // Duas vezes é igual a uma.
    transfers.cancel("job-1", "macbook").await;
    assert!(transfers.is_cancelled("job-1").await);

    // Um id que ainda não chegou aqui fica marcado assim mesmo, para o loop
    // que começar depois já nascer cancelado.
    transfers.cancel("job-desconhecido", "ksia").await;
    assert!(transfers.is_cancelled("job-desconhecido").await);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p node cancelling_marks_the_job_and_holds`
Expected: FAIL — `no method named cancel`

- [ ] **Step 3: Add the state**

Acrescente a constante junto das outras:

```rust
const TRANSFER_CANCELLED: &str = "cancelled";
```

Acrescente o campo à `TransferState`:

```rust
    /// Ids cancelados. Guardados mesmo para job que ainda não existe aqui: o
    /// aviso do outro lado pode chegar antes de o download começar, e sem isso
    /// a transferência arrancaria já cancelada e ninguém pararia.
    cancelled: RwLock<HashSet<String>>,
```

E `use std::collections::HashSet;` no topo.

Substitua o stub de `is_cancelled` e acrescente `cancel`:

```rust
    pub async fn is_cancelled(&self, id: &str) -> bool {
        self.cancelled.read().await.contains(id)
    }

    /// Marca o cancelamento e rotula o job com quem pediu.
    ///
    /// "Cancelado por ksia" e "falhou" são coisas diferentes para quem olha a
    /// tela: a primeira foi decisão de alguém, a segunda é motivo para tentar
    /// de novo.
    pub async fn cancel(&self, id: &str, by: &str) {
        self.cancelled.write().await.insert(id.to_string());
        let mut jobs = self.jobs.write().await;
        if let Some(j) = jobs.get_mut(id) {
            if j.state != TRANSFER_DONE {
                j.state = TRANSFER_CANCELLED.into();
                j.detail = format!("cancelado por {by}");
            }
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p node cancelling_marks_the_job_and_holds`
Expected: PASS

- [ ] **Step 5: Add the route**

Em `routes()`, acrescente:

```rust
        .route("/api/transfer/jobs/:id/cancel", post(cancel_job))
```

E o handler, junto de `list_jobs`:

```rust
#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    /// Máquina que pediu o cancelamento, para o outro lado poder dizer quem foi.
    #[serde(default)]
    pub by: Option<String>,
    /// `true` quando a chamada veio do peer. Evita o eco: quem foi avisado não
    /// avisa de volta.
    #[serde(default)]
    pub from_peer: bool,
}

/// Cancela uma transferência. A mesma rota serve para a tela local e para o
/// aviso que o outro lado manda — um verbo só, com a mesma semântica.
async fn cancel_job(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<CancelRequest>>,
) -> Response {
    let req = body.map(|Json(b)| b).unwrap_or(CancelRequest {
        by: None,
        from_peer: false,
    });
    let by = req
        .by
        .unwrap_or_else(|| state.network.hostname.clone());

    let peer_host = state
        .transfers
        .jobs
        .read()
        .await
        .get(&id)
        .map(|j| j.peer_host.clone());

    state.transfers.cancel(&id, &by).await;
    info!("🛑 transferência {id} cancelada por {by}");

    // Avisar é o que separa "cancelado" de "caiu a rede" na tela do outro lado.
    // Se o aviso não chegar, o cancelamento local continua valendo e o outro
    // lado cai no caminho de conexão morta, que reporta o motivo real.
    if !req.from_peer {
        if let Some(host) = peer_host {
            notify_peer_cancel(&state, &id, &by, &host).await;
        }
    }

    Json(json!({ "cancelled": true })).into_response()
}

/// Repassa o cancelamento ao peer que está do outro lado desta transferência.
async fn notify_peer_cancel(state: &GatewayState, id: &str, by: &str, peer_host: &str) {
    let peers = state.all_peers().await;
    let Some(peer) = peers
        .iter()
        .find(|p| p.hostname == peer_host || p.node_id.starts_with(peer_host))
    else {
        warn!("não deu para avisar {peer_host} do cancelamento de {id}: peer desconhecido");
        return;
    };
    let Some(base) = peer.gateway_url() else {
        warn!("não deu para avisar {peer_host}: peer não anuncia porta de gateway");
        return;
    };
    let key = state.auth.read().await.connection_key.clone();
    let sent = state
        .http
        .post(format!("{base}/api/transfer/jobs/{id}/cancel"))
        .bearer_auth(&key)
        .json(&json!({ "by": by, "from_peer": true }))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    if let Err(e) = sent {
        warn!("aviso de cancelamento não chegou em {peer_host}: {e}");
    }
}
```

- [ ] **Step 6: Make the receiver loop stop and clean up**

Em `run_download`, dentro do `while let Some(chunk) = stream.next().await {`, como **primeira** instrução do laço:

```rust
        if state.transfers.is_cancelled(job_id).await {
            drop(file);
            // Cancelar é desistir: o parcial sai do disco. Deixá-lo seria
            // ocupar gigabytes que a tela não mostra.
            let _ = tokio::fs::remove_file(part).await;
            return Err("cancelado".to_string());
        }
```

E no `tokio::spawn` de `spawn_download`, o tratamento de erro não pode transformar cancelamento em falha. Substitua o corpo do `if let Err(e) = run_download(...)`:

```rust
        {
            warn!("transferência de {name} falhou: {e}");
            let _ = tokio::fs::remove_file(&part).await;
            let mut jobs = task_state.transfers.jobs.write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                // `cancel` já rotulou este job; sobrescrever apagaria quem pediu.
                if j.state != TRANSFER_CANCELLED {
                    j.state = TRANSFER_FAILED.into();
                    j.detail = e;
                }
            }
        }
```

- [ ] **Step 7: Run the suite**

Run: `cargo test -p node`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/node/src/backend/transfer.rs
git commit -m "feat(transfer): cancelar dos dois lados, com aviso ao outro

Uma rota so, simetrica: a tela local chama, e quem cancelou chama a mesma
rota no peer. Sem o aviso, cortar o stream e indistinguivel de queda de
rede, e o operador do outro lado tenta de novo achando que foi soluco.

Cancelar apaga o .part - desistir e desistir, e arquivo pela metade nunca
pode parecer fatia usavel.

O id fica marcado mesmo para job ainda desconhecido, senao um aviso que
chega antes do download faria a transferencia arrancar ja cancelada e
ninguem pararia."
```

---

### Task 4: Excluir do disco

**Files:**
- Modify: `crates/node/src/backend/webui.rs`
- Test: `crates/node/src/backend/webui.rs`

**Interfaces:**
- Consumes: `ensure_gguf` (Task 1, precisa virar `pub(crate)`), `TransferJob` (Task 2).
- Produces: `POST /api/ui/models/delete` com `{path, force}` → 200 `{deleted, path, freed_bytes}` ou 409 `{warnings: [{kind, message}]}`.

- [ ] **Step 1: Make the gate reachable**

Em `crates/node/src/backend/transfer.rs`, troque `fn ensure_gguf` por `pub(crate) fn ensure_gguf`.

- [ ] **Step 2: Write the failing test**

Em `crates/node/src/backend/webui.rs`, no módulo `mod tests`:

```rust
/// O aviso do store do Ollama existe porque apagar um blob de lá deixa o
/// manifesto apontando para nada - o store fica quebrado, e não é nosso para
/// consertar.
#[test]
fn an_ollama_blob_is_recognised_by_its_layout() {
    let root = std::env::temp_dir().join(format!("rdma-del-{}", std::process::id()));
    let blobs = root.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::create_dir_all(root.join("manifests")).unwrap();
    let blob = blobs.join("sha256-abc");
    std::fs::write(&blob, b"GGUF").unwrap();

    assert!(is_ollama_blob(&blob));

    let solto = root.join("solto.gguf");
    std::fs::write(&solto, b"GGUF").unwrap();
    assert!(!is_ollama_blob(&solto));

    std::fs::remove_dir_all(&root).ok();
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p node an_ollama_blob_is_recognised_by_its_layout`
Expected: FAIL — `cannot find function is_ollama_blob`

- [ ] **Step 4: Implement the detector**

Em `crates/node/src/backend/webui.rs`:

```rust
/// Um blob do store do Ollama: `<raiz>/blobs/sha256-…`, com `<raiz>/manifests`
/// ao lado. O manifesto é que dá nome ao modelo, e ele não é reescrito quando
/// apagamos o blob por fora.
fn is_ollama_blob(path: &std::path::Path) -> bool {
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
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p node an_ollama_blob_is_recognised_by_its_layout`
Expected: PASS

- [ ] **Step 6: Add the endpoint**

Em `routes()` de `webui.rs`:

```rust
        .route("/api/ui/models/delete", post(delete_model))
```

E o handler:

```rust
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
/// silêncio algo que está no ar - a resposta nomeia o que segura o arquivo.
async fn delete_model(
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

    if let Err(e) = crate::transfer::ensure_gguf(&path) {
        return bad_request(&e);
    }

    let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
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
    let in_flight: Vec<String> = state
        .transfers
        .jobs
        .read()
        .await
        .values()
        .filter(|j| {
            j.path == path.to_string_lossy()
                && (j.state == "running" || j.state == "verifying")
        })
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
            "message": "este arquivo está dentro do store do Ollama; apagá-lo deixa o manifesto do Ollama órfão. O certo é `ollama rm`",
        }));
    }

    if !warnings.is_empty() && !req.force {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "warnings": warnings, "path": path.to_string_lossy(), "size_bytes": size })),
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
    Json(json!({
        "deleted": true,
        "path": path.to_string_lossy(),
        "freed_bytes": size,
    }))
    .into_response()
}
```

Se `bad_request` não existir em `webui.rs`, use:

```rust
    return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
```

- [ ] **Step 7: Run the suite**

Run: `cargo test -p node`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/node/src/backend/webui.rs crates/node/src/backend/transfer.rs
git commit -m "feat(webui): apagar modelo ou fatia do disco pela interface

Avisa, mas nao impede - quem opera decide. Os avisos nomeiam o que segura
o arquivo: fatia servida por este no, transferencia em curso, e blob do
store do Ollama, cujo manifesto fica orfao (o certo la e ollama rm).

So GGUF, pelo mesmo portao que ja governa o que sai da maquina."
```

---

### Task 5: A tela

**Files:**
- Modify: `apps/frontend/src/lib/queries.ts` (tipo `TransferJob`)
- Modify: `apps/frontend/src/routes/Transfers.tsx`
- Modify: `apps/frontend/src/routes/WholeModels.tsx`
- Modify: `apps/frontend/src/routes/Shards.tsx`
- Test: `apps/frontend/src/lib/transferStatus.test.ts`

**Interfaces:**
- Consumes: `POST /api/transfer/jobs/:id/cancel` (Task 3), `POST /api/ui/models/delete` (Task 4).

- [ ] **Step 1: Write the failing test**

Em `apps/frontend/src/lib/transferStatus.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { isActiveTransfer, isFinishedTransfer, canCancel } from "./transferStatus";

describe("canCancel", () => {
  it("permite cancelar só o que ainda está andando", () => {
    expect(canCancel("running")).toBe(true);
    expect(canCancel("verifying")).toBe(true);
    expect(canCancel("done")).toBe(false);
    expect(canCancel("failed")).toBe(false);
    expect(canCancel("cancelled")).toBe(false);
  });

  it("trata cancelled como transferência terminada, não ativa", () => {
    expect(isActiveTransfer("cancelled")).toBe(false);
    expect(isFinishedTransfer("cancelled")).toBe(true);
  });
});
```

- [ ] **Step 2: Run it to see it fail**

Run: `cd apps/frontend && npx vitest run src/lib/transferStatus.test.ts`
Expected: FAIL — `canCancel is not a function`

- [ ] **Step 3: Implement in `apps/frontend/src/lib/transferStatus.ts`**

Acrescente ao arquivo (e inclua `"cancelled"` onde `failed`/`done` já são tratados como terminados):

```ts
/** Só uma transferência em andamento pode ser cancelada. */
export function canCancel(state: string): boolean {
  return state === "running" || state === "verifying";
}
```

- [ ] **Step 4: Run it to see it pass**

Run: `cd apps/frontend && npx vitest run src/lib/transferStatus.test.ts`
Expected: PASS

- [ ] **Step 5: Update the `TransferJob` type**

Em `apps/frontend/src/lib/queries.ts`, no tipo `TransferJob`, troque `from_host: string;` por:

```ts
  direction: "sending" | "receiving";
  peer_host: string;
  path: string;
```

Corrija os usos de `from_host` que o `tsc` apontar em `Transfers.tsx` para `peer_host`.

- [ ] **Step 6: Add the cancel button**

Em `apps/frontend/src/routes/Transfers.tsx`, importe `canCancel` de `../lib/transferStatus` e `X` de `lucide-react` (já importado), e acrescente a função dentro do componente `Transfers`:

```tsx
  async function cancel(id: string) {
    try {
      await api.post(`/api/transfer/jobs/${id}/cancel`);
      toast.success("Transferência cancelada");
      void transfers.refetch();
    } catch (e) {
      toast.fromError("Não deu para cancelar", e);
    }
  }
```

Na linha de cada job ativo, acrescente ao fim da linha:

```tsx
  {canCancel(job.state) && (
    <Button
      variant="ghost"
      icon={<X className="h-3.5 w-3.5" />}
      onClick={() => void cancel(job.id)}
    >
      Cancelar
    </Button>
  )}
```

E mostre a direção junto do nome do peer:

```tsx
  <span className="text-faint">
    {job.direction === "sending" ? "enviando para" : "recebendo de"} {job.peer_host}
  </span>
```

- [ ] **Step 7: Add the delete button to both model screens**

Em `apps/frontend/src/routes/WholeModels.tsx` e `apps/frontend/src/routes/Shards.tsx`, importe `Trash2` de `lucide-react`, `api` de `../lib/api` e `useToast` de `../lib/toast`, e acrescente:

```tsx
  async function remove(path: string, name: string) {
    if (!window.confirm(`Apagar ${name} do disco desta máquina?\n\n${path}`)) return;
    try {
      await api.post("/api/ui/models/delete", { path });
      toast.success(`${name} apagado`);
      void models.refetch();
    } catch (e) {
      // 409 traz os avisos: mostre-os e peça confirmação explícita.
      const warnings = (e as { body?: { warnings?: Array<{ message: string }> } })?.body?.warnings;
      if (warnings?.length) {
        const lista = warnings.map((w) => `• ${w.message}`).join("\n");
        if (!window.confirm(`Atenção:\n\n${lista}\n\nApagar mesmo assim?`)) return;
        try {
          await api.post("/api/ui/models/delete", { path, force: true });
          toast.success(`${name} apagado`);
          void models.refetch();
        } catch (e2) {
          toast.fromError("Não deu para apagar", e2);
        }
        return;
      }
      toast.fromError("Não deu para apagar", e);
    }
  }
```

E o botão na linha de cada modelo/fatia:

```tsx
  <Button
    variant="ghost"
    icon={<Trash2 className="h-3.5 w-3.5" />}
    onClick={() => void remove(m.path, m.name)}
  >
    Excluir
  </Button>
```

Se `ApiError` não expuser o corpo da resposta em `apps/frontend/src/lib/api.ts`, acrescente o campo `body` à classe e preencha-o ao lançar — os avisos do 409 vêm por ali.

- [ ] **Step 8: Typecheck, test and build**

Run: `cd apps/frontend && npx tsc --noEmit && npm test -- --run && npm run build`
Expected: sem erros; todos os testes passando; build concluído

- [ ] **Step 9: Commit**

```bash
git add apps/frontend/src
git commit -m "feat(ui): cancelar transferencia e excluir modelo do disco

A lista de transferencias passa a dizer a direcao e o peer, e oferece
cancelar enquanto a transferencia anda. Excluir mostra os avisos do 409 e
so entao pede confirmacao explicita para forcar."
```

---

### Task 6: Verificação e entrega

**Files:** nenhum novo.

- [ ] **Step 1: Full suites**

Run: `cargo test -p node`
Expected: PASS

Run: `cd apps/frontend && npx tsc --noEmit && npm test -- --run && npm run build`
Expected: PASS

- [ ] **Step 2: Whitespace check**

Run: `git diff --check`
Expected: sem saída

- [ ] **Step 3: Manual check against a live pair**

Com dois nós no ar: envie uma fatia de um para o outro e confirme que **as duas** telas mostram a transferência, com direções opostas. Cancele de um lado e confirme que o outro diz "cancelado por &lt;máquina&gt;", não "falhou". Confira que o `.part` sumiu de `<shard_dir>/recebidos/`.

- [ ] **Step 4: Push**

```bash
git push origin HEAD:main
```
