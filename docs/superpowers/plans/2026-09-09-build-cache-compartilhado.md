# Cache de compilacao compartilhado Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fazer worktrees do rdma-p2p compartilharem artefatos de compilacao por `sccache` e por um target Cargo comum, sem quebrar builds em maquinas sem a ferramenta.

**Architecture:** Um helper shell opt-in por `source` detecta `sccache` e exporta o diretorio CAS, o teto, o socket dedicado, `RUSTC_WRAPPER`, launchers CMake, `CCACHE` e `CARGO_TARGET_DIR`. O build Rust continua sendo o dono do `OUT_DIR`; o build script do wrapper declara os launchers como entradas observadas, repassa os valores CMake que ja suporta e publica o loader Vulkan no link macOS.

**Tech Stack:** Bash, Cargo, Rust build scripts, `cc`, `cmake`, `sccache`, Markdown.

**Spec:** `docs/superpowers/specs/2026-09-09-build-cache-compartilhado-design.md`

## Global Constraints

- Nao alterar o codigo C++ ou CUDA do fork `llama.cpp`.
- Sem `sccache` no `PATH`, o build deve manter o comportamento atual.
- O cache padrao deve usar `~/.cache/sccache` com teto de `80G`.
- O target compartilhado deve respeitar `CARGO_TARGET_DIR` fornecido pelo operador.
- A documentacao deve ser pt-BR e usar hifen simples.

---

### Task 1: Contrato do helper de ambiente

**Files:**
- Create: `scripts/test-build-env.sh`
- Create: `scripts/build-env.sh`
- Modify: `scripts/AGENTS.md`

**Interfaces:**
- Consumes: `PATH`, `HOME`, `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE`, `CARGO_TARGET_DIR` e wrappers ja exportados.
- Produces: ambiente de build com `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE`, `RUSTC_WRAPPER`, `CMAKE_C_COMPILER_LAUNCHER`, `CMAKE_CXX_COMPILER_LAUNCHER`, `CMAKE_CUDA_COMPILER_LAUNCHER`, `CCACHE` e `CARGO_TARGET_DIR` quando `sccache` existe.

- [x] **Step 1: Write the failing test**

  O teste executa o helper em um subshell sem o binario do `sccache` e afirma que nenhum wrapper, socket ou target novo aparece; depois executa-o com um binario falso e afirma os nove valores exportados.

- [x] **Step 2: Run test to verify it fails**

  Run: `bash scripts/test-build-env.sh`

  Expected: FAIL porque `scripts/build-env.sh` ainda nao existe.

- [x] **Step 3: Write minimal implementation**

  Criar um helper que envolva os exports em `if command -v sccache >/dev/null 2>&1`, use `${VAR:-default}`, crie somente o diretorio do cache e emita uma mensagem curta no stderr quando a ferramenta faltar.

- [x] **Step 4: Run test to verify it passes**

  Run: `bash scripts/test-build-env.sh`

  Expected: PASS nos modos com e sem `sccache`.

- [x] **Step 5: Commit**

  O commit sera feito junto com os build scripts e a documentacao depois da validacao final, para manter a branch local com um unico conjunto coerente.

### Task 2: Observabilidade dos launchers no build nativo

**Files:**
- Modify: `vendor/llama-cpp-sys-2/build.rs:619-622,874-879`
- Modify: `crates/llama-engine/build.rs:34-55`
- Modify: `vendor/llama-cpp-sys-2/AGENTS.md`
- Modify: `crates/llama-engine/AGENTS.md`

**Interfaces:**
- Consumes: `CMAKE_C_COMPILER_LAUNCHER`, `CMAKE_CXX_COMPILER_LAUNCHER`, `CMAKE_CUDA_COMPILER_LAUNCHER` e `CCACHE`.
- Produces: rerun do build script quando um launcher muda e tratamento de `CCACHE` vazio como ausencia de wrapper.

- [x] **Step 1: Write the failing test**

  Verificar por inspeccao automatizada que o build script ainda nao declara os tres launchers com `cargo:rerun-if-env-changed`.

- [x] **Step 2: Run test to verify it fails**

  Run: `rg -n 'rerun-if-env-changed=CMAKE_(C|CXX|CUDA)_COMPILER_LAUNCHER' vendor/llama-cpp-sys-2/build.rs`

  Expected: FAIL sem tres linhas.

- [x] **Step 3: Write minimal implementation**

  Declarar os tres nomes antes do loop CMake, filtrar `CCACHE` vazio antes de construir `sccache nvcc` e publicar o link do loader Vulkan em macOS quando a feature estiver ativa.

- [x] **Step 4: Run test to verify it passes**

  Run: `rg -n 'rerun-if-env-changed=CMAKE_(C|CXX|CUDA)_COMPILER_LAUNCHER' vendor/llama-cpp-sys-2/build.rs`

  Expected: PASS com as tres linhas.

- [x] **Step 5: Commit**

  Incorporar a mudanca no commit final apos o build demonstrar os launchers no comando CMake.

### Task 3: Documentacao, medicao e follow-up operacional

**Files:**
- Modify: `docs/subsystems/build-execucao.md`
- Modify: `docs/AGENTS.md`

**Interfaces:**
- Consumes: resultados reais dos builds baseline, cache frio e cache quente.
- Produces: procedimento de uso/limpeza, limites, decisao sobre prebuilt e tabela de tempos/tamanhos.

- [x] **Step 1: Write the failing test**

  Confirmar que a documentacao nao contem `scripts/build-env.sh`, `SCCACHE_CACHE_SIZE` nem a decisao sobre `LLAMA_CPP_LIB_DIR`.

- [x] **Step 2: Run test to verify it fails**

  Run: `rg -n 'build-env.sh|SCCACHE_CACHE_SIZE|LLAMA_CPP_LIB_DIR' docs/subsystems/build-execucao.md`

  Expected: FAIL sem as tres referencias.

- [x] **Step 3: Write minimal implementation**

  Documentar `source scripts/build-env.sh`, fallback, limpeza, medicao real e o follow-up dos scripts do firstmate sem implementa-lo.

- [x] **Step 4: Run test to verify it passes**

  Run: `rg -n 'build-env.sh|SCCACHE_CACHE_SIZE|LLAMA_CPP_LIB_DIR' docs/subsystems/build-execucao.md`

  Expected: PASS com uso, teto e investigacao registrados.

- [x] **Step 5: Commit**

  Fazer o commit final somente depois de `cargo build`, testes e status Git limpo.
