# NCCL Tensor Parallel Enterprise Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Medir, em uma cópia validável do POM na ksia, se o backend NCCL melhora o tensor parallelism de duas RTX 5060 Ti e registrar uma conclusão reproduzível na linha `feat/enterprise`.

**Architecture:** A investigação separa o binário servido da validação: `/data/ksia-target` não será sobrescrito para o rebuild, enquanto `/data/ftk-cuda-src` e `/data/ftk-cuda-target` formam o checkout e target CUDA isolados já descritos pelo subsistema de build. O benchmark pré e pós-rebuild usa o mesmo prompt curto e `max_tokens=32` nos caminhos denso e MoE; a comparação só atribui efeito ao NCCL quando o binário novo mostra `libnccl.so.2` via `ldd` e as demais condições são registradas.

**Tech Stack:** Rust/Cargo, llama.cpp fork `pom-tensor-parallel`, CMake, CUDA 12/13 disponível na ksia, NCCL, SSH, `ldconfig`, `ldd`, API HTTP do POM e documentação Markdown em pt-BR.

**Spec:** briefing `FIRSTMATE_OP: v1 launch-brief` desta tarefa.

## Global Constraints

- Trabalhar somente nesta worktree e publicar apenas em `origin/feat/enterprise`.
- Não abrir PR, não usar `--force`, não reiniciar produção sem autorização explícita do captain.
- Usar `/data/ftk-cuda-src` e `/data/ftk-cuda-target` para validação; preservar `/data/ksia-target` até haver autorização para promoção.
- Reproduzir antes de mudar qualquer coisa e não declarar melhora sem comparar números observados.
- Documentar em pt-BR, usar hífen simples e não editar arquivos gerados ou `CHANGELOG.md`.

---

### Task 1: Fixar a linha de entrega e o protocolo de medição

**Files:**
- Read: `docs/subsystems/operacao.md`
- Read: `docs/subsystems/build-execucao.md`
- Read: `docs/subsystems/freetoken-moe.md`
- Read: `scripts/bench-rotas.sh`
- Read: `scripts/bench-llamacpp.sh`

**Interfaces:**
- Consumes: `origin/feat/enterprise`, a implantação ativa na ksia e o benchmark histórico em `data/rdma-moe-nao-usa-duas-gpus/` quando presente.
- Produces: SHA da branch, endpoint/modelo/configuração efetivos e uma receita única para as medições pré e pós-rebuild.

- [x] **Step 1: Confirmar o SHA remoto e o worktree**

```bash
git fetch origin
git rev-parse origin/feat/enterprise
git status --short --branch
```

- [x] **Step 2: Inspecionar o contrato de benchmark e a implantação**

```bash
sed -n '1,260p' docs/subsystems/operacao.md
sed -n '1,280p' scripts/bench-rotas.sh
```

Registrar o endpoint, modelo e forma de selecionar `dense/llama.cpp` e `moe` sem alterar a configuração persistida.

- [x] **Step 3: Confirmar a linha histórica sem inventar dados**

```bash
rg -n -i '24[,.]97|15[,.]968|f4b7eb4|benchmark|tok/s' data/rdma-moe-nao-usa-duas-gpus docs/subsystems/freetoken-moe.md
```

Se o diretório não existir ou não contiver o detalhe pedido, registrar a ausência e usar somente os números cuja origem puder ser encontrada.

### Task 2: Reproduzir o benchmark E2E antes do rebuild

**Files:**
- Read: `scripts/bench-rotas.sh`, `docs/subsystems/operacao.md`

**Interfaces:**
- Consumes: endpoint autenticado do gateway e o prompt `Oi, teste. Responda em uma frase curta.`.
- Produces: resultados brutos timestamped dos caminhos `dense/llama.cpp` e `moe`, com `max_tokens=32`, latência, tokens de saída, tok/s e SHA/binário observado.

- [x] **Step 1: Coletar o estado remoto sem mutação**

```bash
ssh ksia 'ldconfig -p | grep libnccl || true; nvidia-smi --query-gpu=name,driver_version,memory.used,memory.free --format=csv,noheader'
```

Consultar também o health/config endpoint autenticado conforme o procedimento já documentado, sem parar ou reiniciar processos.

- [x] **Step 2: Executar o caso denso**

Enviar exatamente uma requisição com o caminho/configuração densa, prompt fixo e `max_tokens=32`; guardar a resposta completa e as métricas publicadas pelo gateway.

- [ ] **Step 3: Executar o caso MoE**

Repetir a requisição com o mesmo prompt, limite e condições, selecionando o modelo/caminho MoE efetivamente servido. Confirmar pela telemetria que `executed_engine=moe` e não inferir o caminho pelo nome do arquivo.

- [ ] **Step 4: Persistir os artefatos de medição**

Guardar os resultados fora de arquivos gerados do produto, com data UTC, host, GPUs, processo/binário, modelo, prompt, `max_tokens`, tokens e tok/s para permitir comparação literal após o rebuild.

### Task 3: Rebuild CUDA isolado com NCCL

**Files:**
- Read: `vendor/llama-cpp-sys-2/build.rs`
- Read: `vendor/llama-cpp-sys-2/llama.cpp/ggml/CMakeLists.txt`
- Read: `docs/subsystems/build-execucao.md`

**Interfaces:**
- Consumes: pacote NCCL já instalado na ksia, checkout do fork em `vendor/llama-cpp-sys-2/llama.cpp` ou `/data/ftk-cuda-llama`, `CUDAARCHS=120` e o source isolado.
- Produces: `/data/ftk-cuda-target/release/pom` construído do zero com `GGML_CUDA_NCCL=ON`, logs do CMake/Cargo e evidência do `ldd`.

- [x] **Step 1: Confirmar descoberta do NCCL pelo sistema e pelo CMake**

```bash
ssh ksia 'ldconfig -p | grep libnccl'
ssh ksia 'find /data/ftk-cuda-target -name CMakeCache.txt -print -exec grep -E "GGML_CUDA_NCCL|NCCL_(FOUND|LIBRARY|INCLUDE)" {} \\;'
```

Se não houver cache ou se o cache não registrar NCCL, isso é evidência a ser investigada antes do rebuild, não motivo para assumir que o link ocorrerá.

- [x] **Step 2: Preparar o source e limpar somente o target de validação**

Usar o procedimento documentado, mantendo `LLAMA_SOURCE` apontando para o checkout do fork e `CARGO_TARGET_DIR=/data/ftk-cuda-target`. A limpeza deve atingir somente o target de validação explicitamente resolvido.

- [x] **Step 3: Compilar com as flags CUDA/NCCL explícitas**

```bash
ssh ksia 'bash -lc "cd /data/ftk-cuda-src && export PATH=/usr/local/cuda-12/bin:\\$PATH && export CUDAARCHS=120 && export CUDACXX=/usr/local/cuda-12/bin/nvcc && export GGML_CUDA_NCCL=ON && CARGO_TARGET_DIR=/data/ftk-cuda-target cargo build --release --features vulkan,cuda"'
```

Adaptar somente paths já comprovados no host. Capturar o output completo e o SHA do source/fork usados no build.

- [x] **Step 4: Validar o artefato sem promovê-lo**

```bash
ssh ksia 'ldd /data/ftk-cuda-target/release/pom | grep -E "libnccl|not found"'
```

O gate passa apenas se houver `libnccl.so.2` resolvido e nenhum `not found`; caso contrário, investigar a descoberta CMake e não reiniciar a produção.

### Task 4: Medir o efeito pós-rebuild e decidir sobre produção

**Files:**
- Read: artefatos pré-rebuild e logs do rebuild
- Read: `/data/ftk-cuda-target/release/pom` via `ldd` e metadados de execução

**Interfaces:**
- Consumes: binário novo linkado a NCCL e o mesmo protocolo da Task 2.
- Produces: comparação denso/MoE antes e depois, delta absoluto/percentual e decisão factual sobre melhora, estabilidade ou regressão.

- [x] **Step 1: Tentar a medição com o binário novo sem tocar no serviço compartilhado**

Se for possível subir uma instância isolada com `POM_HOME`, portas e estado próprios, fazê-lo e repetir o protocolo da Task 2. Não compartilhar identidade, portas ou `POM_HOME` com a produção.

- [ ] **Step 2: Abrir `needs-decision` se a medição exigir restart da produção**

```text
needs-decision [key=nccl-production-restart]: autoriza reiniciar o POM da ksia com o binário /data/ftk-cuda-target/release/pom para medir E2E pós-NCCL? Opções: autorizar janela agora; concluir somente com a prova de build/link e documentar benchmark pós-restart pendente.
```

Não executar restart antes da decisão ou de uma autorização explícita já registrada.

- [ ] **Step 3: Repetir exatamente os dois casos**

Manter prompt, `max_tokens=32`, modelo, temperatura, quantidade de repetições, condição de warmup e método de cálculo constantes. Comparar `dense/llama.cpp` com `moe` e também cada caminho antes/depois.

- [ ] **Step 4: Classificar o resultado**

Chamar de melhora somente se o tok/s medido subir de forma reproduzível sob as mesmas condições. Caso contrário, registrar `não mudou` ou `piorou`, incluindo possíveis limites como custo de comunicação, implementação atual de allreduce, batch pequeno ou gargalos anteriores ao collective.

### Task 5: Documentar a evidência e publicar na linha enterprise

**Files:**
- Modify: `docs/subsystems/freetoken-moe.md`
- Modify: `docs/AGENTS.md` se a nova seção alterar o mapa documental
- Modify: `docs/superpowers/plans/2026-09-16-nccl-tensor-parallel-enterprise.md`

**Interfaces:**
- Consumes: logs, SHAs, `ldconfig`, CMake cache, `ldd` e tabelas de benchmark das Tasks 1-4.
- Produces: registro permanente do método, números, conclusão e causa raiz, sem alegar ganho não medido.

- [x] **Step 1: Escrever a seção de investigação NCCL**

Incluir data, host/GPU, source/fork SHA, flags, caminho do target, prova `ldd`, protocolo exato, tabelas dense/MoE antes/depois e a conclusão técnica. Separar claramente prova de link de prova de desempenho.

- [ ] **Step 2: Rodar a validação proporcional**

Executar os testes/documentação relevantes sem reconstruir ou alterar arquivos gerados; revisar `git diff --check`, status e a consistência dos números.

- [ ] **Step 3: Atualizar sobre commits concorrentes e publicar**

```bash
git fetch
git pull --rebase origin feat/enterprise
git push origin HEAD:feat/enterprise
git ls-remote origin refs/heads/feat/enterprise
```

Resolver conflitos preservando commits alheios, confirmar que o SHA local é o SHA remoto e somente então registrar o estado final.

- [ ] **Step 4: Reportar conclusão ao supervisor**

```text
done: publicado em feat/enterprise, SHA <sha> confirmado; NCCL <melhorou/não mudou/piorou> dense <antes -> depois> tok/s e MoE <antes -> depois> tok/s.
```
