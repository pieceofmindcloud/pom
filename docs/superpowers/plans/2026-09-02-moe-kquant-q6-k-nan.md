# MoE K-quant Q6_K NaN Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Corrigir a leitura do layout GGML Q6_K no executor MoE compartilhado por CPU e CUDA, provando a dequantização, o `gate_up` com oito experts e a não regressão do caminho heterogêneo.

**Architecture:** Manter a dequantização Q6_K como uma única convenção de bytes compartilhada pelo executor K-quant CPU, pelo caminho heterogêneo e pelo kernel CUDA. Corrigir apenas os offsets de layout comprovados pelo `block_q6_K` do fork local, sem alterar o llama.cpp, o roteamento, a política de cache ou qualquer máscara de NaN. As provas usam blocos no layout real e um fixture de oito experts com hidden 2048.

**Tech Stack:** Rust unit tests, `KQuantExpert`, `KQuantSource`, `HeterogeneousMoeSource`, CUDA `k_dot_q6_k`, GGUF fixtures sintéticos.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-moe-orcamento-zero/ornith-nan.md`

## Global Constraints

- A branch de trabalho é `fm/rdma-moe-kquant-nan`, baseada em `origin/feat/implantar`.
- Não tocar no `llama.cpp`; apenas ler o layout e a referência existentes.
- Não mascarar NaN, não alargar replay CPU, manter `FIT_MAX_UBATCH=512` e não mudar `kv_seq_budget`.
- O caminho heterogêneo que funciona deve permanecer coberto.
- Confirmar que o baseline tinha exatamente 10 falhas conhecidas antes da mudança.
- Atualizar `crates/node/src/inference/AGENTS.md` e `docs/subsystems/freetoken-moe.md` no mesmo commit das mudanças de inferência.
- Validar CUDA somente em ambiente com `nvcc`; a compilação Rust no macOS não compila `cuda_kernels.cu`.
- Publicar apenas com fast-forward rebaseado sobre `origin/feat/implantar`, sem PR e sem pipeline no-mistakes.

Durante a validação da suíte completa, uma leitura além do payload em `IQ4_NL`
foi reproduzida e corrigida de forma incidental: esse formato usa 32 elementos
por bloco, enquanto os demais IQ usam 256. A correção é independente do Q6_K,
mas necessária para manter a suíte determinística.

---

### Task 1: Provas RED do layout Q6_K e do gate_up de oito experts

**Files:**
- Modify: `crates/node/src/inference/moe/format_k_quants.rs` nos testes de dequantização e helpers de fixture.
- Modify: `crates/node/src/inference/moe/format_heterogeneous.rs` nos testes do executor heterogêneo.

**Interfaces:**
- Consumes: `dequantize_row`, `KQuantExpert::from_payload`, `KQuantSource`, `TensorDecoder::K`, `HeterogeneousMoeSource`.
- Produces: testes reproduzíveis que falham no código atual por usar Q6_K real, não o layout artificial antigo.

- [x] **Step 1: Escrever o teste que compara um bloco Q6_K real com referências conhecidas**

  O bloco deve ter 210 bytes na ordem `ql[0..128]`, `qh[128..192]`, `scales[192..208]`, `d[208..210]`. Usar `d=1.0` (`0x3c00` no fim), `ql=0x11`, `qh=0xaa` e escalas `2..17`; afirmar tamanho 256 e valores de referência nos grupos `0`, `16`, `32`, `64`, `96` e `128`.

- [x] **Step 2: Executar somente o teste de dequantização para confirmar RED**

  Rodar `rtk cargo test -p node --bin pom moe::format_k_quants::tests::q6_k_decodifica_bloco_ggml_real_com_referencia -- --exact --nocapture`.

  Esperado: FAIL porque o código atual interpreta os dois primeiros bytes como `d` e desloca `ql`, `qh` e `scales`.

- [x] **Step 3: Escrever o teste completo de `gate_up` K-quant com hidden 2048 e oito experts**

  Construir oito payloads Q6_K com `KQuantExpert::from_payload`, hidden 2048, intermediate 256 e entrada finita determinística. Colocar nos primeiros bytes de cada `ql` um padrão que seria NaN se lido como f16, mas manter `d=1.0` no offset real; executar todos os oito experts e afirmar que cada saída de 2048 valores é finita. Isso prova a rota compartilhada e morde o erro de offset sem aceitar retorno vazio ou recusa.

- [x] **Step 4: Executar a prova de oito experts para confirmar RED**

  Rodar `rtk cargo test -p node --bin pom moe::format_k_quants::tests::q6_k_gate_up_oito_experts_hidden_2048_e_finito -- --exact --nocapture`.

  Esperado: FAIL com valores não finitos no caminho atual.

- [x] **Step 5: Escrever a prova equivalente no executor heterogêneo**

  Montar uma fonte heterogênea Q6_K com o mesmo bloco real, oito experts, hidden 2048, gate/up fundidos e down válido; executar uma rota com oito pesos finitos e afirmar que o resultado de hidden 2048 é finito. A prova deve usar o decoder K compartilhado, não um mock do executor.

- [x] **Step 6: Executar a prova heterogênea para confirmar RED**

  Rodar `rtk cargo test -p node --bin pom moe::format_heterogeneous::tests::q6_k_gate_up_heterogeneo_oito_experts_hidden_2048_e_finito -- --exact --nocapture`.

  Esperado: FAIL pela mesma leitura incorreta do bloco Q6_K.

### Task 2: Corrigir a convenção Q6_K nos caminhos compartilhados

**Files:**
- Modify: `crates/node/src/inference/moe/format_k_quants.rs` em `dequantize_q6_k_block`.
- Modify: `crates/node/src/inference/moe/cuda_kernels.cu` em `k_dot_q6_k`.
- Modify: `crates/node/src/inference/moe/format_k_quants.rs` nos helpers de fixtures, para que Q6_K sintético também siga a ordem GGML real.

**Interfaces:**
- Consumes: as provas RED da Task 1 e a referência somente leitura de `llama.cpp/ggml/src/ggml-common.h` e `ggml-quants.c`.
- Produces: CPU e CUDA lendo `d` em `208`, `scales` em `192`, `qh` em `128` e `ql` em `0`, sem mudança de ABI ou geometria de experts.

- [x] **Step 1: Implementar a correção mínima no decoder Rust**

  Ajustar os slices e a leitura de `d` para a ordem real do `block_q6_K`; preservar a decomposição de q1/q2/q3/q4 e a ordem dos 256 valores validada pela referência.

- [x] **Step 2: Rodar as três provas CPU e heterogênea para confirmar GREEN**

  Rodar os testes de bloco, oito experts K-quant e oito experts heterogêneos; esperar PASS, sem aceitar qualquer filtragem ou substituição de NaN.

- [x] **Step 3: Implementar o mesmo offset no `k_dot_q6_k` CUDA**

  Alterar somente os ponteiros usados pelo decoder Q6_K no kernel, mantendo a geometria por expert, o endereço do payload, as escalas laterais e os outros dtypes intactos.

- [x] **Step 4: Adicionar ou adaptar o teste CUDA diferencial Q6_K**

  Reusar a infraestrutura CUDA existente para enviar o mesmo bloco e a mesma entrada ao kernel, comparar o resultado com a referência CPU dentro da tolerância já usada pelos testes diferenciais e exigir ausência de NaN em `gate_up`.

- [x] **Step 5: Rodar a prova CUDA focal quando `nvcc` estiver disponível**

  O ambiente local não possui `nvcc` nem GPU NVIDIA; a limitação foi registrada e a prova CUDA foi adicionada para execução na máquina de validação, sem declarar CUDA validado localmente.

### Task 3: Documentação, regressão e entrega local-only

**Files:**
- Modify: `crates/node/src/inference/AGENTS.md` para registrar que Q6_K usa o layout GGML real e que a prova numérica cobre o caminho K-quant.
- Modify: `docs/subsystems/freetoken-moe.md` para registrar a causa corrigida e os comandos de validação, sem registrar o defeito como ainda aberto.

**Interfaces:**
- Consumes: comportamento corrigido e resultados das provas das Tasks 1 e 2.
- Produces: documentação factual do runtime atual, commit limpo e branch publicada em `origin/feat/implantar`.

- [x] **Step 1: Atualizar os dois documentos no mesmo passe da mudança**

  Escrever em pt-BR, usar hífen simples, não editar arquivos gerados e remover qualquer anotação que diga que a causa continua sem prova.

- [x] **Step 2: Rodar a suíte focal e verificar que as falhas conhecidas continuam em exatamente 10**

  Rodar os testes MoE focais e depois `rtk cargo test --workspace -- --test-threads=1`; contar o resumo e comparar com o baseline `1257 passed, 10 failed, 16 ignored`, aceitando apenas que novas provas passem e que nenhuma falha adicional apareça. A execução serial final ficou em `1261 passed, 10 failed, 16 ignored`; execuções paralelas também exibiram falhas temporais preexistentes adicionais.

- [x] **Step 3: Revisar diff, branch e documentação antes do commit**

  Rodar `rtk git diff --check`, `rtk git status --short`, revisar os arquivos tocados e confirmar que `llama.cpp`, `FIT_MAX_UBATCH` e `kv_seq_budget` não foram alterados.

- [ ] **Step 4: Commitar a correção com mensagem sem coautor automático**

  Usar uma mensagem curta que nomeie a correção Q6_K e incluir somente código, testes e documentação relacionados.

- [ ] **Step 5: Rebasear sobre `origin/feat/implantar` e publicar sem force**

  Executar `rtk git pull --rebase origin feat/implantar`, verificar novamente o commit e então `rtk git push origin HEAD:feat/implantar`.

- [ ] **Step 6: Registrar a entrega e encerrar**

  Depois de confirmar o SHA publicado e a branch limpa, anexar uma única linha `done: publicado em feat/implantar como <sha>` ao status exigido.

## Self-review

- A prova usa bytes no layout real do `block_q6_K`, em vez de reproduzir o fixture incorreto existente.
- A prova de oito experts usa hidden 2048 e exige saída finita para cada expert.
- O caminho heterogêneo é exercitado com o decoder K compartilhado.
- CPU e CUDA recebem a mesma correção de offsets; nenhum fallback ou mascaramento é introduzido.
- A suíte completa preserva o número conhecido de falhas em 10.
- A documentação dos diretórios tocados e o subsistema MoE entram no mesmo commit.
