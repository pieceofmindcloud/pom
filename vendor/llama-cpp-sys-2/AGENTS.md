# llama-cpp-sys-2

Este diretório contém o wrapper Rust e o script de build do binding nativo.
O código do fork `llama.cpp` fica fora deste diretório e só pode ser alterado
com autorização explícita. O wrapper compila CUDA Graphs desligado por padrão
por segurança de VRAM; `POM_CUDA_GRAPHS=1` é uma opção de build para habilitá-los.

## Preparo automático do `llama.cpp`

`llama.cpp`, neste diretório, é um **symlink versionado** para a raiz do
repositório, e a pasta que ele aponta (`/llama.cpp/`) está no `.gitignore`. O
`build.rs` detecta um symlink quebrado ou a representação textual que o Git
usa no Windows e prepara automaticamente um clone raso da branch
`pom-tensor-parallel` de `pereirajair/llama.cpp` antes de caminhar pelos
arquivos nativos. O fork continua fora do histórico deste repositório e o
script não altera arquivos dentro de um checkout já existente.

Se o download automático falhar, o erro informa o comando completo para
preparar `vendor/llama-cpp-sys-2/llama.cpp` manualmente. Também é possível usar
um checkout local sem copiá-lo:

```bash
LLAMA_SOURCE=/caminho/para/llama.cpp cargo build -p node
```

O diretório informado precisa conter `ggml`. `POM_LLAMA_CPP_REPOSITORY` e
`POM_LLAMA_CPP_REF` permitem apontar o preparo automático para outro remoto ou
branch durante testes e desenvolvimento; o padrão é o fork público e a branch
`pom-tensor-parallel`.

O `build.rs` força o reconfigure do CMake a cada execução. Isso é intencional:
o fork coleta os arquivos de `src/models` com `file(GLOB ...)`, enquanto Docker
e Cargo podem reutilizar o diretório `target/` e o `CMakeCache.txt`. Sem o
reconfigure, um arquivo de modelo presente no checkout pode continuar fora de
`libllama.a` e só aparecer como símbolo C++ indefinido no link final.

O mesmo caminho é usado em Linux, macOS e Windows. No Windows o checkout pode
materializar o symlink como um arquivo de texto; o `build.rs` o substitui pelo
checkout no caminho que o Cargo lê. Ver `docs/subsystems/build-execucao.md`,
"Release pelo GitHub Actions".

## `wrapper_common.cpp` - o que é nosso dentro deste diretório

`wrapper_common.{h,cpp}` e `wrapper_utils.h` são código **deste** repositório,
não do fork: é a ponte `extern "C"` para o que o `llama.h` público não expõe. O
`build.rs` os compila com os includes do fork (linhas ~628-635) e o bindgen lê
as declarações do `.h`, então cada função nova aparece em `llama_cpp_sys_2::`
sem mais nenhum passo.

Duas delas incluem cabeçalhos **internos** do fork (`src/llama-model.h`,
`src/llama-ext.h`), e é bom saber por quê antes de mexer:

| Função | O que responde |
|---|---|
| `llama_rs_model_placement_json` | onde cada camada ficou de verdade, lendo o buffer de cada tensor em `tensors_by_name` mais `dev_layer(il)` |
| `llama_rs_context_memory_json` | a memória do contexto por buffer (`llama_get_memory_breakdown`), cujo campo `context` é o cache KV e `host` distingue RAM de VRAM |
| `llama_rs_context_flash_attn` | o Flash Attention **resolvido**, de `get_cparams()`. O padrão do `llama.h` é `AUTO`, o llama.cpp sonda o backend ao criar o contexto e nada público devolve a decisão; `out_auto` diz se ela veio da sonda ou do pedido |
| `llama_rs_get_devices` | os handles elegíveis na mesma ordem padrão que o llama.cpp usaria |
| `llama_rs_get_device_memory` | o livre e o total atuais dos dispositivos escolhidos pelo posicionamento padrão, na mesma ordem do `common_fit_params` |
| `llama_rs_benchmark_device` | uma pontuação de um grafo GGML genérico, com cache por handle no processo; zero significa velocidade indeterminável |
| `llama_rs_backend_devices_json` | descricao e `device_id` PCI dos dispositivos registrados pelos backends Vulkan, Metal, CUDA e CPU |

`llama_rs_benchmark_device` usa somente a interface pública de backend do GGML
para executar uma multiplicação matriz-vetor pequena. O POM chama a medição no
máximo uma vez por dispositivo e só usa a ordem quando todos os resultados são
válidos e as diferenças superam o limiar de confiança. Backend sem memória,
inicialização, alocação ou execução compatível devolve zero e conserva a ordem
padrão. O probe não contém tabela de hardware nem nomes de fabricantes.

`llama_rs_fit_params` eleva temporariamente o limite de verbosidade do logger
comum para `TRACE` e o restaura ao sair, porque `common_fit_params` registra a
decisão nesses níveis e não é thread-safe. O fit MTP no POM não usa um wrapper
de sondagem seca: calcula o KV determinístico a partir da metadata GGUF e
deixa a construção do grafo para a tentativa real, que tem recuo recuperável
para a RAM. O `common/fit.cpp` do checkout local captura `free` e `total` antes
de criar seu contexto de medição genérico. Essa é uma divergência local do
fork e precisa acompanhar a branch `pom-tensor-parallel` usada no build. A
regressão da contabilidade sintética fica em `llama.cpp/tests/test-fit-memory.cpp`
e no alvo CTest `test-fit-memory`.

As funções que devolvem JSON retornam strings de `malloc` que **têm que** voltar por
`llama_rs_string_free`; do lado Rust quem faz isso é `json_from_wrapper` em
`crates/node/src/inference/llm.rs`, e é por isso que a liberação mora lá e não
em cada chamador.

O preço de usar cabeçalho interno é que uma atualização do fork pode mudar
`llama_model` sem aviso. O sintoma é erro de compilação alto e na hora, nunca
resultado errado em silêncio, e o conserto é reler o cabeçalho novo - a
alternativa, derivar a colocação de `n_gpu_layers`, mente sempre que um override
de buffer type entra. Ver `docs/subsystems/inferencia.md`, "Onde as camadas
ficaram".
