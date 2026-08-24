# Integração Hugging Face por ModelProvider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Adicionar um contrato agnóstico de provider e uma implementação Hugging Face baseada no CLI `hf`, com descoberta, inspeção, autenticação, download cancelável, progresso, GGUF, licença e manifesto sem rede nos testes.

**Architecture:** `provider.rs` contém somente o contrato e os tipos do domínio. `hf_cli.rs` isola montagem, execução segura e parsing do processo externo. `huggingface.rs` adapta esse runner ao contrato, valida entradas, reutiliza `catalog::parse_quant` e `pom::hash_file`, e delega o manifesto a `ModelStorage`.

**Tech Stack:** Rust async/await, `async_trait`, Tokio process/pipes/cancellation, Serde JSON, `tracing`, `sha256` existente, filesystem portátil e testes unitários com runner fake.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-hf-provider/spec.md`; desenho registrado em `docs/superpowers/specs/2026-08-24-rdma-hf-provider-design.md`.

## Global Constraints

- O POM depende conceitualmente de `ModelProvider`; `HuggingFaceProvider` é uma implementação substituível.
- Execute `hf` com `Command::new("hf").args(...)`; nunca shell, string concatenada ou token em argumento/log/telemetria/erro/resposta.
- Valide `repo_id`, `revision`, nomes de arquivo, filtros e caminhos antes de usá-los.
- O CLI ausente deve virar `HfCliStatus { installed: false, version: None, authenticated: false }`, não instalação implícita.
- Use JSON nas operações estruturadas sempre que o CLI oferecer formato JSON; mantenha o parser de saída no módulo `hf_cli.rs`.
- Downloads devem ser assíncronos, canceláveis e somente armazenar, enumerar e hashear conteúdo; não executar código remoto nem enviar `trust_remote_code`.
- Quantização, tamanho e nome base são opcionais e nunca podem ser inventados; licença desconhecida retorna `detected: false`.
- Testes não acessam Hugging Face, não dependem de `hf` instalado e usam runner fake.
- Escreva documentação em pt-BR, use hífen simples e nunca altere `CHANGELOG.md` ou arquivos gerados.

---

### Task 1: Contrato agnóstico de provider e storage

**Files:**
- Create: `crates/node/src/models/provider.rs`
- Modify: `crates/node/src/main.rs` para declarar `model_provider`
- Modify: `crates/node/src/models/AGENTS.md` para descrever os tipos efetivos
- Test: `crates/node/src/models/provider.rs` no módulo `#[cfg(test)]`

**Interfaces:**
- Produces `ModelProvider`, `ModelStorage`, `LocalModelStorage`, `HfError`,
  `ModelSummary`, `ModelInfo`, `ModelFiles`, `ModelFile`, `GgufFile`,
  `ModelLicense`, `HfCliStatus`, `DownloadOptions`, `DownloadProgress`,
  `DownloadStatus`, `DownloadResult`, `ModelManifest` e `ManifestFile`.
- `ModelProvider` deve expor:
  `async fn search_models(&self, query: &str) -> Result<Vec<ModelSummary>, HfError>`;
  `async fn get_model_info(&self, repo_id: &str) -> Result<ModelInfo, HfError>`;
  `async fn list_files(&self, repo_id: &str, revision: Option<&str>) -> Result<ModelFiles, HfError>`;
  `async fn download_file(&self, repo_id: &str, filename: &str, options: DownloadOptions, progress: Option<ProgressSender>, cancel: CancellationToken) -> Result<DownloadResult, HfError>`;
  `async fn download_model(&self, repo_id: &str, options: DownloadOptions, progress: Option<ProgressSender>, cancel: CancellationToken) -> Result<DownloadResult, HfError>`;
  `async fn get_license(&self, repo_id: &str) -> Result<ModelLicense, HfError>`;
  `async fn check_authentication(&self) -> Result<HfCliStatus, HfError>`.
- `ModelStorage::manifest` recebe source, repo, revisão e arquivos locais e
  produz `ModelManifest` sem conhecer o CLI.

- [ ] **Step 1: Escrever testes falhos de validação e metadados**

  Adicionar testes nomeados `rejects_repo_id_traversal`,
  `accepts_revision_branch_with_internal_slash`,
  `rejects_filename_parent_component`, `parses_quantization_without_guessing`,
  `unknown_license_is_not_detected` e `manifest_keeps_optional_sha256`.
  Os testes devem chamar as funções públicas de crate `validate_repo_id`,
  `validate_revision`, `validate_filename`, `gguf_metadata` e
  `LocalModelStorage::manifest`.

- [ ] **Step 2: Rodar os testes para confirmar RED**

  Run: `rtk cargo test -p node provider::tests -- --nocapture`

  Expected: falha de compilação por módulo/tipos/funções ausentes, sem tocar
  rede ou processo externo.

- [ ] **Step 3: Implementar o contrato mínimo**

  Definir os structs com `Debug`, `Clone`, `Serialize`/`Deserialize` quando
  forem dados da API e `PartialEq` nos tipos de teste. Implementar
  `HfError` com `Display` e `std::error::Error`, redaction no campo de detalhe,
  validações portáveis de separadores e componentes, extração de quantização
  delegando para `crate::catalog::parse_quant`, nome base opcional e
  `LocalModelStorage` que usa `crate::pom::hash_file` dentro de
  `tokio::task::spawn_blocking`. Declarar o trait com `async_trait` e
  `ProgressSender = tokio::sync::mpsc::UnboundedSender<DownloadProgress>`.

- [ ] **Step 4: Rodar os testes para confirmar GREEN**

  Run: `rtk cargo test -p node provider::tests -- --nocapture`

  Expected: todos os testes do contrato passam; nenhum teste cria processo ou
  lê rede.

- [ ] **Step 5: Commitar**

  ```bash
  rtk git add crates/node/src/main.rs crates/node/src/models/provider.rs crates/node/src/models/AGENTS.md
  rtk git commit -m "feat: add model provider contract"
  ```

### Task 2: Runner seguro e parser isolado do hf

**Files:**
- Create: `crates/node/src/models/hf_cli.rs`
- Modify: `crates/node/src/main.rs` para declarar `hf_cli`
- Test: `crates/node/src/models/hf_cli.rs` no módulo `#[cfg(test)]`

**Interfaces:**
- Produces `CommandSpec { program: String, args: Vec<String> }`,
  `ProcessOutput { stdout: String, stderr: String, exit_code: Option<i32> }`,
  `OutputStream`, `ProcessObserver`, `HfCommandRunner`, `TokioHfCommandRunner`
  e funções de parser/classificação consumidas por `huggingface.rs`.
- `HfCommandRunner::run(&self, spec: CommandSpec, observer: ProcessObserver, cancel: CancellationToken) -> Result<ProcessOutput, HfError>`.
- O runner real deve usar pipes separados, `stdin(Stdio::null())`, `.args`,
  observação de chunks e `child.kill()` no cancelamento.

- [ ] **Step 1: Escrever testes falhos de argumentos, parser e redaction**

  Adicionar testes `parses_version_and_auth_json`,
  `parses_model_search_and_file_json`, `download_path_is_last_nonempty_line`,
  `classifies_private_repo_without_token_as_authentication`,
  `classifies_rate_limit`, `redacts_hf_token_from_process_text` e
  `command_spec_keeps_user_value_as_one_argument`. Os testes devem verificar
  arrays de argumentos, não uma string de shell.

- [ ] **Step 2: Rodar os testes para confirmar RED**

  Run: `rtk cargo test -p node hf_cli::tests -- --nocapture`

  Expected: falha porque o módulo e os parsers ainda não existem.

- [ ] **Step 3: Implementar parser e runner**

  Implementar parsing tolerante de arrays/objetos JSON dos comandos `models ls`,
  `models info`, `models card --metadata` e `auth whoami`, extraindo apenas
  campos conhecidos. Interpretar a última linha não vazia como caminho do
  download. Implementar `redact_sensitive` sem regex nova, cobrindo padrões
  `hf_...`, `HF_TOKEN=...`, `token=...` e `Authorization: Bearer ...`.
  Classificar somente mensagens sanitizadas. No runner, ler stdout/stderr em
  tarefas separadas, enviar chunks ao observer, acumular o texto e esperar o
  filho; no cancelamento matar e esperar o filho antes de retornar
  `HfError::Cancelled`. `NotFound` ao criar o processo vira
  `HfError::HfCliNotInstalled`.

- [ ] **Step 4: Rodar os testes para confirmar GREEN**

  Run: `rtk cargo test -p node hf_cli::tests -- --nocapture`

  Expected: todos os testes do parser passam e nenhum executa `hf` real.

- [ ] **Step 5: Commitar**

  ```bash
  rtk git add crates/node/src/main.rs crates/node/src/models/hf_cli.rs
  rtk git commit -m "feat: add safe Hugging Face CLI runner"
  ```

### Task 3: HuggingFaceProvider, downloads e manifesto

**Files:**
- Create: `crates/node/src/models/huggingface.rs`
- Test: `crates/node/src/models/huggingface.rs` no módulo `#[cfg(test)]`
- Modify: `crates/node/src/models/AGENTS.md` com a API efetiva do provider

**Interfaces:**
- Produces `HuggingFaceProvider::new()`,
  `HuggingFaceProvider::with_runner(Arc<dyn HfCommandRunner>)` e
  `HuggingFaceProvider::with_dependencies(Arc<dyn HfCommandRunner>, Arc<dyn ModelStorage>)`.
- Implements every method of `ModelProvider` and keeps all `hf`-specific
  command construction private to this file.
- `download_model` emits `Starting`, progress textual best-effort,
  `Completed`, `Failed` or `Cancelled`; returns local paths, bytes, duration e
  manifesto-ready file metadata through `DownloadResult`.

- [ ] **Step 1: Escrever testes falhos com runner fake**

  Criar `FakeRunner` que guarda cada `CommandSpec`, permite uma fila de
  `ProcessOutput`/erro e envia chunks ao observer. Adicionar testes
  `reports_missing_cli_as_structured_status`,
  `reports_unauthenticated_cli_without_leaking_token`,
  `searches_with_json_arguments`, `maps_repository_not_found`,
  `lists_gguf_and_parses_quantization`, `downloads_file_and_builds_manifest`,
  `maps_download_failure`, `cancellation_stops_download`,
  `rejects_repo_revision_and_filename_traversal` e
  `telemetry_and_errors_do_not_contain_token`.

  O teste de download deve criar um arquivo GGUF temporário com conteúdo
  conhecido, fazer o fake retornar o caminho como última linha e comparar
  bytes, SHA-256, caminho e duração não negativa. O teste de cancelamento deve
  passar `CancellationToken::new()` cancelado antes da chamada e verificar que
  o runner recebeu o cancelamento sem criar shell.

- [ ] **Step 2: Rodar os testes para confirmar RED**

  Run: `rtk cargo test -p node huggingface::tests -- --nocapture`

  Expected: falha de compilação porque `HuggingFaceProvider` e seus métodos
  ainda não existem.

- [ ] **Step 3: Implementar a adaptação**

  Construir cada `CommandSpec` com argumentos separados. Usar `--format json`
  para busca, info, arquivos, card e auth; não chamar `hf auth token`. Mapear
  os erros do runner pelos padrões do parser e retornar o enum tipado. Para
  download, passar revisão, cache, diretório local, include/exclude e arquivos
  opcionais; interpretar o caminho final sem executar nada; enumerar somente
  arquivos regulares dentro do resultado; chamar `LocalModelStorage` para
  SHA-256/manifesto. Emitir `tracing::info!` no sucesso e
  `tracing::warn!` na falha usando apenas campos não secretos. O status de
  instalação/autenticação deve permanecer estruturado mesmo quando `hf` não
  existe ou `whoami` retorna falha de autenticação.

- [ ] **Step 4: Rodar os testes para confirmar GREEN**

  Run: `rtk cargo test -p node huggingface::tests -- --nocapture`

  Expected: todos os testes do provider passam offline e as asserções de
  argumentos confirmam ausência de shell e token.

- [ ] **Step 5: Commitar**

  ```bash
  rtk git add crates/node/src/models/huggingface.rs crates/node/src/models/AGENTS.md
  rtk git commit -m "feat: integrate Hugging Face model provider"
  ```

### Task 4: Documentação final e validação da branch

**Files:**
- Create: `docs/rdma-hf-provider-relatorio.md`
- Modify: `docs/AGENTS.md` para apontar o relatório
- Modify: `crates/node/AGENTS.md` se a declaração final de módulos divergir

**Interfaces:**
- Produces the required eight-section handoff: arquitetura implementada,
  arquivos criados, arquivos alterados, decisões técnicas importantes,
  comandos de teste manual, testes adicionados, limitações conhecidas e
  próximos passos.

- [ ] **Step 1: Escrever o teste/checagem documental falha**

  Conferir por busca que o relatório ainda não existe e listar a checklist da
  especificação em um comando de verificação local, sem adicionar código.

- [ ] **Step 2: Confirmar RED documental**

  Run: `rtk rg -n "Arquitetura implementada|Arquivos criados|Próximos passos" docs/rdma-hf-provider-relatorio.md`

  Expected: código de saída diferente de zero porque o relatório ainda não
  foi criado.

- [ ] **Step 3: Escrever relatório factual**

  Registrar os oito itens em pt-BR, os comandos manuais sem token explícito,
  a limitação de ausência de UI/rota nesta etapa, e o resultado real de cada
  teste/build. Não afirmar cobertura nativa se `llama-cpp-sys-2` falhar.

- [ ] **Step 4: Rodar validação completa**

  Run: `rtk cargo fmt --all -- --check`

  Run: `rtk cargo test -p node`

  Run: `rtk cargo test --workspace`

  Run: `rtk cargo build --release`

  Run: `rtk git diff --check`

  Expected: registrar saídas e falhas exatas no relatório; se a compilação
  nativa de `llama-cpp-sys-2` falhar, manter a falha documentada e não chamá-la
  de verde.

- [ ] **Step 5: Commitar, verificar branch limpa e sinalizar**

  ```bash
  rtk git add docs/rdma-hf-provider-relatorio.md docs/AGENTS.md crates/node/AGENTS.md
  rtk git commit -m "docs: report Hugging Face provider delivery"
  rtk git status --short --branch
  ```

  Depois de verificar que a branch está `fm/rdma-hf-provider` e o commit
  contém todas as alterações, apendar exatamente:

  ```bash
  rtk sh -c 'echo "done: ready in branch fm/rdma-hf-provider" >> "/Users/jairpereira/Sites/firstmate/state/rdma-hf-provider.status"'
  ```
