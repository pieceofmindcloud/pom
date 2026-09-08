# Benchmark llama.cpp puro Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Entregar um harness versionado que selecione uma implantação POM a partir de um snapshot, suba o `llama-server` do fork usado pelo POM na ksia, meça a mesma bateria e produza CSV e resumo comparáveis.

**Architecture:** `scripts/bench-llamacpp.sh` será a CLI estável e delegará a implementação determinística a `scripts/bench_llamacpp.py`. O Python selecionará a implantação, gerará prompts, parseará SSE/timings e escreverá os artefatos; o shell controlará o servidor remoto, a verificação de VRAM e o cleanup por PID. O processo remoto usará somente `127.0.0.1:PORT`, nunca tocará na porta 8080 e compilará fora da árvore do fork quando necessário.

**Tech Stack:** Bash, Python 3 stdlib, `jq`, `curl`, `ssh`, `nvidia-smi`, `cmake` e CUDA no host remoto.

**Spec:** briefing `FIRSTMATE_OP: v1 launch-brief` da tarefa.

## Global Constraints

- Não matar, reiniciar ou alterar o POM; recusar a execução quando a VRAM livre não passar a política de segurança.
- Não modificar arquivos rastreados da árvore llama.cpp; configurar builds em diretório externo.
- Usar o fork remoto `/data/fm-prefixo-src/llama.cpp` por padrão, backend CUDA, split automático entre as GPUs.
- Rejeitar `shards`/`tensor_parallel` sem tentar reproduzir RPC distribuído; documentar a limitação.
- Reutilizar exatamente os cinco IDs/prompts da bateria `bench-rotas.sh`, mantendo cópia sincronizada e explicitamente identificada.
- Sempre registrar `n_ctx` efetivo, configuração POM selecionada, flags llama.cpp e diferenças não replicadas.
- Derrubar apenas o PID do llama-server criado pelo harness em saída normal, erro, interrupção ou timeout.
- Escrever documentação em pt-BR e usar hífen simples.

---

### Task 1: Contratos puros de seleção e métricas

**Files:**
- Create: `scripts/test_bench_llamacpp.py`
- Create: `scripts/bench_llamacpp.py`

**Interfaces:**
- Produces `select_deployment(snapshot, model=None, deploy_id=None)`, `build_bench_plan(...)`, `parse_sse_result(...)`, `median(...)` e `summary_rows(...)` para a CLI.

- [ ] **Step 1: Write the failing test**

  Cobrir a seleção default pelo modelo ativo e deployment `serving`, recusa de ambiguidade sem `--deploy-id`, rejeição de deployment multi-node e extração dos timings SSE com mediana.

- [ ] **Step 2: Run test to verify it fails**

  Run: `python3 -m unittest scripts.test_bench_llamacpp -v`
  Expected: erro de importação porque `scripts/bench_llamacpp.py` ainda não existe.

- [ ] **Step 3: Write minimal implementation**

  Implementar dataclasses/validação para o wrapper `.deployment`, selecionar `default_model`/`active_model`, exigir `whole`, mapear os campos POM e interpretar o evento final nativo de `/completion`.

- [ ] **Step 4: Run test to verify it passes**

  Run: `python3 -m unittest scripts.test_bench_llamacpp -v`
  Expected: todos os testes passam.

- [ ] **Step 5: Commit**

  `git add scripts/test_bench_llamacpp.py scripts/bench_llamacpp.py && git commit -m "test: define contratos do benchmark llama.cpp"`

### Task 2: CLI, servidor remoto e medição

**Files:**
- Create: `scripts/bench-llamacpp.sh`
- Modify: `scripts/bench_llamacpp.py`

**Interfaces:**
- CLI: `bench-llamacpp.sh CONFIG_JSON [--model LABEL] [--deploy-id ID] [--n-ctx N] [--port N] [--remote-host HOST] [--llama-src PATH] [--model-root PATH] [--csv PATH] [--summary PATH] [--output-dir DIR] [--repeticoes N]`.
- Variáveis: `LLAMACPP_SSH_HOST`, `LLAMACPP_SRC`, `LLAMACPP_MODEL_ROOT`, `LLAMACPP_BUILD_DIR`, `LLAMACPP_BUILD_JOBS`, `LLAMACPP_CUDA_ARCHITECTURES`, `LLAMACPP_MIN_FREE_MB`, `POM_BENCH_PREFILL_WORDS` e `POM_BENCH_PREFILL_MAX_TOKENS`.
- Produces CSV estável, `summary.md`, `metadata.json` e log remoto com backend explicitamente `llama.cpp puro`.

- [ ] **Step 1: Write the failing integration-shaped test**

  Acrescentar no teste uma asserção do corpo `/completion`, dos cinco IDs, dos campos CSV e da mediana sem warmup.

- [ ] **Step 2: Run test to verify it fails**

  Run: `python3 -m unittest scripts.test_bench_llamacpp -v`
  Expected: falha nas interfaces novas.

- [ ] **Step 3: Write minimal implementation**

  Implementar launch em `127.0.0.1`, espera em `/health`, request SSE com `curl -N`, timestamp do primeiro evento de token, cleanup por PID, verificação de porta/VRAM e compilação CMake CUDA em `/tmp` ou `LLAMACPP_BUILD_DIR` externo.

- [ ] **Step 4: Run test to verify it passes**

  Run: `python3 -m unittest scripts.test_bench_llamacpp -v` e `bash -n scripts/bench-llamacpp.sh`.
  Expected: testes e sintaxe passam.

- [ ] **Step 5: Commit**

  `git add scripts/bench-llamacpp.sh scripts/bench_llamacpp.py scripts/test_bench_llamacpp.py && git commit -m "feat: medir llama.cpp puro com config POM"`

### Task 3: Documentação operacional

**Files:**
- Modify: `scripts/AGENTS.md`
- Create: `/Users/jairpereira/Sites/firstmate/data/rdma-bench-llamacpp/report.md`

**Interfaces:**
- Produces CLI exata, mapa de flags, política VRAM/cleanup, formato dos artefatos, diferenças MTP/dtype/fit-memory/topologia e evidência da ksia.

- [ ] **Step 1: Atualizar documentação após validar o comportamento**

  Descrever somente o código confirmado, adicionar o script ao mapa de `scripts/` e não tocar em arquivos gerados.

- [ ] **Step 2: Verificar documentação**

  Run: `rg -n "bench-llamacpp|llama.cpp puro|VRAM|MTP|shards" scripts/AGENTS.md /Users/jairpereira/Sites/firstmate/data/rdma-bench-llamacpp/report.md`.
  Expected: CLI, limitações e evidência aparecem sem segredo.

- [ ] **Step 3: Commit**

  `git add scripts/AGENTS.md && git commit -m "docs: registrar benchmark llama.cpp puro"`

### Task 4: Validação final e entrega

**Files:**
- Verify: `scripts/bench-llamacpp.sh`, testes, diff e artefatos de relatório.

- [ ] **Step 1: Run local validation**

  `python3 -m unittest scripts.test_bench_llamacpp -v`, `bash -n scripts/bench-llamacpp.sh` e `shellcheck scripts/bench-llamacpp.sh` quando disponível.

- [ ] **Step 2: Run ksia safety gate**

  Executar o alvo real; com POM ocupado, esperar a recusa antes de qualquer launch. Se as GPUs forem liberadas por operação externa, repetir com `--n-ctx` adequado e capturar CSV/resumo/log.

- [ ] **Step 3: Review requirements and publish**

  Confirmar cleanup, diff limpo de alterações não relacionadas, commit, `git pull --rebase` e publicação na branch remota exigida conforme a autoridade da tarefa.
