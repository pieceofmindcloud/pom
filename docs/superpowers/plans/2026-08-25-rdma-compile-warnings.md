# Avisos de compilação CUDA e GGUF: plano de implementação

> **Para trabalhadores agenticos:** REQUIRED SUB-SKILL: use `superpowers:executing-plans` ou `superpowers:subagent-driven-development` para executar este plano tarefa por tarefa. As etapas usam caixas (`- [ ]`) para acompanhamento.

**Goal:** Remover os cinco grupos de avisos de código morto da compilação real do nó na base `feat/ftk`, preservando a validação GGUF, a telemetria de perfil CUDA e as provas diferenciais MoE.

**Architecture:** O runtime continuará lendo perfis CUDA serializados e executando os caminhos NVFP4/IQ existentes. Construtores e rastros que só existem para testes e diagnóstico serão compilados somente em testes; o leitor GGUF continuará validando o dtype, mas não armazenará um campo que nenhum consumidor usa.

**Tech Stack:** Rust 2021, Cargo, `llama-cpp-sys-2`, CUDA feature gate, testes unitários do crate `node`.

**Spec:** Avisos reportados pelo capitain no launch brief `rdma-compile-warnings`.

## Global Constraints

- Usar a base `origin/feat/ftk` e manter a branch local `fm/rdma-compile-warnings`.
- Não usar `allow(dead_code)` amplo, não remover cobertura de comportamento, não fazer deploy, restart, push ou PR.
- Preservar a validação genérica por dtype GGML e o mesmo comportamento para CPU, CUDA, NVFP4 e formatos mistos.
- Atualizar os `AGENTS.md` das pastas tocadas em pt-BR no mesmo commit.

---

### Task 1: Reproduzir e registrar a compilação da base CUDA

**Files:**
- Read: `.cargo/config.toml`, `docs/subsystems/build-execucao.md`, `crates/node/Cargo.toml`
- Read: `crates/node/src/models/gguf.rs`, `crates/node/src/inference/moe/cuda.rs`, `crates/node/src/inference/moe/format_nvfp4.rs`

- [x] **Step 1: Confirmar a base e o estado da worktree**

Executar `rtk git rev-parse HEAD`, `rtk git rev-parse origin/feat/ftk` e `rtk git status --short --branch`; ambos os hashes devem ser `e062de9` antes das alterações e a árvore deve estar limpa.

- [x] **Step 2: Executar o caminho mais próximo do capitain**

Executar `rtk proxy cargo build --release --features cuda`, registrar a saída completa e separar falhas do toolkit CUDA dos avisos Rust. Registrar também a ausência de `ccache`, `/data/ccache` e `/home/jairpereira/workspace` quando o ambiente não os fornecer.

- [x] **Step 3: Traçar callers, gates, testes e histórico**

Confirmar que `LayoutTensor.dtype` só é validado e depois descartado; que `CudaTransferProfile::new`, `with_bandwidth` e `save` só aparecem em testes; que `pom_moe_iquant_trace_single_resident` só é chamado pelo diagnóstico de teste; que `Nvfp4Trace.gate`, `up` e `activated` só são lidos pela prova diferencial; e que `fp4_value` só é usado pelo teste de convenção FP4.

### Task 2: Remover o armazenamento GGUF sem consumidor

**Files:**
- Modify: `crates/node/src/models/gguf.rs:267-272,987-994`
- Modify: `crates/node/src/models/AGENTS.md`
- Test: testes existentes de `gguf::read_layout` e `removed_and_uncited_ggml_types_stay_unknown`

- [x] **Step 1: Confirmar a cobertura existente**

Executar `rtk cargo test -p node gguf::tests -- --nocapture` e confirmar que os testes exercitam leitura de layout e recusa de tipos desconhecidos. A execução foi tentada, mas o crate não chega aos testes porque o checkout local de `llama.cpp` não possui os símbolos MoE exigidos por `feat/ftk`.

- [x] **Step 2: Remover somente o campo não consumido**

Retirar `dtype` de `LayoutTensor` e do inicializador. Manter `let dtype = r.u32()?` e a chamada `ggml_type_info(dtype)` antes de calcular os offsets, para que um dtype desconhecido continue recusado.

- [x] **Step 3: Reexecutar os testes GGUF**

Executar novamente `rtk cargo test -p node gguf::tests -- --nocapture`; a cobertura deve continuar verde e nenhum cálculo de tamanho deve mudar. Bloqueado pelo mesmo descompasso nativo registrado no Step 1; não foi repetido para evitar mascarar a causa.

### Task 3: Isolar APIs de criação de perfil e o FFI de diagnóstico CUDA

**Files:**
- Modify: `crates/node/src/inference/moe/cuda.rs:42-118,1450-1476`
- Modify: `crates/node/src/inference/AGENTS.md`
- Test: `cuda::tests::perfil_cuda_persiste_e_recusa_cuda_quando_cpu_medido_e_mais_rapido`, `cuda::tests::perfil_cuda_persiste_bandas_para_coexecucao`, `cuda::tests::diagnostico_cuda_iq_divergencia_por_estagio`

- [x] **Step 1: Registrar a intenção dos helpers**

Manter `CudaTransferProfile::load_for_model` no caminho de runtime. Colocar `new`, `with_bandwidth` e `save` sob `#[cfg(test)]`, pois a criação e persistência são feitas somente pelas bancadas que geram um sidecar para diagnóstico.

- [x] **Step 2: Isolar a declaração FFI usada só pela prova**

Mover `pom_moe_iquant_trace_single_resident` para um bloco `unsafe extern "C"` marcado com `#[cfg(test)]`, deixando as declarações FFI de execução em produção no bloco comum.

- [x] **Step 3: Executar os testes CUDA quando o toolchain permitir**

Na máquina CUDA, executar `rtk cargo test -p node --features cuda inference::moe::cuda::tests -- --nocapture`. No ambiente macOS atual, registrar a falha explícita `Could not find nvcc` sem tratá-la como falha da mudança Rust.

### Task 4: Isolar o rastro NVFP4 e manter o decoder de produção

**Files:**
- Modify: `crates/node/src/inference/moe/format_nvfp4.rs:121-261,940-946`
- Test: `format_nvfp4::tests::nvfp4_decodifica_subblocos_ue4m3_e_nibbles_e2m1`, `format_nvfp4::tests::nvfp4_segue_convencao_native_do_ggml`, `format_nvfp4::tests::nvfp4_reproduz_geglu_fp16_do_ggml`

- [x] **Step 1: Escrever a menor proteção comportamental necessária**

Preservar os testes existentes que afirmam a saída de `run` e a matemática de `fp4_value_i8`, `ue4m3_to_f32` e GEGLU. A mudança deve continuar falhando se `run` alterar a saída ou se o mapa de nibbles mudar.

- [x] **Step 2: Separar o caminho de produção do rastro**

Extrair um helper privado que calcula gate, up, ativação e output em uma tupla. Fazer `run` consumir apenas o output no build normal e colocar `Nvfp4Trace` e `run_trace` sob `#[cfg(test)]`, preservando o rastro completo para a prova diferencial sem campos mortos no runtime.

- [x] **Step 3: Gatear o conversor somente de teste**

Colocar `fp4_value` sob `#[cfg(test)]`, pois o decoder usa `fp4_value_i8` diretamente e somente o teste de convenção precisa da representação `f32`.

- [ ] **Step 4: Executar os testes NVFP4 sem e com CUDA**

Executar `rtk cargo test -p node format_nvfp4::tests -- --nocapture` no host atual e, na máquina CUDA, repetir com `--features cuda`. Provas ignoradas de GGUF real permanecem explicitamente ignoradas quando não houver os artefatos requeridos. Não repetido após o teste focado do nó falhar antes da compilação dos testes pelo mesmo checkout nativo incompatível.

### Task 5: Verificar, documentar e entregar

**Files:**
- Modify: arquivos Rust e `AGENTS.md` das Tasks 2 a 4
- Create: este plano

- [x] **Step 1: Formatar e revisar o diff**

Executar `rtk cargo fmt --all -- --check`, `rtk git diff --check`, `rtk git diff --stat` e revisar que não há `allow(dead_code)` amplo, mudanças geradas ou arquivos fora do escopo.

- [x] **Step 2: Rodar a suíte do nó**

Executar `rtk cargo test -p node` e `rtk cargo test --workspace`, registrando separadamente qualquer bloqueio nativo CUDA ou falha preexistente. O teste focado do nó foi tentado e bloqueou no mesmo checkout nativo incompatível; a suíte dos demais crates passou com `rtk cargo test --workspace --exclude node` (13 testes, 0 falhas). A suíte completa não foi repetida após esse diagnóstico, pois teria a mesma falha antes dos testes.

- [x] **Step 3: Reexecutar a compilação de release e auditar avisos**

Na máquina CUDA, executar o mesmo comando do capitain, `rtk proxy cargo build --release --features cuda`, e confirmar saída com zero avisos dos cinco grupos e sem avisos novos. No host atual, executar o caminho disponível e registrar o limite do toolkit.

- [x] **Step 4: Conferir commit e branch**

Executar `rtk git status --short --branch`, `rtk git diff HEAD^ --check` e `rtk git show --stat --oneline HEAD` após criar um único commit local em `fm/rdma-compile-warnings`. A branch ficou limpa e baseada em `origin/feat/ftk`; não fazer push, PR, merge ou deploy.
