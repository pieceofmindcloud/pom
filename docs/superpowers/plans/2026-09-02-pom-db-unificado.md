# Banco local único do POM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unificar auditoria, implantações, chaves, histórico de modelos e
conversas do Chat em `pom.db`, criado automaticamente e sem migração legada no
runtime.

**Architecture:** `backend/db.rs` será a fronteira de persistência e abrirá conexões SQLite curtas com WAL, timeout e transações. `GatewayState` compartilhará o banco; deploy, admin, auditoria e histórico do Chat escreverão nele, enquanto o catálogo atualizará o histórico em background. A única migração opcional será um script manual em `scripts/`.

**Tech Stack:** Rust 2021, Tokio, Axum, rusqlite bundled, serde/serde_json, Python 3 stdlib para a ferramenta descartável.

**Spec:** `docs/superpowers/specs/2026-09-02-pom-db-unificado-design.md`

## Global Constraints

- O arranque cria `pom.db` em `POM_DB_DIR` quando definido; sem a variável usa
  `config::project_root()`. O diretório pai é criado automaticamente.
- O runtime não migra `audit.sqlite3` nem `deployments/`; `pom.json` continua
  sendo configuração e cópia legada, mas chaves, térmico e token usados no
  runtime vêm do banco novo sem fallback para esses campos.
- `journal_mode=WAL`, `busy_timeout=5000` ms, `foreign_keys=ON` e `synchronous=NORMAL` são aplicados a toda conexão.
- O histórico de modelos é append/update e nunca perde linhas porque o arquivo desapareceu.
- Revogação é append-only no histórico e uma chave revogada permanece inativa.
- A ferramenta opcional é `scripts/migrate_pom_db.py` e não é chamada pelo binário.
- As fontes antigas permanecem intactas.
- Preferências térmicas vivem em `local_settings`; o token do Hugging Face vive
  em `local_secrets`, com o banco e seus sidecars restritos ao dono.
- Conversas vivem em `chat_conversations` e `chat_messages`, expostas apenas
  por `ConnectionAuth`; o conteúdo não entra em logs de erro.

---

### Task 1: Schema SQLite e API de persistência

**Files:**
- Create: `crates/node/src/backend/db.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/Cargo.toml` only if the existing rusqlite dependency is insufficient
- Test: `crates/node/src/backend/db.rs` unit tests

**Interfaces:**
- Produces `PomDb::open(path)`, `PomDb::path()`, deployment CRUD, API-key CRUD/revocation, `record_catalog`, and test helpers.
- `GatewayState.database: Arc<PomDb>` is available to admin, deploy and catalog code.

- [x] **Step 1: Write failing tests** for absent database creation, required tables, WAL configuration, concurrent writes, and model history surviving a removed file.
- [x] **Step 2: Run** `rtk cargo test -p node db:: -- --nocapture` and confirm the tests fail because `db` and `GatewayState.database` do not exist.
- [x] **Step 3: Implement** schema creation, connection configuration, typed persistence operations, chat history, and `record_catalog` with `present` reconciliation.
- [x] **Step 4: Run** the focused tests and confirm all pass, including a concurrent writer test with multiple threads.
- [x] **Step 5:** Integrar o banco no estado do gateway; o commit final reúne
  esta etapa com as demais mudanças coerentes.

### Task 2: Move deployment records and API keys

**Files:**
- Modify: `crates/node/src/models/deployment.rs`
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `crates/node/src/backend/admin.rs`
- Modify: `crates/node/src/backend/auth.rs`
- Modify: `crates/node/src/main.rs`
- Test: existing admin/deploy/auth tests plus new revocation persistence test

**Interfaces:**
- `deployment::all/save/remove` consume `&PomDb` or equivalent database operations.
- Admin handlers use `GatewayState.database`; revocation receives the authenticated operator identity.

- [x] **Step 1: Write failing tests** proving deploy save/list/remove uses `pom.db`, API-key generation persists there, revoked keys fail authentication, and revocation actor/timestamp remain after reopening.
- [x] **Step 2: Run** focused Rust tests and verify failure against file/config-only behavior.
- [x] **Step 3: Implement** database-backed deployment reads/writes with persistent removal tombstones and database-backed key generation/revocation, keeping old sources untouched.
- [x] **Step 4: Run** admin, deploy, auth and db tests and confirm green.
- [x] **Step 5:** Integrar deploy e administração; o commit final reúne esta
  etapa com as demais mudanças coerentes.

### Task 3: Persist catalog model history

**Files:**
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/models/catalog.rs` only if a small public identity helper is needed
- Test: catalog/database tests

**Interfaces:**
- Catalog refresh/enrichment receives `Arc<PomDb>` and invokes `record_catalog` after a complete inventory.

- [x] **Step 1: Write failing tests** for whole-model and manifest-backed slice records, layer/size/hash fields, and `present=0` after the disk file is removed.
- [x] **Step 2: Run** focused tests and verify they fail because refresh does not write history.
- [x] **Step 3: Implement** background recording, including safe hashing of whole files and preservation of prior rows.
- [x] **Step 4: Run** catalog and database tests and confirm green.
- [x] **Step 5:** Integrar o registro do catálogo no commit final; não houve
  alteração em `catalog.rs`.

### Task 4: Add explicit optional migration tool

**Files:**
- Create: `scripts/migrate_pom_db.py`
- Modify: `scripts/AGENTS.md`
- Test: `rtk python3 -m py_compile scripts/migrate_pom_db.py` e cenário manual
  isolado

**Interfaces:**
- CLI: `python3 scripts/migrate_pom_db.py --database pom.db --audit audit.sqlite3 --deployments deployments --config pom.json`.
- The command validates and copies old data, never deletes or modifies source files, and exits nonzero on unsafe conflicts.

- [x] **Step 1:** Definir o cenário manual isolado para auditoria, implantação,
  chave e preservação das fontes; não criar uma segunda ferramenta de migração.
- [x] **Step 2:** Rodar a compilação Python e o cenário isolado.
- [x] **Step 3: Implement** the one-off copier using stdlib `sqlite3`, JSON validation, explicit transactions, and conflict checks.
- [x] **Step 4: Run** the script validation and confirm sources remain intact.
- [x] **Step 5:** Documentar o caminho do script e que as fontes antigas ficam
  intactas.

### Task 5: Documentation and full verification

**Files:**
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `crates/node/src/models/AGENTS.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/implantacao.md`
- Modify: `docs/subsystems/fatiamento.md`
- Modify: `docs/AGENTS.md`

- [x] **Step 1:** Documentar ownership do banco, WAL, revogação, histórico de
  modelos e conversas, diretório configurável, térmico, Hugging Face e script
  manual em pt-BR.
- [x] **Step 2:** Run `rtk cargo fmt --all -- --check`; as diferenças restantes
  são preexistentes em `backend/tool_call.rs`, `inference/device_placement.rs`,
  `inference/llm.rs` e `inference/prefix_reuse_lab.rs`, fora desta alteração.
- [x] **Step 3:** Rodar as provas focadas de `db`, `config`, `auth`, `admin`,
  `model_provider`, histórico webui e o cenário Python; todas passaram. A suíte
  completa do node terminou com falhas de laboratórios preexistentes além de
  1222 testes passados.
- [x] **Step 4:** Rodar `rtk cargo test --workspace`; os crates fora de `node`
  passaram (13 testes). A execução do node foi interrompida depois de ficar
  presa em teste laboratorial concorrente; o diff e o status foram revisados.
- [ ] **Step 5:** Commit documentation and final fixes, then append the required `done` status only after the branch is clean.
