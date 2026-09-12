# Build e execução

Como o projeto compila, empacota e roda. Autoritativo é o código; este
documento reflete o estado atual do repositório. O `README.md` da raiz é
apresentação, não fonte de verdade para arquitetura.

## Workspace de crates

`Cargo.toml` na raiz define um workspace com cinco membros e
`default-members = ["crates/node"]`, então `cargo build`/`cargo test` na raiz
compilam e testam o nó. Para todos os crates públicos e o nó, `cargo test
--workspace`. `resolver = "2"`; versão e edition (0.1.0, 2021) vêm de
`[workspace.package]`; dependências compartilhadas são `anyhow`, `serde` e
`serde_json`.

- `node` - o único binário (`pom`), o produto inteiro. Depende só de
  `routing-contract` (path dep). É um binário de raiz única (`src/main.rs`)
  que declara os módulos via `#[path]`; features `cuda`/`metal` repassadas a
  `llama-cpp-sys-2`.
- `routing-contract` - contrato serializável e versionado de roteamento.
- `routing-base` - planejador determinístico aberto (não é chamado por
  ninguém hoje; ver Defeitos).
- `config`, `types` - pontos de extração, hoje scaffolding vazio (lib.rs sem
  conteúdo, nada os linka).

Nota: os `crate::config` citados dentro do `node` são o módulo próprio
`crates/node/src/utils/config.rs`, não o crate `config`.

## Build cargo

```bash
cargo build --release    # binário em target/release/pom
cargo build              # debug
cargo test               # testes do nó
```

- O perfil release em `.cargo/config.toml` usa `opt-level = 3`, `lto = "fat"`
  e `codegen-units = 1` (medido ~30x no laço quente do sampler), `debug = 0`.
  No Linux o linker é clang com `-fuse-ld=mold` para acelerar o link fat LTO;
  **o pacote `mold` é dependência de build no Linux** (`apt-get install mold`).
  `BINDGEN_EXTRA_CLANG_ARGS` aponta o bindgen do llama-cpp-sys para os headers
  do GCC no Ubuntu (o clang do apt não os traz); no macOS é inofensivo.
- **Features**: `cuda`/`metal` (default vazio) repassadas a `llama-cpp-sys-2`,
  que compila o GGML nativo com CUDA ou Metal. Ex.: `cargo build --release
  --features cuda`. São usadas pelo `Dockerfile` e pelo devupdate
  (`POM_BUILD_FEATURES`). Nos builds CUDA, CUDA Graphs ficam desligados por
  padrão para evitar a reserva adicional de VRAM que pode falhar no
  `cudaGraphLaunch` mesmo depois do warmup. Para habilitar esse recurso,
  somente quando houver VRAM livre comprovada, use
  `POM_CUDA_GRAPHS=1 cargo build --release --features cuda`.
- O sysmon consulta a descrição dos dispositivos no próprio registro de
  backends do ggml depois que ele é inicializado. A ponte comum
  `llama_rs_backend_devices_json`, em `vendor/llama-cpp-sys-2`, inclui também o
  `device_id` PCI para que a descrição Vulkan seja casada com a placa correta.
  Linux ainda usa `/usr/share/misc/pci.ids` ou `/usr/share/hwdata/pci.ids` como
  fallback; macOS usa a descrição Metal e depois `system_profiler`.
- O provider Hugging Face é HTTP nativo em Rust (`reqwest`): o build do POM não
  instala `hf`, Python ou outro runtime externo.
- O `crates/llama-engine/build.rs` lê, em modo somente leitura, o mesmo checkout
  preparado pelo `vendor/llama-cpp-sys-2/build.rs` para gerar
  `OUT_DIR/mtp_graph_architectures.rs`. O primeiro caminho é o metadado Cargo
  `DEP_LLAMA_LLAMA_SOURCE`, publicado depois que a dependência escolhe a fonte;
  há fallback para `LLAMA_SOURCE` e depois para
  `vendor/llama-cpp-sys-2/llama.cpp`. No Windows esse é o diretório real
  instalado pelo preparo anterior, não a pasta `llama.cpp` ignorada na raiz;
  Cargo executa o build script da dependência antes do build script do crate que
  gera a lista. O extrator percorre as
  verificações efetivas de `LLM_GRAPH_TYPE_DECODER_MTP` em `src/models`, cruza
  cada classe com o dispatch de `src/llama-model.cpp` e usa a tabela
  `LLM_ARCH_NAMES` de `src/llama-arch.cpp` para obter a cadeia comparada ao
  `general.architecture` do GGUF. A verificação precisa estar no escopo
  inequívoco de `build_arch_graph`; comentários não contam. Se o fork estiver
  ausente, se nenhuma verificação for encontrada, se o resultado tiver menos de
  treze entradas, ou se for inconsistente, o build falha de forma ruidosa.

### Cache compartilhado entre worktrees

Para builds locais, carregue o helper antes de chamar o Cargo:

```bash
brew install sccache             # uma vez no macOS
source scripts/build-env.sh
cargo build --release --features metal
```

O helper só ativa o cache quando encontra `sccache` no `PATH`. Sem a ferramenta,
ele deixa o ambiente inalterado e o build segue pelo caminho normal. Quando o
cache está disponível, estes são os padrões:

| Variável | Padrão | Efeito |
|---|---|---|
| `SCCACHE_DIR` | `$HOME/.cache/sccache` | cache local content-addressed compartilhado pelos worktrees |
| `SCCACHE_CACHE_SIZE` | `80G` | teto do cache de compilação |
| `SCCACHE_SERVER_UDS` | `$SCCACHE_DIR/server.sock` | socket do servidor dedicado ao cache compartilhado |
| `CARGO_TARGET_DIR` | `$HOME/.cache/pom-target` | saída do Cargo comum aos worktrees |
| `RUSTC_WRAPPER` | `sccache` | cacheia compilações Rust |
| `CMAKE_C_COMPILER_LAUNCHER` | `sccache` | cacheia C no CMake do llama.cpp |
| `CMAKE_CXX_COMPILER_LAUNCHER` | `sccache` | cacheia C++ no CMake do llama.cpp |
| `CMAKE_CUDA_COMPILER_LAUNCHER` | `sccache` | mantém o caminho CUDA coerente na ksia |
| `CCACHE` | `sccache` | envolve o `nvcc` chamado diretamente pelo `llama-engine` |

Os valores podem ser sobrescritos antes do `source`, por exemplo
`CARGO_TARGET_DIR=/tmp/pom-target source scripts/build-env.sh`. O target comum
é uma saída do Cargo, não o CAS do sccache: para toolchains, plataformas ou
combinações de features incompatíveis, use um target explícito separado. O
CAS do sccache é seguro para compilações concorrentes e evita recompilar os
mesmos objetos em worktrees diferentes. O `vendor/llama-cpp-sys-2/build.rs`
repassa as três variáveis `CMAKE_*_COMPILER_LAUNCHER` ao CMake e as observa
como entradas de rerun do Cargo.

Para acompanhar e limpar o cache, sem builds ativos:

```bash
source scripts/build-env.sh
sccache --show-stats
sccache --zero-stats
sccache --stop-server
rm -rf "$HOME/.cache/sccache"
```

O `.cargo/config.toml` não define os wrappers: isso manteria um wrapper
obrigatório em máquinas sem sccache e não permitiria selecionar explicitamente
o servidor/socket compartilhado.

### Cache no GitHub Actions

Os três workflows de release (`.github/workflows/release-{linux,macos,windows}.yml`)
resolvem o SHA atual da branch `pom-tensor-parallel` antes do build. O checkout
do fork fica em `.ci-cache/llama.cpp`, fora do symlink versionado, e é guardado
por `actions/cache` com a chave:

```text
llama-source-v1-<sistema>-<arquitetura>-<sha-do-fork>
```

Depois do restore, o passo de preparo confirma com `git rev-parse` que o
checkout tem exatamente esse SHA. Em cache miss, ele faz o clone raso e recusa
continuar se a branch tiver avançado entre a resolução e o clone. O build
recebe `LLAMA_SOURCE` para usar esse diretório; assim o `git clean` do
`actions/checkout` não transforma o clone vendorizado em uma dependência
implícita do estado anterior do runner.

Os workflows também ativam `mozilla-actions/sccache-action` com o backend do
cache do GitHub. `RUSTC_WRAPPER`, os três launchers do CMake e `CCACHE` apontam
para `sccache`, então o mesmo cache content-addressed cobre as unidades Rust,
C/C++ do llama.cpp e a ponte CUDA quando houver CUDA. No Windows, onde o runner
é efêmero, `Swatinem/rust-cache` continua cobrindo `~/.cargo` e `target/`; sua
chave adicional inclui o SHA do fork e `hashFiles('**/Cargo.lock')`. Linux e
macOS usam runners self-hosted persistentes e mantêm o target local, enquanto o
`sccache` cobre recompilações entre revisões e worktrees.

Essa combinação invalida o checkout quando o fork muda, invalida o cache do
Cargo quando o lockfile muda e deixa o próprio `sccache` separar compiladores,
flags e conteúdo-fonte. O primeiro build de uma chave continua pagando clone,
configure e compilação; nos seguintes, a expectativa é evitar os minutos das
unidades C/C++ e Rust inalteradas. O link final, especialmente o LTO de
release, continua sendo executado e por isso o ganho não é o tempo total do
build. A estratégia foi validada offline por inspeção dos workflows, do
`vendor/llama-cpp-sys-2/build.rs` e das chaves; nenhuma execução do workflow foi
disparada.

### Por que não há `LLAMA_CPP_LIB_DIR`

O build nativo do llama.cpp não é apenas um arquivo `.a` isolado. O
`vendor/llama-cpp-sys-2/build.rs` configura o CMake, gera bindings e metadados
no `OUT_DIR` e liga as bibliotecas produzidas para a combinação exata de
checkout, plataforma, features e compilador. Um diretório apontado por
`LLAMA_CPP_LIB_DIR` não carregaria prova suficiente de que esses itens são
compatíveis; reutilizá-lo poderia produzir um binário silenciosamente ligado
à revisão ou às flags erradas. Por isso esta mudança não adiciona um caminho
prebuilt: o target comum reduz a cópia do resultado para o mesmo commit e o
sccache reutiliza cada compilação de forma verificável pelas entradas do CAS.

### Medição no macOS

Medição realizada em 2026-09-09, Apple Silicon, perfil release e
`--features metal`, com o checkout de `llama.cpp` local. O baseline foi um
worktree limpo sem wrappers, com `GGML_CCACHE=OFF`; os números depois do helper
usam o target comum e o cache em `~/.cache/sccache`.

| Cenário | Tempo | Saída do Cargo | Cache |
|---|---:|---|---|
| Antes, build limpo | 6m39s | `target/`: 1,4G no worktree | sem requests sccache |
| Depois, cache frio | 6m33s | `$HOME/.cache/pom-target`: 1,4G; `target/` local não é usado | 732 misses, 224 MiB |
| Depois, cache quente após `cargo clean` | 6m39s | `$HOME/.cache/pom-target`: 1,4G; `target/` local não é usado | 728 hits acumulados, 42,47% dos requests |

O link fat LTO continua sendo a maior parcela do tempo e não é cacheado pelo
sccache; o ganho medido está nas compilações Rust/C/C++. Em um segundo worktree
no mesmo commit, o target comum também permite que o Cargo encontre o binário
já produzido, sem criar outra árvore de 1,4G. O cache frio registrou quatro
erros do caminho C/C++ do sccache, mas o compilador retomou e o binário final
passou; os warnings existentes do llama.cpp continuam aparecendo.

Para validar Vulkan neste macOS, o host precisou de
`vulkan-headers`, `vulkan-loader`, `shaderc`, `molten-vk`, `spirv-headers` e
`glslang`. Como o Homebrew não expõe um SDK monolítico, o CMake recebeu os
paths explicitamente e os headers SPIR-V foram adicionados ao include path:

```bash
brew install vulkan-headers vulkan-loader shaderc molten-vk spirv-headers glslang
source scripts/build-env.sh
export CMAKE_Vulkan_INCLUDE_DIR=/opt/homebrew/opt/vulkan-headers/include
export CMAKE_Vulkan_LIBRARY=/opt/homebrew/opt/vulkan-loader/lib/libvulkan.dylib
export CMAKE_PREFIX_PATH=/opt/homebrew/opt/vulkan-loader:/opt/homebrew/opt/vulkan-headers:/opt/homebrew/opt/shaderc:/opt/homebrew/opt/molten-vk:/opt/homebrew/opt/spirv-headers:/opt/homebrew/opt/glslang
export CPATH=/opt/homebrew/opt/spirv-headers/include
cargo build --features vulkan
```

Essa validação passou no perfil dev em 13,35s. Sem essas dependências, a
tentativa falha no configure com `Vulkan_LIBRARY`, `Vulkan_INCLUDE_DIR` ou
`glslc` ausentes; isso é uma pré-condição do host, não uma alteração deste
wiring.

Follow-up para o firstmate, não implementado nesta branch: `bin/fm-spawn.sh`
deve exportar estas variáveis para os crewmates de rdma-p2p, e
`bin/fm-teardown.sh` deve recuperar o `target/` do worktree no teardown. Hoje o
teardown deixa 25-65 GB para trás.

### Validação CUDA isolada na ksia

A validação CUDA não deve escrever no checkout servido nem no diretório que o
script de implantação usa. O checkout de validação é `/data/ftk-cuda-src`, com
`llama.cpp` apontando para `/data/ftk-cuda-llama`, e o target separado é
`/data/ftk-cuda-target`. Na ksia, `nvcc` existe em
`/usr/local/cuda-12/bin`, mas não entra no `PATH` de uma sessão SSH não
interativa. A invocação reproduzível é:

```bash
ssh ksia 'bash -lc "cd /data/ftk-cuda-src && \
  export PATH=/usr/local/cuda-12/bin:\$PATH && \
  export CUDAARCHS=120 && \
  export CUDACXX=/usr/local/cuda-12/bin/nvcc && \
  export CCACHE=ccache && \
  CARGO_TARGET_DIR=/data/ftk-cuda-target \
  cargo build --release --features vulkan,cuda"'
```

O `PATH` é necessário porque o `build.rs` executa o programa `nvcc`
diretamente; `CUDACXX` também deve ser definido para os sub-builds que
consumirem a convenção CMake. `CUDAARCHS=120` limita o kernel MoE à GPU da
ksia. `CCACHE=ccache` faz o `build.rs` executar `ccache nvcc`; o target Rust
continua em `/data/ftk-cuda-target`. `/data/ksia-target` fica reservado ao
binário servido e nunca é usado para validar uma revisão.

## Identidade do produto (`brand.json`)

`brand.json`, na raiz, é a fonte única do nome, do slogan e dos arquivos de
logo. Renomear o produto é editar esse arquivo - e, só para o executável, o
`[[bin]]` de `crates/node/Cargo.toml`, que o cargo exige literal.

- **Binário**: embute o arquivo em tempo de compilação (`utils/brand.rs`,
  `include_str!`), então o nome viaja dentro do executável e não há arquivo para
  entregar ao lado dele. Tudo o que carregava o nome antigo passou a derivar do
  `slug`: `brand::config_filename()` (`pom.json`), `brand::env_var("HOME")`
  (`POM_HOME`), as chaves de identidade (`.pom-node.key`, `.pom.key`),
  `brand::gguf_key("start_layer")` (`pom.start_layer`) e `brand::dns_record()`
  (`_pom`, o TXT do pkarr). Há teste que quebra se algum deles parar de derivar.
- **Interface**: `apps/frontend/scripts/build-brand.mjs` roda antes de
  dev/build/test (pre-hooks do npm), copia o manifesto para `src/brand/` e as
  imagens para `public/brand/` - as duas pastas são geradas e ignoradas pelo
  git. `src/lib/brand.ts` expõe `brand` e `storageKey()`, que é o que dá o
  prefixo das chaves de `localStorage` (`pom.theme`, `pom.locale`, ...).
  O `index.html` não pode importar nada, então o plugin `brandHtml` do
  `vite.config.ts` substitui `%BRAND_NAME%`, `%BRAND_SLUG%` e os ícones no
  build - é o que impede a aba do navegador de anunciar um nome velho.
- **Imagens**: `assets/brand/` guarda o logo completo (`pom-logo.png`, o
  lockup com letreiro e slogan), a arte do ícone (`pom-icon-src.png`, a origem),
  a marca quadrada em 1024 (`pom-mark.png`), os PNGs de 16 a 512 e o
  `favicon.ico`. As cópias servidas pela interface levam **hash de conteúdo no
  nome** (`pom-icon-512.f95e6ea3.png`), porque `webapp.rs` serve tudo que não é
  `index.html` como `immutable` por um ano - com nome fixo, trocar a logo não
  chegaria a quem já abriu a tela.
- **O lockup tem duas artes, uma por tema**: `logo.light` e `logo.dark` no
  `brand.json`, e por isso ele não precisa de placa atrás - cada tema recebe a
  arte desenhada para o fundo dele. A escura é **derivada** da clara por
  `scripts/derive-dark-logo.py`: o letreiro (azul-petróleo chapado, feito para
  fundo claro) tem a luminância espelhada, e a ilustração do cérebro não é
  tocada, porque recolori-la achatava o volume. Quando existir uma arte escura
  desenhada de verdade, sobrescrever `assets/brand/pom-logo-dark.png` é a
  mudança inteira - e o script pode ser apagado.

- **Bandeiras do seletor de idioma**: `assets/brand/flags/{us,br}.svg`,
  mapeadas por locale em `brand.json` (`flags`). São SVG porque o alvo é um
  ícone de 24x16 na barra lateral e um PNG pequeno esticado fica sujo; trocar
  por outra arte é substituir o arquivo e rodar o build da interface.

### Ícone do aplicativo, por plataforma

Um executável Unix não carrega ícone; cada sistema resolve isso de um jeito, e
os três saem do mesmo `brand.json` (`app_icons`):

| Sistema | Arquivo | Como entra |
|---|---|---|
| Windows | `assets/brand/pom.ico` | recurso **dentro do `.exe`**, gravado por `crates/node/build.rs` (`embed_windows_icon`, via `winresource`). O `cfg(windows)` ali é do host: o release compila o alvo Windows num runner Windows. Cruzar de Linux sai sem ícone, em silêncio, em vez de quebrar o build. O binário usa `windows_subsystem = "windows"`; `crates/node/src/windows_status.rs` abre `/admin-ui/chat` no WebView2 e mantém a janela acessível pela área de notificação. |
| macOS | `assets/brand/pom.icns` + `assets/brand/macos/pom-status-template{,@2x,@3x}.png` | quem mostra o ícone é o bundle, não o binário: `scripts/make-macos-app.sh` monta `POM.app` em volta do executável já compilado (Info.plist, `LSUIElement`, ícone do Finder em `Resources/` e a template image do `NSStatusItem`, gerada por `scripts/derive-macos-status-icon.py`). O binário liga a ponte AppKit/WebKit só nesse alvo - `cc` é build-dependency apenas para host macOS; o release roda o script e leva o `.app` no pacote do macOS. |
| Linux | `assets/brand/linux/hicolor/**` + `pom.desktop` | árvore `hicolor` de 16 a 512 e a entrada `.desktop`, empacotadas em `share/` no tarball. Instalar é copiar para `~/.local/share/{icons,applications}`. |

**O `.app` tem que achar a interface.** Dentro do bundle o binário fica em
`Contents/MacOS/`, e dali `project_root()` cairia no cwd de quem abriu (o
Finder abre em `/`), com a tela respondendo 500. `config::bundled_ui_dist()`
resolve `Contents/Resources/apps/frontend/dist` antes de tentar o project root;
o script de release copia o `dist` para lá.

Renomear o produto de novo: edite `brand.json`, troque o `[[bin]]`, gere as
imagens novas em `assets/brand/` e rode `scripts/migrar-para-pom.sh` (ou o
equivalente) em cada máquina, porque config e chaves já gravadas em disco
continuam com o nome anterior.

## Carimbo de commit (crates/node/build.rs)

O binário carimba o commit de que foi construído, exposto em `/api/ui/node`
(`build_commit`) - é como se descobre um peer com binário velho sem shell na
máquina. Precedência:

1. Variável `POM_BUILD_COMMIT` (é como o commit entra num container: o
   `.dockerignore` corta `.git/` do contexto e a imagem builder não tem `git`,
   então ler dentro dela falha e carimbaria "desconhecido").
2. `git rev-parse --short HEAD` + sufixo `-sujo` se `git status --porcelain`
   mostrar mudanças.
3. `desconhecido` se não der para ler nada.

`rerun-if-env-changed` na variável e `rerun-if-changed` em `.git/HEAD` e
`.git/index` impedem o carimbo de congelar no primeiro build.

## Atualizar um nó pela própria interface (só desenvolvimento)

`backend/devupdate.rs`. O card de cada peer ganha "Atualizar e reiniciar" quando
**aquele processo** subiu com `POM_DEV_UPDATE=1` - é variável de ambiente,
por processo, porque a config é reescrita pela própria interface e um campo lá
deixaria o portão ligável pela rede. `POM_BUILD_FEATURES=cuda` (ou `metal`)
diz com que features a máquina compila; sem isso uma máquina com GPU perde o
offload no primeiro clique.

- **Agir sobre outro peer é repasse**: o painel só fala com o próprio nó
  (caminhos relativos em `apps/frontend/src/lib/api.ts`), então
  `POST /api/ui/dev/update` com `{"node_id": "..."}` faz o nó local encaminhar.
  O portão é verificado **no destino**, nunca em quem repassa.
- **Nenhum comando destrutivo**: a sequência recusa branch fora do `main`, HEAD
  destacado e árvore suja, e só então faz `git pull --ff-only` e
  `cargo build --release`. Não há `reset`, `checkout`, `clean` nem `stash`, então
  numa máquina onde alguém está codificando o botão falha e explica. Falhou em
  qualquer passo, ninguém reinicia.
- **Cada nó reinicia a si mesmo** (`/api/ui/dev/restart`): nenhum consegue
  respawnar o irmão, porque não conhece o `POM_CONFIG` nem o
  `POM_NODE_KEY` dele. Irmão é peer com o mesmo hostname **e** o mesmo host
  de rede (`same_machine`) - hostname sozinho casaria duas VMs chamadas `ubuntu`;
  peer com `addr` `0.0.0.0`/`::` é descartado, porque esse endereço não reescrito
  aponta para o próprio nó.
- `devupdate` **não** escreve a anotação de última fatia: esse ciclo de vida é do
  `runtime.rs` (ver `implantacao.md`), e uma escrita aqui só poderia mentir.

## Docker

- **`Dockerfile`**: dois estágios. Builder
  `nvidia/cuda:13.0.3-devel-ubuntu24.04` instala rustup + node, faz
  `npm run build` em `apps/frontend` e `cargo build --release --features cuda`
  com cache mounts (registry/git/target). Runtime é a base CUDA 13.0.3 com
  `LD_LIBRARY_PATH` incluindo `compat/` e `POM_HOME=/app`. O `ARG
  POM_BUILD_COMMIT` recebe o commit do host.
- O cache de `target/` também conserva o `CMakeCache.txt` e a árvore nativa do
  `llama.cpp`. Como o fork coleta `src/models/*.cpp` por `file(GLOB ...)` sem
  `CONFIGURE_DEPENDS`, uma árvore gerada antes de um arquivo de modelo existir
  produz `libllama.a` sem o objeto correspondente e falha só no link Rust
  (`undefined symbol: vtable for llama_model_*`). O `build.rs` do
  `llama-cpp-sys-2` chama `always_configure(true)`, mas isso só age quando o
  Cargo reexecuta o build script - com o cache mount persistente e o `COPY . .`
  do Docker ele não reexecuta, e a `libllama.a` velha é relinkada a cada build.
  Por isso o `Dockerfile` chaveia o `id` do cache mount de `/build/target` em
  `pom-target-${LLAMA_CPP_COMMIT}`: cada checkout do fork ganha uma árvore
  nativa limpa. Ids antigos ficam ocupando espaço até um
  `docker builder prune` (`--filter type=exec.cachemount` limpa só os mounts).
  `docker compose build --no-cache` **não** resolve: cache mounts sobrevivem ao
  `--no-cache`.
- **`scripts/build-docker.sh`**: leia o commit **no host** (`git rev-parse
  --short HEAD`, sufixo `-sujo` se a árvore tem mudanças), exporta
  `POM_BUILD_COMMIT` e `LLAMA_CPP_COMMIT` (`git -C llama.cpp rev-parse --short
  HEAD`, chaveia o cache mount acima), roda `npm run build` e
  `docker compose build`.
  Construa a imagem por este script; `docker compose build` na mão carimba
  "desconhecido" em todo container, de propósito.
- **`scripts/run-docker.sh`**: `docker compose down` + `docker compose up -d`
  (não recria a imagem).
- **`docker-compose.yml`**: cluster local de 4 serviços (`node-gpu`,
  `node-gpu-2`, `node-cpu-1`, `node-cpu-2`) com `network_mode: host`; estado
  em `tests/fixtures/<nome>`, modelos montados de `./models`, GPU só via
  `deploy.resources` (exige NVIDIA Container Toolkit). Cada serviço define
  `POM_HOME=/data` e `POM_UI_DIST=/app/apps/frontend/dist` (a UI
  não ficaria sob o diretório de dados montado). As portas de cada nó vêm da
  config, não do CLI - daí o gerador abaixo. Os nomes de fixture do compose
  (`node-gpu`, `node-cpu-1`...) e os do gerador (`node-<N>`) são de ambientes
  diferentes de propósito e não precisam se encontrar - ver `scripts/AGENTS.md`.
- **`scripts/gen-node-configs.py`**: gera um `pom.json` por nó em
  `tests/fixtures/node-<N>` (nó 1 = principal, guarda `connection_key` e
  `api_keys`; demais = node com bootstrap para `127.0.0.1:42777`). Recusa
  gerar sem `connection_key` (herda da config base `--base`, nunca gera a
  chave), `--force` sobrescreve e `--advertise-addr` atualiza apenas o
  endereço anunciado mesmo em configs existentes. Se o valor vier como
  `HOST:PORT`, `HOST` é compartilhado e cada config conserva seu `p2p_port`;
  não se deve fazer quatro processos anunciarem o mesmo socket. As portas seguem a série:
  gateway 8080+, p2p 42777+, inferência 8181+ (nó 1 herda a histórica 8092).
- `.dockerignore` corta `target/`, `.git/`, `venv/`, `models/`, `*.gguf`,
  `*.key`, `pom.json` e `docs/` do contexto de build - por isso o commit
  chega por variável e a config não entra na imagem.

## Python legado e venv

O runtime suportado é **Rust** (`crates/node/src/inference/infer_server.rs` é
o servidor de inferência; `crates/node/src/models/splitter.rs` o fatiador).
Os scripts `inference_server.py` e `split_model.py` vivem em
`old/python-inference/` e **não são invocados pelo binário**. Um venv em
`./venv` (python 3.9+ do macOS ou o do Ubuntu) é útil só para o operador que
quiser rodar o legado via `old/scripts/run-inf.sh`; a tabela de tipos ggml de
`gguf.rs` é conferida contra a `GGML_TRUTH` no próprio arquivo (teste de
build), não contra o venv. O único script que o build Rust consulta
externamente é o `gen-node-configs.py` (não é do build, é de operação).

## Scripts de cluster

Os lançadores (`run-p1.sh`, `run-p2.sh`, `request.sh`) **não estão na raiz**:
vivem em `old/scripts/` e são referência de operador (o binário não os usa).
O procedimento de subida está em `operacao.md`.

- `old/scripts/run-p1.sh` - peer 1 (MacBook, Metal): camadas 0-15, `--port
  42777`, gateway em 8080, `--inference-port 8181`, `--n-gpu-layers 99`,
  `--no-thinking`. Aceita `MODEL`, `N_CTX`, `N_BATCH`, `MAX_NEW_TOKENS` por
  variável de ambiente.
- `old/scripts/run-p2.sh` - peer 2 (ksia): camadas 16-31, bootstrap para o
  peer 1, `--inference-port 8182`. Faz `cd "$(dirname "$0")"` e assume o
  binário relativo ao cwd.
- `old/scripts/request.sh` - `POST /api/generate` para o gateway com o prompt
  como primeiro argumento (`GATEWAY`/`PORT` sobrescrevem o host/porta).
- `scripts/bench-rotas.sh` - bateria de medicao da geracao que compara quatro
  origens e cinco prompts, grava um CSV estavel com os contadores e a taxa de
  aceitacao MTP por requisicao ao final das colunas existentes e imprime
  medianas por celula. `POM_BENCH_API_KEY` deve conter a API key de inferência
  ativa e é enviada em cada chamada, sem ser impressa nem gravada no CSV. O
  modelo e omitido por padrao para usar o modelo ativo da rede;
  `POM_BENCH_MODEL` fixa um modelo somente quando desejado. Para a origem da
  salem, exporte `SALEM_PASS` sem gravar a senha em arquivo.

O detector conservador de ciclos vem ligado por padrão em cada processo. Para
manter o comportamento anterior, o operador pode iniciar um nó com
`--no-cycle-detection`; os limites da janela e o número de repetições são
parâmetros de runtime persistidos pela tela de implantação.

Não existe `pom-ksia.sh` em lugar nenhum do repositório (nem em histórico de
git); a referência a ele no plano da etapa não corresponde a artefato
versionado. O papel de "subir a ksia" hoje é do `old/scripts/run-p2.sh`.

## Variáveis de ambiente importantes

| Variável | Efeito |
|---|---|
| `POM_HOME` | Resolução de `project_root()` em primeiro lugar: config, `models/`, `venv/`, UI. Sem ela: cwd que parece o projeto, depois subir do diretório do executável. Dois nós na mesma máquina precisam de casas distintas (identidade `node_id` também mora no project root). |
| `POM_CONFIG` | Caminho do `pom.json` (padrão `<project_root>/pom.json`). |
| `POM_DB_DIR` | Diretório de `pom.db` e seus arquivos WAL/SHM. Se ausente ou vazio, usa `<project_root>`; o diretório é criado no arranque. |
| `POM_UI_DIST` | Onde `webapp.rs` lê o bundle da UI (padrão `<project_root>/apps/frontend/dist`). A imagem Docker a seta explicitamente. |
| `POM_SLOT_SAVE_PATH` | Diretório de snapshots de KV cache (`--slot-save-path` do llama-server). Vence o campo `slot_save_path` do `pom.json`; a flag `--slot-save-path` vence a env var. Por processo, não persistida. Ausente desliga `POST /v1/session/slot` e `/admin/slots`. Ver `inferencia.md` §3.1.1. |
| `POM_SLOT_PERSIST_PREFIX_CACHE` | `1`/`true`/`yes`/`on` liga o cache de prefixo persistente (gateway grava/restaura o KV retido entre reinícios). Vence `slot_persist_prefix_cache` do `pom.json`; exige `slot_save_path` em todos os hops. Desligado por padrão. Ver `inferencia.md` §3.1.1. |
| `POM_SLOT_AUTO_TTL_SECS` | TTL de um snapshot automático de prefixo sem uso, em segundos (padrão 7 dias). Vence `slot_auto_ttl_secs` do `pom.json`. Lido no arranque do gateway. |
| `POM_DEV_AUTO_ACCEPT_TRANSFERS` | `=1` faz o nó **receptor** aceitar toda oferta de transferência sem clique. Só desenvolvimento; lida uma vez no arranque. O remetente não pode pedi-la. |
| `POM_DEV_UPDATE` | `=1` liga o portão de "Atualizar e reiniciar" no card do peer (`devupdate.rs`). Por processo, nunca na config. |
| `POM_BUILD_FEATURES` | Features com que a máquina compila no devupdate (`cuda`/`metal`); sem ela a ksia perde a GPU no primeiro clique. |
| `POM_CUDA_GRAPHS` | `=1`, `true`, `yes` ou `on` habilita CUDA Graphs no build CUDA. O padrão é desligado para reduzir o risco de OOM no lançamento do grafo. É uma opção de compilação, então exige reconstruir o binário. |
| `POM_BUILD_COMMIT` | Commit carimbado pelo `build.rs` (usado pelo build-docker.sh; `.dockerignore` corta `.git/`). |
| `LLAMA_SOURCE` | Checkout local de `llama.cpp` já preparado; precisa conter `ggml`, é normalizado para caminho absoluto e evita copiar o fork para o worktree. |
| `POM_LLAMA_CPP_REPOSITORY` / `POM_LLAMA_CPP_REF` | Remoto e branch usados pela preparação automática quando o fork está ausente. Os padrões são o fork público e `pom-tensor-parallel`. |
| `POM_LLAMA_CPP_AUTOCLONE` | Use `=0` para impedir o download automático e receber somente a mensagem com o procedimento manual. |
| `VITE_API_TARGET` | Alvo do proxy do dev server da UI (padrão `http://127.0.0.1:8080`). |

## Release pelo GitHub Actions

Três workflows, um por sistema, todos só manuais (`workflow_dispatch`):

| Arquivo | Alvo | Roda em | Entradas |
|---|---|---|---|
| `.github/workflows/release-linux.yml` | Linux x86_64 (CPU, e CUDA opcional) | runner `ksia` | `version`, `cuda`, `prerelease` |
| `.github/workflows/release-macos.yml` | macOS arm64, Metal | runner `macbook` | `version`, `prerelease` |
| `.github/workflows/release-windows.yml` | Windows x86_64, CPU | hospedado (`windows-2022`) | `version`, `prerelease` |

**São separados para poder rodar sozinhos.** Publicar só o macOS não deve
custar um build de Windows, e um alvo que quebra não leva os outros junto. Cada
workflow publica o próprio pacote **na mesma tag**: o primeiro a rodar cria a
release, os seguintes anexam o arquivo deles. Rodar de novo só o de Linux não
apaga o pacote de macOS que já estava lá.

### O fork do llama.cpp é o passo que não pode faltar

`vendor/llama-cpp-sys-2/llama.cpp` é um **symlink versionado** para a raiz do
repositório, e a pasta que ele aponta (`/llama.cpp/`) está no `.gitignore`. Num
checkout limpo, o próprio `build.rs` do `llama-cpp-sys-2` detecta o symlink
quebrado e clona automaticamente o fork público
`pereirajair/llama.cpp`, branch `pom-tensor-parallel`, antes de caminhar pelos
arquivos nativos. Assim CI, releases e `cargo build` local seguem o mesmo
caminho, sem fixar um commit do fork.

Essa branch também carrega a correção local do `common_fit_params`: a memória
livre é capturada antes do contexto seco, pois os buffers de estado recorrente
são reais mesmo com `no_alloc=true`. O projeto não versiona o conteúdo apontado
pelo symlink, então atualizar o fork é parte da atualização da dependência.

No Windows o checkout pode materializar o symlink como um arquivo de texto ou
um reparse point. O `build.rs` não segue essa entrada: remove o placeholder e
instala o clone diretamente no caminho que o Cargo lê
(`<manifest_dir>/llama.cpp`). Isso deixa um diretório real para o `WalkDir` e
evita `PermissionDenied` no entry de profundidade zero. Em Unix, o clone é
instalado no destino relativo do symlink, preservando o layout versionado. Se
o clone não puder ser baixado, o erro lista o comando completo para a
preparação manual, em vez de deixar o `NotFound` cru do `walkdir`.

Antes do build nativo, `release-windows.yml` compila e executa
`vendor/llama-cpp-sys-2/src/build_support.rs` com o `rustc` do runner. Esse
smoke test confirma no próprio Windows que um symlink versionado não vira a
raiz percorrida pelo `WalkDir` e que o destino Unix continua preservado.

### Preparar um worktree local

O symlink versionado em `vendor/llama-cpp-sys-2/llama.cpp` resolve para
`<worktree>/llama.cpp`, mas o conteúdo da raiz é ignorado pelo git. O primeiro
`cargo build -p node` já dispara o preparo automático. Para reutilizar um
checkout local do fork sem copiá-lo para o worktree, informe `LLAMA_SOURCE`:

```bash
LLAMA_SOURCE=/caminho/para/llama.cpp
test -d "$LLAMA_SOURCE/ggml"
cargo build -p node
```

Se `llama.cpp` já existir na raiz do worktree, confirme que ele é o checkout
do fork e que contém `ggml`; um diretório existente sem `ggml` não é sobrescrito
automaticamente. `POM_LLAMA_CPP_REPOSITORY` e `POM_LLAMA_CPP_REF` podem apontar
o preparo automático para outro remoto ou branch durante testes, sem tornar
essa referência uma dependência fixa do projeto.

### A interface tem que ir no pacote

A UI é lida do disco em tempo de execução (`config::ui_dist_dir`), não embutida
no binário: sem `apps/frontend/dist` no pacote, o nó sobe e responde 500 na
tela.

Nos dois workflows self-hosted ela é compilada no próprio job, porque lá não há
multiplicador de minuto e separá-la só acrescentaria uma ida e volta de artefato
entre dois jobs que rodariam na mesma máquina de qualquer jeito - o runner
atende um job por vez. No de Windows, que é hospedado, o job `ui` continua em
`ubuntu-latest`: ali o minuto de Windows conta 2x e compilar a mesma interface
lá custaria o dobro pelo mesmo `dist`.

Os dois workflows self-hosted fixam o Node com `actions/setup-node` em vez de
usar o do sistema. O runner grava um **retrato do PATH de quem o configurou**, e
esse retrato envelhece: a ksia traz Node 18, e no macOS o Node vem do nvm, cujo
caminho muda quando a versão padrão muda.

### Runners self-hosted

Linux e macOS rodam em máquinas próprias, então **não consomem cota**:

| Runner | Máquina | Labels | Serviço |
|---|---|---|---|
| `ksia` | Ubuntu 24.04 x86_64, 8 núcleos, 46 GB | `self-hosted, Linux, X64, ksia, cuda` | systemd, usuário `github-runner` |
| `macbook` | macOS arm64 | `self-hosted, macOS, ARM64, macbook, metal` | LaunchAgent, no login do usuário |

**O runner da ksia roda como `github-runner`, sem sudo, e isso é deliberado.**
Um runner self-hosted executa o que estiver no workflow de qualquer branch do
repositório; como root, quem conseguisse abrir um branch aqui teria root na
máquina. Sem sudo, o estrago fica limitado ao usuário do runner. Pelo mesmo
motivo as dependências são provisionadas **uma vez na máquina** e o workflow só
confere se estão lá, em vez de instalar a cada execução.

No macOS o runner roda na conta do próprio usuário - inerente a self-hosted numa
máquina pessoal: um workflow tem o mesmo acesso que o usuário logado. Vale
enquanto quem pode empurrar branch neste repositório é confiável.

Comandos de manutenção:

```bash
# ksia
ssh root@ksia '/home/github-runner/actions-runner/svc.sh status'
ssh root@ksia '/home/github-runner/actions-runner/svc.sh stop'
# macbook
~/actions-runner/svc.sh status
```

Se a máquina estiver desligada, o job **fica na fila** em vez de falhar.

Para remover um runner: `./svc.sh uninstall` e depois `./config.sh remove
--token <token de remoção>` (`gh api --method POST
/repos/pereirajair/rdma-p2p/actions/runners/remove-token`).

### Cota (só o workflow de Windows)

Actions é gratuito e ilimitado em repositório **público**. Neste, que é privado,
vale a cota mensal do plano (2.000 minutos no Free) com multiplicador por
sistema: Linux 1x, Windows 2x, macOS 10x. Depois que Linux e macOS foram para
runners próprios, só o de Windows consome cota - e é o único que ainda vale a
pena vigiar.

Um build limpo do zero - incluindo o llama.cpp nativo - foi medido em **3m33s**
num Apple Silicon; um `target/` deste projeto mede **~9 GB**, o que importa na
ksia, onde o disco é o recurso apertado. O workflow de Linux avisa (sem falhar)
quando sobram menos de 15 GB.

### Arquiteturas de GPU: release larga, desenvolvimento estreita

O CMake do llama.cpp compila **cada arquivo `.cu` para dez arquiteturas**
(`compute_50` até `121a`). Medido na ksia: 186 objetos a ~3/min, mais de vinte
minutos só nisso - e nove das dez arquiteturas são para placas que aquela
máquina não tem.

As duas situações querem coisas opostas, e por isso são configuradas em lugares
diferentes:

- **Release** (estes workflows): a lista larga, porque o binário vai rodar em
  máquinas que não são a que compilou. O passo `Compilar` do workflow de Linux
  força `CUDAARCHS: ''` - **não é esquecimento**. O job roda num runner
  self-hosted, e uma máquina de desenvolvimento pode ter `CUDAARCHS` no ambiente
  para encurtar o build local; herdar aquele valor produziria, em silêncio, uma
  release que só funciona na GPU de quem compilou.
- **Desenvolvimento** (`pom-ksia.sh`, fora do repositório): só a arquitetura da
  máquina, via `CUDAARCHS`. O CMake ≥ 3.20 inicializa `CMAKE_CUDA_ARCHITECTURES`
  a partir dessa variável de ambiente, então não é preciso tocar no `build.rs`.

Note o sufixo `a` de `120a`/`121a` na lista do llama.cpp: ele liga instruções
específicas da arquitetura. O CMake só aceita esse sufixo a partir da 3.30, então
numa máquina com CMake mais antigo o valor possível é `120`, sem o `a`.

**Quanto custa a lista larga.** Medido na ksia em 25/08/2026, mesmo código, mesma
máquina, ~141 objetos CUDA gerados uma vez por arquitetura:

| Configuração | Tempo |
|---|---|
| dez arquiteturas (sem `CUDAARCHS`) | 24+ min, ainda não tinha terminado |
| `CUDAARCHS=120` | 10m29s |
| `CUDAARCHS=120` com ccache já povoado | 6m00s |

Ou seja: **compilar sem `CUDAARCHS` é quase sempre desperdício**. A lista larga
só se justifica para um binário que vai ser distribuído para hardware
desconhecido - o caso do release.

Arquitetura de cada máquina do cluster:

| Máquina | GPU | `CUDAARCHS` |
|---|---|---|
| ksia | RTX 5060 Ti | `120` |
| salem | duas arquiteturas | `"120;86"` |

A ksia é `compute_120` e só isso. O salem tem **duas** arquiteturas e precisa
das duas juntas - `86` sozinho não serve lá. Para validação e desenvolvimento
local - o caso comum - use o valor da máquina em que está compilando.

O `ccache` está instalado na ksia, com cache em `/data/ccache`, e o sistema de
compilação do llama.cpp o detecta sozinho. Ele **não** acelera a parte Rust; só
C/C++/CUDA, que é justamente o passo caro.

**Pegadinha** (também anotada no `pom-ksia.sh` do operador, fora do
repositório): trocar o valor de `CUDAARCHS` invalida o build nativo anterior, e o
cargo sozinho não percebe a troca - o build nativo do llama.cpp precisa ser
refeito à mão.

### O pacote

Binário na raiz do pacote com `apps/` ao lado, para `project_root()` achar a UI
a partir do diretório do executável - a pasta descompactada funciona em qualquer
lugar, sem `POM_HOME`. Além disso, por sistema: macOS leva o bundle `POM.app`
(com uma segunda cópia do `dist` dentro, para o `.app` sobreviver a ser
arrastado para `/Applications`), Linux leva `share/hicolor` e o `.desktop`, e o
Windows não leva nada porque o ícone já está dentro do `.exe`. Unix empacota com
`tar czf`, Windows com `7z`.

**Windows é o alvo menos exercitado**: nunca foi compilado neste repositório e a
inferência é só em CPU. Ele falha sozinho, sem afetar os outros dois.

## Defeitos anotados

- DEFEITO ANOTADO: `routing-base::plan` (sua única lógica real) não é chamado
  por ninguém - o planejador do nó
  (`crates/node/src/routing/pipeline.rs::plan_pipeline`) faz o próprio plano
  greedy. `routing-base` é código morto mantido vivo só pelos próprios testes.
  Adiado até a decisão de open-core (ver `TODO.md`).
