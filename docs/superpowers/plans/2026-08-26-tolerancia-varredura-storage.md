# Tolerância a entradas ilegíveis na varredura de armazenamento - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fazer o cálculo de uso e as varreduras do catálogo tolerarem entradas ilegíveis, mantendo erros úteis quando a raiz configurada não puder ser lida.

**Architecture:** `storage::directory_bytes` continuará retornando `io::Result` para falhas na raiz, mas tratará falhas em filhos como entradas puladas e registrará cada uma em `debug`. `enforce_limit` seguirá a mesma regra ao enumerar candidatos. O catálogo já é tolerante; seus walkers receberão logs de diagnóstico para não esconder entradas puladas.

**Tech Stack:** Rust, `std::fs`, `tracing`, testes unitários do crate `node`.

**Spec:** solicitação `v1 launch-brief` sobre subdiretório ilegível dentro de `shard_dir`.

## Global Constraints

- Uma entrada ilegível não pode abortar a soma do restante da árvore.
- Falha na própria raiz inexistente, não diretório ou inacessível deve conservar erro e informar caminho, causa e ação ao usuário.
- Teste de regressão deve criar permissão sem leitura e pular explicitamente quando executado como root.
- Não modificar `CHANGELOG.md` nem o symlink/vendor quebrado de `llama-cpp-sys-2`.
- Atualizar o `AGENTS.md` da pasta tocada e o README de subsistema afetado em pt-BR, usando hífen simples.

---

### Task 1: Fixar a reprodução no cálculo de uso

**Files:**
- Modify: `crates/node/src/models/storage.rs:27-43`
- Test: `crates/node/src/models/storage.rs` (módulo `tests` inline)

**Interfaces:**
- Consumes: `directory_bytes(&Path) -> std::io::Result<u64>` existente.
- Produces: comportamento testado que soma arquivos legíveis e não falha por um diretório filho sem permissão de leitura.

- [ ] **Step 1: Write the failing test**

Adicionar um teste que, quando `std::env::var("USER")`/UID indicar execução como root (`nix` não é dependência, então usar `unsafe { libc::geteuid() }` não é permitido sem libc direta; usar `std::process::Command::new("id").arg("-u")`), remove leitura de `inacessivel`, chama `directory_bytes`, espera o tamanho somente de `legivel`, restaura permissões e limpa a árvore. Se o UID for `0`, retornar cedo com `eprintln!("skipped: ... root ...")`.

- [ ] **Step 2: Run test to verify it fails**

Run: `rtk cargo test -p node storage::tests::sums_readable_files_when_nested_directory_is_inaccessible -- --nocapture`
Expected: FAIL on a non-root runner because `directory_bytes` currently propagates `Permission denied` from the child.

- [ ] **Step 3: Write minimal implementation**

Manter a verificação da raiz como erro retornável; em cada entrada filha, registrar e continuar quando `read_dir`, `file_type`, recurse ou `metadata` falhar. Registrar o caminho exato e a causa com `tracing::debug!`.

- [ ] **Step 4: Run test to verify it passes**

Run: `rtk cargo test -p node storage::tests::sums_readable_files_when_nested_directory_is_inaccessible -- --nocapture`
Expected: PASS em usuário comum; em root, SKIP explícito.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/node/src/models/storage.rs
rtk git commit -m "fix: tolerate unreadable storage subdirectories"
```

### Task 2: Tornar o apply e as varreduras relacionadas robustos

**Files:**
- Modify: `crates/node/src/models/storage.rs:45-105`
- Modify: `crates/node/src/models/catalog.rs:311-329, 790-820`
- Modify: `crates/node/src/backend/webui.rs:2700-2703`
- Modify: `crates/node/src/models/AGENTS.md:43-45`
- Modify: `docs/subsystems/implantacao.md:281-298`
- Test: `crates/node/src/models/storage.rs` (testes inline)

**Interfaces:**
- Consumes: soma tolerante da Task 1.
- Produces: `enforce_limit` com erro de raiz contextualizado e walkers de catálogo/candidatos que continuam após filhos ilegíveis.

- [ ] **Step 1: Write the failing test**

Adicionar teste de `enforce_limit` com arquivo legível de tamanho conhecido e diretório sem leitura; afirmar que o limite abaixo da soma legível aciona a retenção sem erro de permissão, e teste de raiz inexistente afirmando que a mensagem contém o caminho, `existe`/`permissão` e uma orientação de verificar a pasta.

- [ ] **Step 2: Run test to verify it fails**

Run: `rtk cargo test -p node storage::tests::enforce_limit_ignores_unreadable_child -- --nocapture`
Expected: FAIL antes da alteração porque `enforce_limit` recebe o erro do child e a mensagem de raiz ainda não orienta o operador.

- [ ] **Step 3: Write minimal implementation**

Em `enforce_limit`, manter falha para `root`, usar contexto em português com caminho, causa e orientação, e ignorar/logar somente falhas de entradas filhas. Adicionar logs equivalentes aos walkers tolerantes do catálogo. Não alterar o critério de quais arquivos ou fatias são descobertos.

- [ ] **Step 4: Run test to verify it passes**

Run: `rtk cargo test -p node storage::tests -- --nocapture`
Expected: PASS em usuário comum; testes condicionados a root informam SKIP.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/node/src/models/storage.rs crates/node/src/models/catalog.rs crates/node/src/backend/webui.rs crates/node/src/models/AGENTS.md docs/subsystems/implantacao.md
rtk git commit -m "fix: make shard storage scans resilient"
```

### Task 3: Verificar regressão, formatação e entrega

**Files:**
- Verify: `crates/node/src/models/storage.rs`, `crates/node/src/models/catalog.rs`, `crates/node/src/backend/webui.rs`

- [ ] **Step 1: Run formatting and focused tests**

Run: `rtk cargo fmt --all -- --check` e `rtk cargo test -p node storage::tests -- --nocapture`.

- [ ] **Step 2: Run broader validation**

Run: `rtk cargo test -p node` e `rtk cargo test --workspace`; registrar literalmente a falha preexistente do symlink/vendor de `llama-cpp-sys-2` se ocorrer, sem corrigi-la ou contorná-la.

- [ ] **Step 3: Review diff and commit**

Run: `rtk git diff --check`, `rtk git diff origin/main...HEAD`, e confirmar que só o critério de tolerância, diagnóstico, mensagem e documentação foram alterados.

- [ ] **Step 4: Push branch and open PR**

```bash
rtk git push -u origin fm/rdma-uso-disco-subdir-ilegivel
gh-axi pr create --base main --head fm/rdma-uso-disco-subdir-ilegivel --title "fix: tolerate unreadable shard subdirectories" --body "Tolerates unreadable child directories during shard storage accounting, adds actionable root errors, and logs skipped catalog entries. Focused tests pass; broader Rust validation is subject to the known llama-cpp-sys-2 vendor symlink errors."
```
