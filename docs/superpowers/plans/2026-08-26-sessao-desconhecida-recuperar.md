# Recuperação automática de sessão desconhecida: Plano de Implementação

> **Para agentes trabalhadores:** SUB-SKILL OBRIGATÓRIO: Use `superpowers:subagent-driven-development` (recomendado) ou `superpowers:executing-plans` para implementar este plano tarefa por tarefa. As etapas usam a sintaxe de checkbox (`- [ ]`) para acompanhamento.

**Goal:** Fazer o pipeline reconstruir automaticamente o contexto quando um peer perder a sessão durante o prefill, sem perder o prompt nem repetir indefinidamente.

**Architecture:** O gateway continuará sendo o dono da recuperação, porque só ele ainda tem o prompt completo e pode refazer todos os hops em ordem. Uma recusa identificada como sessão desconhecida dispara no máximo uma segunda execução fria de `prefill::run`; o recomeço no chunk 0 reseta cada peer e garante que nenhum estado oculto seja usado com posição incompatível. Falhas posteriores recebem uma mensagem útil e não expõem a instrução interna `reset=true`.

**Tech Stack:** Rust, Tokio, Axum, reqwest, serde/serde_json, testes de integração locais da bancada `prefix_reuse_lab`.

**Spec:** `docs/subsystems/inferencia.md` e o firstmate v1 launch-brief fornecido na tarefa.

## Global Constraints

- A recuperação automática faz somente uma nova execução fria do prefill por requisição.
- A recuperação é aplicada quando o erro indica sessão desconhecida; a recuperação já existente para falhas de reuso de prefixo continua limitada a uma execução fria.
- O prompt completo deve ser reaplicado desde o primeiro chunk; não é permitido reenviar apenas o hidden state do chunk que falhou.
- Não tocar em `vendor/llama-cpp-sys-2`, no symlink de `llama.cpp` ou nos bindings ausentes da linha de base.
- Não alterar `/v1/session/rollback` nem o caminho do defeito `rdma-panic-sessao-rewind`.
- Testes devem usar a bancada local e não o cluster real.
- Atualizar `AGENTS.md` e os READMEs de subsistema afetados no mesmo commit; nunca editar cópias geradas em `apps/frontend/src/docs/`.

---

### Task 1: Reproduzir uma perda de sessão durante o prefill

**Files:**
- Modify: `crates/node/src/inference/prefix_reuse_lab.rs` nas estruturas `Engine`/`Hop`, na ingestão fake de prefill e nos testes finais da bancada.
- Test: `crates/node/src/inference/prefix_reuse_lab.rs`

**Interfaces:**
- Consumes: `gateway::generate_stream_with_stops`, `prefill::run` e os três transportes já exercitados pela bancada.
- Produces: um cenário de peer que remove a sessão uma vez antes de um chunk posterior, responde uma recusa de sessão desconhecida e registra que uma recuperação fria reprocessou o prompt completo.

- [x] **Step 1: Write the failing test**

Adicionar um flag de falha controlada ao motor fake e um teste que o liga no segundo hop. No chunk configurado, o fake deve remover a sessão e devolver uma resposta 400 com `sessão '<id>' desconhecida - envie reset=true antes`; o teste deve executar `generate_stream_with_stops` com `Transporte::todos()`, exigir `falhou == None`, verificar que o prompt foi processado novamente a partir do chunk 0 e verificar que nenhum pedido de rewind chegou a uma sessão ausente.

- [x] **Step 2: Run test to verify it fails**

Run: `rtk cargo test -p node unknown_session_during_prefill_is_recovered -- --nocapture`

Resultado: a execução foi bloqueada antes de compilar os testes pelo symlink ausente
de `vendor/llama-cpp-sys-2/llama.cpp`, já presente na linha de base.

- [ ] **Step 3: Commit**

Não criar commit separado nesta tarefa; manter o teste vermelho junto do ciclo de implementação para preservar a comparação da linha de base da compilação.

### Task 2: Detectar a recusa e repetir o prefill completo uma vez

**Files:**
- Modify: `crates/node/src/inference/relay.rs` junto de `HttpRefusal` e dos predicados de classificação de erro.
- Modify: `crates/node/src/backend/gateway.rs` no bloco de prefill de `generate_stream_inner`.
- Test: `crates/node/src/inference/relay.rs` para a classificação da recusa e `crates/node/src/inference/prefix_reuse_lab.rs` para o caminho completo.

**Interfaces:**
- Consumes: `HttpRefusal`, `RelayFailure`, `ring_relay_error` e `direct_failure`, que preservam a mensagem da sessão perdida em hub, anel e resposta direta.
- Produces: `crate::relay::is_unknown_session_error(&anyhow::Error) -> bool` e uma política do gateway que chama `prefill_attempt(false)` no máximo uma vez quando a primeira execução falha por sessão desconhecida.

- [x] **Step 1: Write the failing classifier test**

Adicionar teste unitário com `HttpRefusal { status: 400, body: "sessão 's' desconhecida - envie reset=true antes", peer_label: "peer" }` e afirmar que `is_unknown_session_error` é verdadeiro. Adicionar também uma recusa 400 comum e afirmar que ela é falsa.

- [x] **Step 2: Run test to verify it fails**

Run: `rtk cargo test -p node relay::tests::unknown_session_refusal_is_classified -- --exact`

Resultado: não executado isoladamente porque a compilação do crate já foi bloqueada
pela falha de vendor registrada acima.

- [x] **Step 3: Write minimal implementation**

Implementar o predicado com base no contrato observável da recusa, incluindo o texto preservado por um `RelayFailure`. No gateway, calcular uma única decisão `should_retry_prefill` antes da primeira chamada: `!cancelled && !is_frame_size_error && (reuse_prefix || is_unknown_session_error)`. Se verdadeira, registrar que a sessão foi reconstruída e executar exatamente `prefill_attempt(false)` uma vez. O segundo resultado deve seguir diretamente para o caminho de erro sem nova chamada.

- [x] **Step 4: Run tests to verify they pass**

Run: `rtk cargo test -p node relay::tests::unknown_session_refusal_is_classified -- --nocapture` e depois `rtk cargo test -p node unknown_session_during_prefill_is_recovered -- --nocapture`.

Resultado: a tentativa de regressão foi bloqueada antes dos testes pelo mesmo symlink
ausente; não declarar PASS sem a compilação do crate.

- [ ] **Step 5: Commit**

Não criar commit separado; prosseguir para a mensagem final e documentação antes do commit compartilhado.

### Task 3: Tornar a falha irrecuperável útil e documentar o ciclo de vida

**Files:**
- Modify: `crates/node/src/backend/gateway.rs` na conversão de erro do prefill para `GenEvent::Failed`.
- Modify: `crates/node/src/inference/prefix_reuse_lab.rs` com um cenário persistente que exige exatamente duas execuções e depois falha.
- Modify: `crates/node/src/inference/AGENTS.md` na descrição de `prefill`, ciclo de vida e defeitos anotados, se necessário.
- Modify: `docs/subsystems/inferencia.md` na seção de prefill e cache de prefixo.

**Interfaces:**
- Consumes: o resultado da política de retry de Task 2.
- Produces: mensagem final sem instrução operacional interna e teste de limite que prova que uma recusa persistente não cria loop.

- [x] **Step 1: Write the failing test**

Estender o fake para uma falha persistente no chunk 1 e adicionar teste com `tokio::time::timeout`. Afirmar que chega um único `GenEvent::Failed`, que não contém `reset=true` nem depende do UUID interno, e que o motor registrou exatamente duas tentativas completas do prefill.

- [x] **Step 2: Run test to verify it fails**

Run: `rtk cargo test -p node unknown_session_recovery_is_bounded -- --nocapture`

Resultado: bloqueado pela compilação de `llama-cpp-sys-2` antes da execução do teste.

- [x] **Step 3: Write minimal implementation**

Ao emitir a falha, substituir apenas erros que passaram pela recuperação automática ou que ainda indicam sessão desconhecida por texto equivalente a: `Prefill falhou: um peer perdeu o estado da sessão; o sistema tentou reconstruir o contexto, mas não conseguiu concluir. Tente enviar a mensagem novamente quando o cluster estiver estável.` Para falhas não relacionadas, preservar o diagnóstico técnico existente.

- [x] **Step 4: Run tests to verify they pass**

Run: `rtk cargo test -p node prefix_reuse_lab::unknown_session_recovery_is_bounded -- --exact --nocapture`

Resultado: não executado até o teste por causa do bloqueio de vendor; a validação
estática confirma formatação e diff sem erros.

- [x] **Step 5: Update documentation**

Registrar que `reap_idle` após 900 segundos, `release` explícito e reinício podem remover legitimamente a sessão física; o cache de prefixo é uma otimização e pode apontar para estado perdido. Explicar que a recuperação reinicia todos os hops desde o prompt completo, o custo extra e que não cobre rollback especulativo.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/node/src/backend/gateway.rs crates/node/src/inference/relay.rs crates/node/src/inference/prefix_reuse_lab.rs crates/node/src/inference/AGENTS.md docs/subsystems/inferencia.md docs/superpowers/plans/2026-08-26-sessao-desconhecida-recuperar.md
rtk git commit -m "fix: recuperar prefill de sessão desconhecida"
```

### Task 4: Validar, sincronizar e entregar na branch compartilhada

**Files:**
- Verify: todos os arquivos alterados e a linha de base de compilação.

**Interfaces:**
- Consumes: os testes e a documentação das tarefas anteriores.
- Produces: commit empurrado para `origin/feat/implantar`, sem PR ou merge.

- [x] **Step 1: Run focused regression tests**

Run: `rtk cargo test -p node prefix_reuse_lab -- --nocapture`

Resultado: a tentativa focada foi feita com filtro do teste de regressão e parou
no mesmo erro preexistente do symlink ausente; nenhum teste foi declarado aprovado.

- [x] **Step 2: Compare compilation baseline**

Run: `rtk proxy cargo check -p node 2>&1`

Resultado: a linha de base local também parou no symlink ausente antes dos bindings;
a tentativa final apresentou o mesmo bloqueio e nenhuma mensagem nova atribuível à
mudança.

- [x] **Step 3: Review the diff and status**

Run: `rtk git diff --check` e `rtk git status --short`.

Expected: nenhum erro de whitespace e somente os arquivos desta tarefa, além do plano.

- [ ] **Step 4: Rebase before push**

Run: `rtk git pull --rebase origin feat/implantar`.

Expected: rebase concluído sem conflito não trivial; se houver conflito não trivial, registrar `blocked:` e parar.

- [ ] **Step 5: Push without force**

Run: `rtk git push origin HEAD:feat/implantar`.

Expected: o commit atual publicado em `origin/feat/implantar`, sem `--force` ou `--force-with-lease`.

- [ ] **Step 6: Report completion**

Append one line to `/Users/jairpereira/Sites/firstmate/state/rdma-sessao-desconhecida-recuperar.status`:

```text
done: empurrado para origin/feat/implantar <sha> - recuperação automática de prefill para sessão desconhecida
```
