# Sessao retida nao despejada por pedido concorrente - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Impedir que um prefill de uma sessão nova remova o KV retido de uma conversa longa quando o runtime tem `max_sessions=1`, preservando o prefixo e devolvendo uma recusa explícita quando não há slot.

**Architecture:** A sessão retida continua ocupando o slot físico até ser reutilizada pela mesma identidade ou liberada explicitamente. O scheduler contará somente sequências realmente livres como slots para sessões novas, e o cache do gateway não reciclará a identidade de uma sessão retida em um miss.

**Tech Stack:** Rust, Tokio scheduler, `PrefixCache`, testes unitários do binário `node`, documentação pt-BR.

**Spec:** Brief do incidente de produção fornecido na tarefa; a investigação permanente está em `docs/subsystems/inferencia.md` §3.1.

## Global Constraints

- Não tocar no `llama.cpp` vendorizado.
- Não alterar `FIT_MAX_UBATCH`.
- Não duplicar telemetria do gateway enquanto o trabalhador `rdma-telemetria-outside-node` não publicar sua mudança na linha compartilhada.
- Preservar os cinco testes vermelhos do baseline e não criar novos vermelhos.
- Atualizar `crates/node/src/inference/AGENTS.md` e `docs/subsystems/inferencia.md` no mesmo commit do código.

### Task 1: Reproduzir a perda da sessão no contrato do cache e da admissão

**Files:**
- Modify: `crates/node/src/inference/prefix_cache.rs` nos testes de `PrefixCache`.
- Modify: `crates/node/src/inference/infer_server/core.rs` no módulo de testes.

**Interfaces:**
- O teste do cache deve provar que um miss conserva a identidade retida para um follow-up posterior.
- O teste da admissão deve provar que `retained` não é contado como slot livre para uma sessão nova.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_miss_keeps_the_retained_session_available_for_a_later_hit() {
    let cache = PrefixCache::new(4);
    cache.acquire(key(), "system", "old-session".into())
        .commit("system".into(), 2);

    let miss = cache.acquire(key(), "unrelated", "new-session".into());
    assert!(!miss.is_hit());
    assert_eq!(miss.session_id(), "new-session");
    miss.discard();

    let hit = cache.acquire(key(), "system\nnext", "follow-up".into());
    assert!(hit.is_hit());
    assert_eq!(hit.session_id(), "old-session");
    hit.discard();
}
```

```rust
#[test]
fn retained_sessions_do_not_create_capacity_for_a_new_prefill() {
    assert_eq!(new_session_slots(0, 1), 0);
    assert_eq!(new_session_slots(2, 1), 2);
}
```

- [ ] **Step 2: Run the exact tests and verify RED**

Run:

```bash
cargo test -p node --bin pom prefix_cache::tests::a_miss_keeps_the_retained_session_available_for_a_later_hit -- --exact
cargo test -p node --bin pom infer_server::core::tests::retained_sessions_do_not_create_capacity_for_a_new_prefill -- --exact
```

Expected: the first test fails because the miss returns `old-session`; the second fails because `new_session_slots` does not exist.

### Task 2: Corrigir a política de slots sem apagar prefixos

**Files:**
- Modify: `crates/node/src/inference/prefix_cache.rs`.
- Modify: `crates/node/src/inference/infer_server/server.rs`.
- Modify: `crates/node/src/inference/infer_server/core.rs`.

**Interfaces:**
- `PrefixCache::acquire` devolve `new_session_id` em todo miss e mantém entradas retidas para hits futuros.
- `new_session_slots(free_slots, retained)` devolve somente `free_slots` e é usado pelo scheduler.
- `InferState::alloc_seq` não chama `evict_retained`; falta de slot devolve erro explícito.

- [ ] **Step 1: Implementar o mínimo para os testes**

```rust
pub(super) fn new_session_slots(free_slots: usize, _retained: usize) -> usize {
    free_slots
}
```

No miss do cache, retornar `new_session_id` sem remover a entrada ociosa. No allocator, depois de `reap_idle()`, retornar `None` se `seq_pool` continuar vazio. No scheduler, passar `new_session_slots(guard.seq_pool.len(), retained)` para `take_batch`.

- [ ] **Step 2: Run the exact tests and verify GREEN**

```bash
cargo test -p node --bin pom prefix_cache::tests::a_miss_keeps_the_retained_session_available_for_a_later_hit -- --exact
cargo test -p node --bin pom infer_server::core::tests::retained_sessions_do_not_create_capacity_for_a_new_prefill -- --exact
```

Expected: both PASS, sem linha de `despejando prefixo retido`.

- [ ] **Step 3: Run the focused regression labs**

```bash
cargo test -p node --bin pom prefix_reuse_lab::unknown_session_recovery_is_bounded -- --exact
cargo test -p node --bin pom prefix_reuse_lab::a_follow_up_sent_the_instant_the_stream_closes_still_hits -- --exact
cargo test -p node --bin pom scheduler::tests::a_new_session_prefill_waits_in_order_when_no_slot_is_free -- --exact
```

Expected: PASS. A request que não tem slot deve falhar com `seq pool esgotado (max N sessões simultâneas)` e não pode converter o follow-up da sessão retida em `sessão desconhecida`.

### Task 3: Documentar a fronteira e validar a entrega

**Files:**
- Modify: `crates/node/src/inference/AGENTS.md` na descrição de `InferState` e do scheduler.
- Modify: `docs/subsystems/inferencia.md` na seção 3.1 e na seção "Por que a sessão desaparece".

**Interfaces:**
- A documentação deve dizer que `retained` é cache quente, não capacidade livre; somente `seq_pool` pode admitir uma sessão nova.
- A documentação deve explicar que `max_sessions=1` produz recusa acionável para uma segunda sessão, preservando a primeira; para concorrência real o operador aumenta `max_sessions`, que multiplica a reserva de KV.

- [ ] **Step 1: Atualizar a documentação no mesmo idioma e sem travessão**

- [ ] **Step 2: Rodar `cargo test -p node` completo**

Esperado: os cinco vermelhos do baseline continuam sendo os únicos vermelhos.

- [ ] **Step 3: Conferir diff, branch, rebase e publicar**

```bash
git diff --check
git status --short
git diff --stat origin/feat/implantar3...HEAD
git add crates/node/src/inference/prefix_cache.rs crates/node/src/inference/infer_server/server.rs crates/node/src/inference/infer_server/core.rs crates/node/src/inference/AGENTS.md docs/subsystems/inferencia.md docs/superpowers/plans/2026-09-08-sessao-retida-nao-despejada.md
git commit -m "fix: preserve retained inference sessions under pressure"
git pull --rebase origin feat/implantar3
git push origin HEAD:feat/implantar3
```
