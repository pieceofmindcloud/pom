# Admissão de Memória GPU e Recusa de Prompt - Plano de Implementação

> **Para agentes de implementação:** use `superpowers:executing-plans` para executar este plano tarefa a tarefa. Os passos usam caixas de seleção para acompanhamento.

**Objetivo:** impedir que um prompt acima da capacidade efetiva chegue ao decode CUDA e fazer o fit contar as GPUs físicas elegíveis, mantendo o processo vivo e emitindo uma mensagem acionável.

**Arquitetura:** a admissão do prompt será feita no gateway com a menor capacidade `n_ctx` confirmada pelos peers que podem servir o modelo, inclusive quando o gateway não carrega um modelo local. A sondagem do fit usará a lista de dispositivos elegíveis devolvida pelo wrapper local, e não a capacidade máxima genérica do llama.cpp. Os dois caminhos terão testes unitários independentes para provar que não dependem um do outro.

**Tecnologias:** Rust, Tokio/Axum, testes unitários do binário `pom`, crate `llama-engine`, wrapper C++ existente somente para leitura.

**Especificação:** brief `v1 launch-brief` do captain de 2026-09-08; invariantes e contratos em `docs/subsystems/inferencia.md`, `docs/subsystems/api-http.md` e `crates/llama-engine/AGENTS.md`.

## Restrições globais

- Não alterar `vendor/llama-cpp-sys-2/llama.cpp` nem `FIT_MAX_UBATCH`.
- Não alterar modelo ou quantização.
- Não transformar falha de alocação CUDA em tentativa de capturar um abort do FFI; a proteção deve recusar antes do decode.
- O limite do prompt é a capacidade efetiva por sessão, menos o orçamento de saída da requisição.
- A contagem de dispositivos do fit deve refletir os handles elegíveis reais, não `llama_max_devices()`.
- Atualizar `AGENTS.md` e READMEs de subsistema afetados no mesmo commit, sem editar documentação gerada.

---

### Tarefa 1: registrar as duas reproduções em testes falhos

**Arquivos:**
- Modificar: `crates/node/src/backend/gateway/budget.rs`
- Modificar: `crates/node/src/backend/gateway/generation.rs`
- Modificar: `crates/node/src/backend/gateway/tests.rs`
- Modificar: `crates/llama-engine/src/llm/config.rs`

**Interfaces:**
- Produzir uma função pura para formatar a recusa com `n_ctx`, `max_new_tokens` e prompt estimado.
- Produzir uma função pura que escolha a contagem efetiva de dispositivos a partir da lista elegível e do fallback de memória.

- [ ] **Passo 1: escrever o teste de prompt remoto.**

Adicionar em `crates/node/src/backend/gateway/tests.rs` um caso que chame a função de admissão com um prompt de 30.289 caracteres, capacidade `n_ctx=7_000`, saída de 200 tokens e ausência de configuração local. O teste deve exigir `Some`, conter `Prompt grande demais`, `~10097` e `7000`, e deixar explícita a ação de aumentar `n_ctx` ou reduzir `max_new_tokens`.

- [ ] **Passo 2: executar somente o teste novo.**

Executar `cargo test -p node --bin pom <nome-do-teste> -- --exact`. Esperar falha porque a função ainda não existe ou porque a admissão não é aplicada ao caminho sem runtime local.

- [ ] **Passo 3: escrever o teste da contagem real de GPUs.**

Adicionar em `crates/llama-engine/src/llm/config.rs` um teste puro com capacidade genérica 16, dois handles elegíveis e dois snapshots de memória. O resultado esperado deve ser dois dispositivos e duas margens, sem produzir uma expectativa de 16. Adicionar também o caso sem handles, que conserva um slot de fallback para host.

- [ ] **Passo 4: executar somente o teste novo do fit.**

Executar `cargo test -p llama-engine --lib <nome-do-teste> -- --exact`. Esperar falha pelo helper ainda inexistente.

### Tarefa 2: implementar a admissão de prompt antes do decode

**Arquivos:**
- Modificar: `crates/node/src/backend/gateway/budget.rs`
- Modificar: `crates/node/src/backend/gateway/generation.rs`
- Modificar: `crates/node/src/backend/gateway/tests.rs`

**Interfaces:**
- `prompt_budget_error_of(n_ctx: u32, max_new_tokens: u32, prompt: &str) -> Option<String>` calcula o orçamento e devolve a mensagem acionável.
- `prompt_budget_error(&NodeConfig, &str)` delega para a função pura, preservando o contrato local existente.

- [ ] **Passo 1: implementar a função pura mínima.**

Usar `estimate_tokens(prompt)` e `n_ctx.saturating_sub(max_new_tokens)`. Quando a estimativa exceder o orçamento, devolver `Prompt grande demais: ~{estimated} tokens estimados contra um limite de {budget}...` com orientação de aumentar `n_ctx`, reduzir `max_new_tokens` ou reduzir o prompt; caso contrário devolver `None`.

- [ ] **Passo 2: aplicar a mesma admissão à capacidade dos peers.**

Depois de `ready_for_pipeline` e antes de abrir prefixo, calcular a capacidade mínima dos peers que correspondem ao modelo e ao deployment selecionados. Se existir uma capacidade confirmada, chamar `prompt_budget_error_of`; emitir `GenEvent::Failed` e retornar antes de `plan_pipeline_with_rtt` executar qualquer prefill. Manter a guarda local no início para preservar a resposta rápida do nó que tem modelo.

- [ ] **Passo 3: executar o teste de prompt e os testes de orçamento existentes.**

Executar os testes novos e `cargo test -p node --bin pom gateway::tests::output_limit_reports_no_capacity_when_the_prompt_fills_the_context -- --exact`. Esperar todos passarem e nenhum decode ser iniciado pelo caso de prompt grande.

### Tarefa 3: usar o número real de dispositivos no fit

**Arquivos:**
- Modificar: `crates/llama-engine/src/llm/config.rs`
- Modificar: `crates/llama-engine/src/llm/AGENTS.md` se existir; caso não exista, atualizar somente `crates/llama-engine/AGENTS.md`.
- Modificar: `docs/subsystems/inferencia.md`

**Interfaces:**
- `fit_device_count(available_count: usize, memory_count: usize, max_devices: usize) -> usize` retorna o número de dispositivos físicos que alimentará o fit, com um slot de host quando nenhuma GPU elegível for descoberta.

- [ ] **Passo 1: implementar o helper e substituir `llama_max_devices()` como expectativa física.**

Manter `llama_max_devices()` somente como capacidade de alocação do vetor inicial. Depois de `llama_rs_get_devices`, usar `available_devices.len()` para dimensionar memórias, margens, snapshots, tensor split e overrides. No caminho vazio, usar a contagem real devolvida por `llama_rs_get_device_memory`, limitada à capacidade alocada, e reservar um slot mínimo para host quando ela for zero.

- [ ] **Passo 2: ajustar o aviso de snapshot parcial.**

Comparar `memory_count` com o número físico efetivo escolhido, não com 16. O caso normal das duas RTX deve deixar de registrar `reported_devices=2 expected_devices=16`. Se um snapshot realmente ficar incompleto, manter o aviso porque o fit não deve fingir que conhece memória ausente.

- [ ] **Passo 3: executar os testes do fit.**

Executar `cargo test -p llama-engine --lib llm::config::fit_memory_margin_tests -- --test-threads=1` e o teste novo de contagem. Verificar que `FIT_MAX_UBATCH` e o código vendorizado não foram alterados.

### Tarefa 4: documentação, bateria e publicação

**Arquivos:**
- Modificar: `crates/llama-engine/AGENTS.md`
- Modificar: `crates/node/src/inference/AGENTS.md`
- Modificar: `docs/subsystems/inferencia.md`
- Modificar: este plano

- [ ] **Passo 1: documentar o comportamento verificado.**

Registrar que o gateway recusa prompt acima da menor capacidade confirmada antes de planejar/prefill, inclusive em modo gateway-only, e que o fit conta os handles elegíveis reportados pelo wrapper. Remover qualquer anotação que descreva esses dois defeitos como pendentes quando a correção estiver comprovada.

- [ ] **Passo 2: rodar verificação completa.**

Executar primeiro os testes exatos novos, depois `cargo test --workspace` uma vez. Registrar qualquer vermelho preexistente sem o atribuir à correção e confirmar que não houve novos vermelhos. Executar `git diff --check` e inspeção de `git diff`.

- [ ] **Passo 3: commit coerente.**

Executar `git add` somente nos arquivos deste plano e `git commit -m "fix: recusar prompts antes de OOM CUDA"`.

- [ ] **Passo 4: rebase e publicação.**

Executar `git pull --rebase origin feat/implantar3`, resolver apenas conflitos da linha concorrente sem descartar commits alheios, verificar novamente o commit e executar `git push origin HEAD:feat/implantar3` sem `--force`.
