# F5 co-execução CPU/GPU medida - plano de implementação

> **Para trabalhadores agentic:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development (recomendado) ou superpowers:executing-plans para executar este plano tarefa a tarefa. Os passos usam checkbox para acompanhamento.

**Goal:** dividir misses de experts NVFP4 entre CPU e GPU pela banda medida e executar as duas lanes em paralelo, preservando a agregação por token e a seleção densa no `llama.cpp`.

**Architecture:** uma política pura valida quatro taxas de bytes por segundo e calcula a fração de misses enviada à GPU. Uma primitiva de escopo executa as lanes CPU e GPU juntas e propaga erro de qualquer uma. O bloco NVFP4 classifica experts residentes como GPU, divide somente misses novos por chave, executa as duas listas em paralelo e soma os vetores por token; sem perfil válido, mantém o caminho CUDA atual.

**Tech Stack:** Rust, `std::thread::scope`, `Arc`, `Mutex`, executor CPU NVFP4, executor CUDA residente, testes unitários do binário `pom`.

**Spec:** `docs/superpowers/specs/2026-08-24-ftk-moe-design.md` e `docs/subsystems/freetoken-moe.md`, seções de F5 e co-execução.

## Global Constraints

- Modelos densos continuam no `llama.cpp` sem alteração.
- A divisão CPU/GPU só vale para o caminho MoE NVFP4 com perfil explícito e válido.
- O perfil é medido por geometria e backend; ausência ou invalidez não escolhe CPU/GPU silenciosamente.
- Nenhum peso trafega na rede; o wire continua em f16.
- Toda compilação CUDA na ksia usa `CUDAARCHS=120`, `CUDACXX=/usr/local/cuda-12/bin/nvcc`, ccache e `CARGO_TARGET_DIR=/data/ftk-cuda-target`.
- A validação CUDA não reinicia serviço nem usa `/data/ksia-target`.

---

### Task 1: Política e primitiva de co-execução

**Files:**
- Create: `crates/node/src/inference/moe/bandwidth.rs`
- Create: `crates/node/src/inference/moe/coexecution.rs`
- Modify: `crates/node/src/inference/moe/mod.rs`
- Test: testes unitários nos dois módulos novos

**Interfaces:**
- Produces `BandwidthProfile::split_misses(&[u32]) -> Result<MissSplit, BandwidthError>`.
- Produces `run_overlap` para duas closures concorrentes e `run_measured` para dividir misses antes de iniciar as duas lanes.

- [x] **Step 1: Write the failing tests**

  Adicionar testes para: fração `transfer / (cpu + transfer)`, fallback para perfil antigo, recusa de medição ausente, divisão determinística, início concorrente com `Barrier`, propagação de erro da lane GPU e orçamento insuficiente para dois slots.

- [x] **Step 2: Run test to verify it fails**

  Run: `cargo test -p node --bin pom bandwidth coexecution`

  Expected: falha de compilação porque os módulos e tipos ainda não existem.

- [x] **Step 3: Write minimal implementation**

  Validar valores finitos e positivos, limitar a fração a `[0, 1]`, manter a ordem dos ids e usar `std::thread::scope` para que as closures só recebam slices vivos durante o forward.

- [x] **Step 4: Run test to verify it passes**

  Run: `cargo test -p node --bin pom bandwidth coexecution`

  Expected: todos os testes dos módulos passam.

- [x] **Step 5: Commit**

  `git commit -m "feat: adicionar politica de co-execucao medida"`

### Task 2: Co-execução do forward de rotas NVFP4

**Files:**
- Modify: `crates/node/src/inference/moe/format_nvfp4.rs`
- Modify: `crates/node/src/inference/moe/block.rs`
- Modify: `crates/node/src/inference/moe/runtime.rs`
- Modify: `crates/node/src/inference/moe/cuda.rs`
- Test: testes de rota e agregação NVFP4 CPU/GPU

**Interfaces:**
- `Nvfp4Executor` expõe uma lane CPU que sempre usa o cache frio e uma lane CUDA que usa leases residentes.
- O forward combinado devolve um vetor `[tokens * hidden]` e soma a contribuição de cada lane na posição do token correspondente.

- [x] **Step 1: Write the failing test**

  Criar um teste puro de agregação com duas listas de rota, uma CPU e uma GPU, que exige a soma ponderada por token e rejeita comprimentos diferentes. Criar também um teste de divisão por experts únicos que verifica que duas ocorrências do mesmo expert ficam na mesma lane.

- [x] **Step 2: Run test to verify it fails**

  Run: `cargo test -p node --bin pom nvfp4 coexecution`

  Expected: falha porque o forward combinado e a classificação de lanes ainda não existem.

- [x] **Step 3: Write minimal implementation**

  Resolver primeiro hits da LRU de device como GPU. Para misses, usar o `BandwidthProfile` somente se ele estiver presente; dividir por expert único, preservar todas as ocorrências e construir offsets independentes por token para cada lane. A lane CPU carrega/admite no cache de RAM e dequantiza; a lane GPU admite no cache residente e chama o batch CUDA. Um lane vazio retorna zeros da largura correta.

- [x] **Step 4: Run test to verify it passes**

  Run: `cargo test -p node --bin pom nvfp4 coexecution`

  Expected: testes de agregação, divisão e erro passam; os testes existentes de NVFP4 permanecem verdes.

- [x] **Step 5: Commit**

  `git commit -m "feat: executar rotas NVFP4 em CPU e GPU"`

### Task 3: Perfil, telemetria e validação

**Files:**
- Modify: `crates/node/src/inference/moe/cuda.rs`
- Modify: `crates/node/src/inference/moe/runtime.rs`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `docs/subsystems/freetoken-moe.md`

**Interfaces:**
- O perfil persistido registra as bandas necessárias e a telemetria publica contagem CPU/GPU, fração usada e motivo de não executar co-execução.

- [x] **Step 1: Test profile loading and invalidation**

  Cobrir JSON sem banda CPU, banda não finita e perfil válido que escolhe a divisão. Perfil ausente conserva o caminho CUDA inteiro e não inventa co-execução.

- [x] **Step 2: Run local dense and MoE tests**

  Run: `cargo test -p node --bin pom`.

  Expected: registrar as falhas externas existentes com saída exata; nenhum modelo denso deve importar `moe`.

- [x] **Step 3: Validate CUDA on ksia**

  Usar checkout temporário isolado, `CUDAARCHS=120`, `CUDACXX=/usr/local/cuda-12/bin/nvcc`, `CCACHE_DIR` existente e `RUSTC_WRAPPER=ccache`, com `CARGO_TARGET_DIR=/data/ftk-cuda-target`. Rodar build release, testes diferenciais NVFP4 e o teste de co-execução; não tocar no serviço.

- [ ] **Step 4: Measure and document**

  Repetir os pontos tiny, Gemma4 token e Gemma4 batch8, publicar `cpu_wall_ms`, `cuda_wall_ms`, contagens CPU/GPU, H2D/D2H e a razão `T_total / max(T_cpu, T_transfer)`. Comparar com Fase 0 sem afirmar ganho quando o critério falhar.

- [ ] **Step 5: Commit**

  `git commit -m "docs: registrar co-execucao CPU GPU medida"`
