# Concorrência do pom.db Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminar a contenção causada por abertura, configuração e DDL repetidos no `pom.db`, serializando escritas e tornando perdas de auditoria identificáveis.

**Architecture:** `PomDb::open` abrirá e configurará uma conexão SQLite compartilhada uma única vez, mantendo essa conexão em `Arc<Mutex<Connection>>`. Um mutex de escrita separado deixará explícita a serialização das mutações; a mesma conexão também serializa leituras curtas, o que compensa a eliminação de abertura e DDL por operação e evita qualquer corrida de configuração. O `AuditLog` reutilizará esse `PomDb`, sem abrir uma conexão paralela.

**Tech Stack:** Rust, Tokio, `std::sync::Mutex`, rusqlite bundled, SQLite WAL.

**Spec:** `docs/superpowers/specs/2026-09-02-pom-db-unificado-design.md`

## Global Constraints

- Manter `busy_timeout` como rede de segurança, sem usá-lo como mecanismo de serialização.
- Não aumentar o timeout nem introduzir retry de escrita como solução principal.
- Não perder registros de auditoria em silêncio: cada falha deve incluir a operação e o identificador do registro.
- Atualizar a documentação da pasta e o subsistema afetado no mesmo commit.
- Confirmar que a linha base continua com exatamente 11 falhas de teste conhecidas.
- Publicar somente em `origin/feat/implantar`, sem force push e sem PR.

### Task 1: Banco compartilhado

**Files:**
- Modify: `crates/node/src/backend/db.rs`
- Test: `crates/node/src/backend/db.rs` unit tests

- [x] Escrever e executar o teste determinístico que falha com `database is locked` no código atual.
- [x] Trocar a abertura por operação por uma conexão configurada no `PomDb::open`, com mutex de conexão e mutex explícito de escrita.
- [x] Adaptar todas as operações para reutilizar a conexão sem repetir `configure_connection` ou `ensure_schema`.
- [x] Adicionar teste de inicialização única e manter testes de concorrência e persistência.

### Task 2: Auditoria no mesmo banco

**Files:**
- Modify: `crates/node/src/backend/reqlog.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/src/backend/gateway.rs` only if test construction requires the new constructor
- Test: `crates/node/src/backend/reqlog.rs` unit tests

- [x] Fazer `AuditLog` receber `Arc<PomDb>` e executar escrita e consulta pelo banco compartilhado.
- [x] Remover a inicialização de schema/configuração por conexão do caminho de runtime.
- [x] Incluir operação e request id nos erros de enfileiramento e gravação.
- [x] Testar que uma sequência concorrente de auditoria não perde linhas.

### Task 3: Documentação e verificação

**Files:**
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/subsystems/api-http.md` only if the audit storage contract needs clarification

- [x] Registrar conexão compartilhada, serialização de escritas e escopo da contenção.
- [x] Rodar formato, testes focados, build/teste amplo e a contagem de 11 falhas preexistentes.
- [x] Rebasear em `origin/feat/implantar`, publicar sem force e registrar o SHA.
