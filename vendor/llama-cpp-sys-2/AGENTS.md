# llama-cpp-sys-2

Este diretório contém o wrapper Rust e o script de build do binding nativo.
O código do fork `llama.cpp` fica fora deste diretório e só pode ser alterado
com autorização explícita. O wrapper compila CUDA Graphs desligado por padrão
por segurança de VRAM; `POM_CUDA_GRAPHS=1` é uma opção de build para habilitá-los.

## O `llama.cpp` daqui não vem do git

`llama.cpp`, neste diretório, é um **symlink versionado** para a raiz do
repositório, e a pasta que ele aponta (`/llama.cpp/`) está no `.gitignore`. Ou
seja: num clone novo - um worktree recém-criado, ou o checkout que a CI faz - o
symlink aponta para o vazio e o `build.rs` morre com
`Failed to obtain entry ... NotFound` antes de compilar qualquer coisa.

O `build.rs` lê `<CARGO_MANIFEST_DIR>/llama.cpp` (linha ~409), então clonar o
fork **por cima do symlink** resolve em qualquer sistema:

```bash
rm -rf vendor/llama-cpp-sys-2/llama.cpp
git clone --depth 1 --branch pom-tensor-parallel \
  https://github.com/pereirajair/llama.cpp.git vendor/llama-cpp-sys-2/llama.cpp
```

Em um worktree local, a alternativa mais simples é apontar a pasta ignorada da
raiz para um checkout já existente, sem tocar no checkout primário:

```bash
LLAMA_SOURCE=/caminho/para/llama.cpp
test -d "$LLAMA_SOURCE/ggml"
ln -s "$LLAMA_SOURCE" llama.cpp
cargo build -p node
```

O primeiro build deve ser usado como verificação do ambiente. Se o symlink
existir mas `ggml` estiver ausente, o `build.rs` falhará antes de compilar o
Rust; corrija o caminho do checkout e rode novamente.

O `build.rs` força o reconfigure do CMake a cada execução. Isso é intencional:
o fork coleta os arquivos de `src/models` com `file(GLOB ...)`, enquanto Docker
e Cargo podem reutilizar o diretório `target/` e o `CMakeCache.txt`. Sem o
reconfigure, um arquivo de modelo presente no checkout pode continuar fora de
`libllama.a` e só aparecer como símbolo C++ indefinido no link final.

É o que os três workflows de release fazem. No Windows é obrigatório clonar
nesse caminho, e não na raiz: lá o checkout não cria symlink - materializa um
arquivo de texto com o caminho dentro -, então repor a pasta na raiz não
adiantaria. Ver `docs/subsystems/build-execucao.md`, "Release pelo GitHub
Actions".

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
| `llama_rs_mtp_context_memory_json` | sondagem seca do contexto `MTP`, sem pesos alocados, devolvendo o custo de `context` + `compute` com `device_index` por GPU |
| `llama_rs_get_devices` | os handles elegíveis na mesma ordem padrão que o llama.cpp usaria |
| `llama_rs_get_device_memory` | o livre e o total atuais dos dispositivos escolhidos pelo posicionamento padrão, na mesma ordem do `common_fit_params` |
| `llama_rs_benchmark_device` | uma pontuação de um grafo GGML genérico, com cache por handle no processo; zero significa velocidade indeterminável |

`llama_rs_benchmark_device` usa somente a interface pública de backend do GGML
para executar uma multiplicação matriz-vetor pequena. O POM chama a medição no
máximo uma vez por dispositivo e só usa a ordem quando todos os resultados são
válidos e as diferenças superam o limiar de confiança. Backend sem memória,
inicialização, alocação ou execução compatível devolve zero e conserva a ordem
padrão. O probe não contém tabela de hardware nem nomes de fabricantes.

`llama_rs_fit_params` eleva temporariamente o limite de verbosidade do logger
comum para `TRACE` e o restaura ao sair, porque `common_fit_params` registra a
decisão nesses níveis e não é thread-safe. O `common/fit.cpp` do checkout local
também captura `free` e `total` antes de criar o contexto seco: mesmo com
`no_alloc=true`, o contexto aloca o estado recorrente, que já aparece no
breakdown e não pode ser descontado do livre antes da projeção. Essa é uma
divergência local do fork e precisa acompanhar a branch `pom-tensor-parallel`
usada no build. A regressão da contabilidade sintética fica em
`llama.cpp/tests/test-fit-memory.cpp` e no alvo CTest `test-fit-memory`.

As duas devolvem JSON de `malloc` que **tem que** voltar por
`llama_rs_string_free`; do lado Rust quem faz isso é `json_from_wrapper` em
`crates/node/src/inference/llm.rs`, e é por isso que a liberação mora lá e não
em cada chamador.

O preço de usar cabeçalho interno é que uma atualização do fork pode mudar
`llama_model` sem aviso. O sintoma é erro de compilação alto e na hora, nunca
resultado errado em silêncio, e o conserto é reler o cabeçalho novo - a
alternativa, derivar a colocação de `n_gpu_layers`, mente sempre que um override
de buffer type entra. Ver `docs/subsystems/inferencia.md`, "Onde as camadas
ficaram".
