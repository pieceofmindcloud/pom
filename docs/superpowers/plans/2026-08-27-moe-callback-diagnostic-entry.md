# Diagnóstico de entrada do callback MoE - Plano de implementação

> **Para agentes:** executar os passos em ordem, mantendo o ciclo TDD e a validação CUDA separada.

**Objetivo:** fazer `POM_MOE_ROUTE_DIAGNOSTICS=1` registrar os argumentos crus na primeira instrução observável do callback e identificar cada retorno antecipado.

**Arquitetura:** `moe_ffn_callback` em `llm.rs` emite um evento bruto sem dereferenciar ponteiros antes de validar argumentos. Um evento de saída registra um motivo estável em todos os caminhos que retornam `false`; o diagnóstico detalhado existente continua depois que a geometria pode ser lida com segurança.

**Tecnologia:** Rust, `tracing`, testes unitários do binário `pom`.

**Especificação:** solicitação do capitão sobre a investigação do NaN no callback MoE.

## Restrições globais

- Não alterar a ponte CUDA nem o payload dos experts.
- A variável `POM_MOE_ROUTE_DIAGNOSTICS` continua desligando toda a instrumentação por padrão.
- Cada retorno antecipado deve indicar seu motivo e preservar ponteiros e contagens crus.
- A compilação de `cuda_kernels.cu` só pode ser validada na máquina com CUDA.

### Tarefa 1: entrada e saídas observáveis do callback

**Arquivos:**

- Modificar: `crates/node/src/inference/llm.rs`
- Modificar: `crates/node/src/inference/AGENTS.md`
- Modificar: `docs/subsystems/freetoken-moe.md`
- Modificar: este plano, marcando os passos executados

- [x] Escrever teste unitário para o formato do motivo de saída e seus argumentos crus.
- [x] Executar o teste e confirmar que falha antes da implementação.
- [x] Emitir `callback_entry` antes de qualquer validação e `callback_exit` em cada retorno `false`, incluindo nulidade, geometria, normalização, metadata e forward.
- [x] Executar `cargo fmt --check` e os testes Rust direcionados.
- [x] Registrar a limitação: a compilação CUDA e a prova numérica permanecem pendentes na máquina do capitão.
- [x] Fazer commit apenas dos arquivos nomeados e empurrar após rebase da branch compartilhada.
