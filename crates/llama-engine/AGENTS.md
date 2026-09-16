# AGENTS.md - crates/llama-engine

## O que é

Todo o código Rust que fala com `llama-cpp-sys-2`, tirado de `crates/node` para
um crate próprio: editar backend/rede/roteamento de `node` não recompila nem
re-typecheck estes módulos (o wrapper `llm/` reúne 250 KB de código). `node` depende deste
crate e **re-exporta** os módulos em `main.rs`, então `crate::llm::…`,
`crate::wire::…`, `crate::gguf::…` etc. continuam resolvendo em `node` sem tocar
os ~40 arquivos que os usam.

O build nativo (o fork do `/llama.cpp`, `cmake`, as features
`cuda`/`metal`/`vulkan`) é o mesmo de antes, agora declarado aqui. Ver
`docs/subsystems/build-execucao.md`.

`build.rs` aceita `CCACHE` como comando wrapper para o `nvcc`; portanto
`CCACHE=sccache` e valido. As pontes C/C++ compiladas por `cc` tambem herdam o
`RUSTC_WRAPPER` e o helper comum de `scripts/build-env.sh` cobre os dois casos.

## Módulos (`src/`)

- `llm/` - wrapper FFI do llama.cpp dividido por responsabilidade em captura,
  configuração, modelo, engine, batch, amostragem e testes. É a única
  superfície `unsafe` do produto. Alcança o estado da coletiva TP por
  `tp_probe` (ver abaixo). Em `llm/config.rs`, `llama_max_devices()` é só a
  capacidade dos vetores entregues ao C; quantas placas existem é
  `fit_device_count`, e é ela que posiciona a reserva de KV do contexto MTP na
  última placa **real** - ver `docs/subsystems/inferencia.md`, seção do fit.
  Os snapshots nativos usados por `/health` passam pelo
  `wrapper_common.cpp`; eles devem tratar qualquer nome/descrição C nulo antes
  de chegar a `std::string` ou JSON, pois essa falha pode virar
  `strlen(nullptr)` no runtime C do Windows. Antes de cada avaliação do grafo
  com callback MoE, `begin_moe_callback` sinaliza o cancelamento do warmup e
  aguarda apenas a admissão CUDA corrente terminar; o caminho quente mantém
  prioridade sobre o preenchimento especulativo, sem ficar atrás de uma
  varredura inteira.
- `mtp.rs` (+ `llama_mtp_shim.cpp`) - predição multi-token pelo bloco `nextn`;
  a lista de arquiteturas MTP aceitas é gerada em build por `build/mtp_arch.rs`.
  O `build.rs` lê o mesmo checkout preparado pelo `llama-cpp-sys-2`, recebido
  pelo metadado Cargo `DEP_LLAMA_LLAMA_SOURCE`; há fallback para
  `LLAMA_SOURCE` e, por padrão, o caminho vendorizado
  `vendor/llama-cpp-sys-2/llama.cpp`, que é materializado como diretório real
  no Windows.
  Quando o modelo alvo usa MoE externo metadata-only, `LlmEngine::set_moe_runtime`
  instala o mesmo callback também no contexto MTP; o acompanhante CPU do recuo
  mantém o caminho nativo porque seus pesos de experts estão presentes.
- `moe/` - executor MoE próprio (denso + esparso), com kernels CUDA em
  `moe/cuda_kernels.cu` compilados por `build.rs` sob `--features cuda`. O
  fallback CPU do router executa os experts selecionados em workers escopados
  e agrega os vetores na ordem original para manter o resultado determinístico;
  Q4_0 e Q5_K também usam a ponte CUDA heterogênea quando `cuda` está
  disponível, e um fallback CPU efetivo fica registrado na telemetria e no log
  de inicialização. O cache CUDA residente é particionado por ordinal (`CUDA0`,
  `CUDA1`, ...), com orçamento e allocator próprios por placa; as rotas são
  agrupadas por shard e executadas em paralelo. O caminho heterogêneo consulta
  primeiro as especificações do mmap e só copia bytes em um miss; seu workspace
  CUDA é um pipeline persistente compartilhado pelas camadas, indexado por
  device e geometria. O agregado do cache publica os nomes dos devices e os
  bytes residentes por shard para a telemetria. A ponte CUDA é código próprio
  deste crate; o fork `vendor/llama-cpp-sys-2/llama.cpp` só fornece o callback e
  os metadados de memória. O caminho uniforme IQ3_XXS (dtype 18) também usa o
  pipeline persistente e kernels cooperativos; IQ e K compartilham buffers e
  o mesmo agrupamento por CUDA<n>, sem `cudaDeviceSynchronize` no hot path.
  Os diagnósticos CUDA são automáticos sempre que um executor MoE CUDA existe;
  `POM_MOE_CUDA_PROFILE` não é necessário nem altera a execução. Quando há
  `<modelo>.cuda-profile.json` com um `BandwidthProfile`, o bloco heterogêneo
  também divide misses entre lanes CPU e GPU e as executa em paralelo, como o
  NVFP4. Sem sidecar, mantém a política exclusivamente CUDA; coexecução só
  usa taxas persistidas e medidas para não inventar uma velocidade de CPU.
  Antes do forward heterogêneo, as specs de toda a rota são construídas em
  uma travessia do inventário da camada e os misses são carregados em uma
  thread por shard;
  cada shard serializa sua admissão pelo mutex, enquanto CUDA0/CUDA1 podem
  fazer H2D em paralelo. O upload usa diretamente a fatia do mmap, sem copiar
  cada tensor para um `Vec` intermediário. Os leases de admissão permanecem
  ativos até o forward terminar. Quando há vários misses no mesmo shard, o
  allocator faz todas as alocações e cópias em um lote e sincroniza o stream
  uma vez; eviction usa `cudaFreeAsync` para não introduzir um wait global por
  tensor. A cópia ainda termina antes de o slice temporário sair de escopo.
  O warmup percorre experts por camada em ordem expert-major, espalhando as
  admissões pelas camadas antes de esgotar o orçamento de cada GPU; isso evita
  aquecer apenas o prefixo do modelo e concentrar misses na primeira
  requisição.
  Pares repetidos de expert e token são calculados uma vez e a agregação
  conserva o peso de cada rota original.
  No IQ3_XXS a entrada é quantizada para `block_q8_1` e o decoder nativo
  `vec_dot_iq3_xxs_q8_1` do fork usa `dp4a`. As 32 lanes cobrem quatro
  superblocos por warp, cada uma processando uma fatia q8 e avançando em
  passos de quatro, sem uma árvore de shared memory por linha. Gate e up são
  calculados na mesma warp e os buffers q8 de hidden e ativação ficam
  residentes na pipeline persistente.
  O caminho heterogêneo também usa os `vec_dot_*_q8_1` canônicos para Q4/K/IQ,
  quantiza hidden e ativação em buffers persistentes e executa uma warp por
  linha; isso é importante para GGUF como Mistral-Small, que combina dtypes
  18, 22 e 23 nos papéis gate/up/down.
  Com o perfil CUDA, `cuda_device_calls` separa chamadas, experts e tempos por
  ordinal para distinguir bytes residentes de uso efetivo da placa.
  A cobertura CUDA por dtype e os números da ksia ficam em
  `docs/subsystems/freetoken-moe.md`.
- `collective.rs` - `reduce_sum` + `ReduceBarrier` do tensor parallelism
  (caminho C). O transporte HTTP e a coordenação com o primary ficam em
  `crates/node/src/inference/tp.rs`, que **não** migrou.
- `tp_probe.rs` - ponte fina: `llm.rs` consulta o contador de reduces e o
  motivo de falha da coletiva de dentro do decode; `node` instala as funções
  reais (`tp::install_probe`) no arranque. Sem instalação, `is_active()` é
  `false` (caminho não-TP).
- `wire.rs` - formato binário do estado oculto entre peers (f16/f8, zstd). A
  identidade de sessão do `BatchItem` é autoritativa ao decodificar, para que
  um relay não reative uma sessão sintética a partir de metadata antiga. O
  caminho quente não copia o tensor à toa: `encode_batch_items` escreve direto
  no buffer que vai para o fio, `split_frame` empresta o payload em vez de o
  copiar, e `peek_batch_header`/`validate_batch_bytes` leem só o cabeçalho para
  quem precisa da rota, do `batch_id` ou de uma validação. Detalhe e números em
  `docs/subsystems/inferencia.md` §2.2.
- `gguf.rs` + `multipart.rs` - leitura de cabeçalhos GGUF, incluindo modelos
  multipart. `kv_budget.rs` - orçamento de bytes do cache KV por geometria.
- `kv_cache.rs` - `KvCacheType` (era `node/src/utils/config.rs`; `Config` de
  `node` só o referencia como campo).
- `sysmon.rs` - monitor de CPU/RAM/disco/GPU. A raiz do projeto que ele mede é
  injetada por `node` (`sysmon::set_disk_root`). Cada família de GPU tem a
  própria leitura - `nvidia-smi`, sysfs do `amdgpu` + `/proc/<pid>/fdinfo`, e
  `ioreg -c AGXAccelerator` no Apple Silicon -, e o que uma plataforma não
  informa fica `None` em vez de virar zero. NVIDIA e AMD também publicam
  `power_watts` quando o driver oferece a leitura no mesmo caminho da GPU;
  Apple continua explicitamente sem esse dado. Montagens de infraestrutura do
  macOS e equivalentes (`/boot/efi`, `/boot`, `/efi`) são retiradas de `disks`,
  enquanto volumes de dados permanecem. `disk` é o volume do projeto;
  `disks` é toda partição montada relevante. Detalhe por plataforma em
  `docs/subsystems/operacao.md` ("Monitor de carga").
- `lib.rs` - re-exporta `KvCacheType`, `DEFAULT_FIT_MEMORY_MARGIN_MB` e
  `llama_cpp_sys_2` (para `node` alcançar a FFI crua sem redeclarar a dep).

## O que ficou em `crates/node`

`inference/tp.rs` (a coletiva é rede), `inference/relay.rs` (`ByteTransport`
vive nele), `inference/executor.rs`, `models/splitter.rs` (usa `gguf` e, nos
testes, `llama_engine::llama_cpp_sys_2`), e todo o resto do pipeline
(`infer_server.rs`, `prefill.rs`, `direct.rs`, `runtime.rs`, ...).

O ciclo de vida nativo deve ser encerrado na ordem inversa da criação:
contextos MTP e Vision liberam callbacks antes de llama_free, e o callback
MoE do engine principal é limpo antes de qualquer userdata compartilhado cair.
Essa regra fica implementada nos Drop de LlmEngine e MtpDrafter; alterá-la
exige revisar a fronteira FFI e testar a destruição de gerações compatíveis.

### Visão

`llm::VisionContext` usa o `mtmd` já fornecido pelo llama.cpp para carregar o
GGUF `mmproj` separado e transformar imagens em tokens no prefill. O caminho é
opcional e é validado junto com `NodeConfig.mmproj_path`.

`decode_multimodal` devolve `MultimodalDecode`, que separa `n_tokens`
(células físicas escritas no KV) de `n_past` (posição lógica seguinte). Em
M-RoPE as duas divergem por desenho do upstream, e `mrope_positions` carrega a
coordenada `[t, y, x, z]` de cada célula para que o estado oculto atravesse os
hops sem reduzir as duas contagens a uma só - ver
`decode_hidden_with_positions` e `crates/node/src/inference/AGENTS.md`.
A captura de residual acumula microlotes pelo número de células físicas: o
`mtmd` pode entregar o trecho textual e o trecho expandido da imagem em
callbacks separados, enquanto `n_past` continua sendo apenas a posição lógica.
Na captura, `ready_wait_us` mede o intervalo entre a consulta do scheduler e
a prontidão do tensor, `copy_us` mede somente a leitura D2H bulk em f32 e
`postprocess_us` mede a conversão/registro. O campo legado
`residual_capture_d2h_sync_us` é a soma dos dois primeiros e não deve ser
interpretado como largura de banda D2H; o primeiro intervalo pode conter a
sincronização do backend exigida pelo scheduler.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
