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

## Módulos (`src/`)

- `llm/` - wrapper FFI do llama.cpp dividido por responsabilidade em captura,
  configuração, modelo, engine, batch, amostragem e testes. É a única
  superfície `unsafe` do produto. Alcança o estado da coletiva TP por
  `tp_probe` (ver abaixo). Em `llm/config.rs`, `llama_max_devices()` é só a
  capacidade dos vetores entregues ao C; quantas placas existem é
  `fit_device_count`, e é ela que posiciona a reserva de KV do contexto MTP na
  última placa **real** - ver `docs/subsystems/inferencia.md`, seção do fit.
- `mtp.rs` (+ `llama_mtp_shim.cpp`) - predição multi-token pelo bloco `nextn`;
  a lista de arquiteturas MTP aceitas é gerada em build por `build/mtp_arch.rs`.
  Quando o modelo alvo usa MoE externo metadata-only, `LlmEngine::set_moe_runtime`
  instala o mesmo callback também no contexto MTP; o acompanhante CPU do recuo
  mantém o caminho nativo porque seus pesos de experts estão presentes.
- `moe/` - executor MoE próprio (denso + esparso), com kernels CUDA em
  `moe/cuda_kernels.cu` compilados por `build.rs` sob `--features cuda`. O
  fallback CPU do router executa os experts selecionados em workers escopados
  e agrega os vetores na ordem original para manter o resultado determinístico;
  Q4_0 e Q5_K também usam a ponte CUDA heterogênea quando `cuda` está
  disponível, e um fallback CPU efetivo fica registrado na telemetria e no log
  de inicialização.
  A cobertura CUDA por dtype e os números da ksia ficam em
  `docs/subsystems/freetoken-moe.md`.
- `collective.rs` - `reduce_sum` + `ReduceBarrier` do tensor parallelism
  (caminho C). O transporte HTTP e a coordenação com o primary ficam em
  `crates/node/src/inference/tp.rs`, que **não** migrou.
- `tp_probe.rs` - ponte fina: `llm.rs` consulta o contador de reduces e o
  motivo de falha da coletiva de dentro do decode; `node` instala as funções
  reais (`tp::install_probe`) no arranque. Sem instalação, `is_active()` é
  `false` (caminho não-TP).
- `wire.rs` - formato binário do estado oculto entre peers (f16/f8, zstd). O
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
  informa fica `None` em vez de virar zero. `disk` é o volume do projeto;
  `disks` é toda partição montada. Detalhe por plataforma em
  `docs/subsystems/operacao.md` ("Monitor de carga").
- `lib.rs` - re-exporta `KvCacheType`, `DEFAULT_FIT_MEMORY_MARGIN_MB` e
  `llama_cpp_sys_2` (para `node` alcançar a FFI crua sem redeclarar a dep).

## O que ficou em `crates/node`

`inference/tp.rs` (a coletiva é rede), `inference/relay.rs` (`ByteTransport`
vive nele), `inference/executor.rs`, `models/splitter.rs` (usa `gguf` e, nos
testes, `llama_engine::llama_cpp_sys_2`), e todo o resto do pipeline
(`infer_server.rs`, `prefill.rs`, `direct.rs`, `runtime.rs`, ...).

### Visão

`llm::VisionContext` usa o `mtmd` já fornecido pelo llama.cpp para carregar o
GGUF `mmproj` separado e transformar imagens em tokens no prefill. O caminho é
opcional e é validado junto com `NodeConfig.mmproj_path`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
