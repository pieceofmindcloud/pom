# AGENTS.md - scripts/

Scripts operacionais (build e geracao de config de cluster) e o endereco real
do codigo Python. Autoritativo: leia os arquivos; isto e um mapa.

## scripts/

- `build-env.sh` - helper opt-in, carregado com `source`, que detecta
  `sccache` e exporta o cache CAS compartilhado, o limite de 80G, os wrappers
  Rust/CMake/CUDA, `CCACHE`, o socket dedicado e um `CARGO_TARGET_DIR` comum.
  Sem `sccache` no `PATH`, nao altera o ambiente do build.
  `test-build-env.sh` verifica os dois caminhos e a preservacao de overrides.
- `bench-rotas.sh` - executa a bateria reproduzivel de medicao da geracao
  distribuida: quatro origens, cinco prompts, aquecimento por origem, CSV com
  as telemetrias de timings/trace/decode/prefill e tabela de medianas. O CSV
  inclui ao final os contadores
  `trace_mtp_draft_tokens_{proposed,accepted,rejected}` e
  `trace_mtp_acceptance_rate`, preservando a ordem das colunas anteriores; o
  quinto prompt usa 1200 palavras hexadecimais novas e `max_tokens=8`. Envia a
  API key de inferência em `POM_BENCH_API_KEY` para cada chamada ao
  `/api/generate`; a variável não é impressa nem gravada no CSV. Por padrao
  omite `model` no `/api/generate`, para o gateway usar o modelo atualmente
  servido; `POM_BENCH_MODEL` e um override opcional. A origem `salem-ksia`
  usa `SALEM_PASS` em tempo de execucao, nunca uma senha gravada no
  repositorio. Veja o cabecalho do script para as colunas, URLs e opcoes
  `--origens`, `--repeticoes` e `--csv`.
- `build-docker.sh` - constroi a imagem do cluster: le o commit **no host**
  (`git rev-parse --short HEAD`, sufixo `-sujo` se a arvore tem alteracoes) e
  exporta `POM_BUILD_COMMIT` apenas para o runtime. Por padrao resolve a tag da
  branch remota `pom-tensor-parallel` (mesmo algoritmo da action de release,
  completando a historia de um clone raso sem baixar blobs) e usa
  `POM_LLAMA_CPP_MODE=release`; o `LLAMA_CPP_CACHE_KEY` passa a incluir essa
  tag. `POM_LLAMA_CPP_RELEASE_TAG` fixa uma release existente.
  `POM_LLAMA_CPP_MODE=source` compila o fork local;
  `POM_LLAMA_CPP_SOURCE_DIR` pode apontar para outro clone Git, cujo conteudo e
  preparado num named context temporario sem `.git`, os dados de `models/` da
  raiz, diretorios de build, chaves ou arquivos dotenv; as fontes `src/models/`
  e arquivos `build-*` em outras pastas sao preservados. O cache e chaveado por
  commit e delta. O contexto principal ignora a copia local de `llama.cpp`,
  pois o fork entra somente pelo named context. O script deriva
  `POM_CUDA_ARCH_CACHE_KEY` da lista real de arquiteturas e chama
  `docker compose build`. Os composes por cluster vivem no repositório
  `pom-cluster-config`, que chama este script com `COMPOSE_FILE`. Salem e Ksia usam o mesmo target `runtime`, com um
  `pom` universal CUDA+Vulkan e os módulos CPU/Vulkan/CUDA no mesmo runtime.
  O frontend é um estágio independente e sua cópia de `dist` é a última camada
  da imagem; Fork/tag, toolkit, arquitetura e toolchain não invalidam a UI, e
  uma edição da UI não refaz Cargo. Todos os serviços usam `/usr/local/bin/pom`.
  Construa por estes scripts, nao rode `docker compose build` diretamente.
- `test-docker-build-cache.sh` - prova que a imagem reaproveita o que nao
  mudou. A parte estrutural verifica os estagios Rust/frontend independentes,
  a cópia final do frontend, o `.dockerignore`, a exclusao do carimbo de commit
  dos estagios de build, o cache Cargo nativo serializado entre os serviços,
  que só há uma build POM combinando CPU, CUDA e Vulkan, e que o kache é
  instalado depois do rustup e antes das fontes, com `kache init` antes do
  `cargo build` e `XDG_CACHE_HOME=/build/target`. O `cargo install` do kache
  conta no grupo `toolchain`. Depois, se houver daemon, deriva do Dockerfile
  um stub e usa BuildKit sobre um
  contexto sintetico para verificar cache em cenarios de fonte, commit, fork,
  CUDA, toolchain e artefatos locais. `POM_CACHE_TEST_DEBUG=1` imprime cada
  passo, grupo e estado do cache.
- `test-docker-build-mode.sh` - testa os modos release/source com um `docker`
  falso: source prepara um named context sem `.git`, dados de `models/` da
  raiz, diretorios de build, GGUFs e credenciais, preservando `src/models/` e
  fontes `build-*` em subpastas; release mantém apenas um marcador e propaga
  a tag e o cache key fixados.
- `test-docker-entrypoint.sh` - smoke test do entrypoint Docker com binários
  falsos: confirma o caminho do host, o `exec` do `pom` e a preservação dos
  argumentos.
- `test-docker-compose.py` - materializa os Compose da Salem e da Ksia. Valida
  que todos os nós usam a mesma imagem/build/binário, além de volumes e
  visibilidade de NVIDIA/Radeon; a entrada Ksia tem um serviço `node-1` com as
  GPUs 0 e 1 e os argumentos de arquitetura/cache coerentes.
- `gen-node-configs.py` - gera um `pom.json` por no em
  `tests/fixtures/node-<N>/`. As portas de cada no vem da config, nao do CLI
  (com `network_mode: host`, colisao de porta e o erro tipico). No 1 =
  principal (guarda `connection_key` e `api_keys`); demais = node com
  `bootstrap_nodes` para `127.0.0.1:42777`. Recusa gerar sem `connection_key`;
  `--force` sobrescreve config existente. Nunca gera a chave de conexao - herda
  da config base (`--base`, padrao `pom.json` na raiz) para nao partir um
  cluster existente. `--advertise-addr` atualiza somente o endereco anunciado
  tambem em configs existentes, preservando os outros campos. Quando recebe
  `HOST:PORT`, trata a porta como a base do conjunto e usa a porta `p2p_port`
  propria de cada no; assim quatro processos no mesmo host nao anunciam todos
  o socket 42777.
- `test_gen_node_configs.py` - testes unitarios do override de
  `advertise_addr`, incluindo a preservacao dos demais campos da config.
- `bench-llamacpp.sh` - CLI do benchmark do llama.cpp puro na ksia. Recebe o
  snapshot `.config.json` do pom-bench, seleciona uma implantacao `whole`,
  verifica VRAM livre, sobe o `llama-server` do fork remoto do POM em uma porta
  local ao host remoto, mede os cinco prompts da bateria e derruba somente o
  PID criado. A implementacao Python fica em `bench_llamacpp.py`; os testes
  puros de selecao, payload, SSE e medianas ficam em
  `test_bench_llamacpp.py`. A geometria equivalente ao POM usa `-c n_ctx`,
  `-np (max_sessions * 2)` e `--kv-unified`, preservando a janela por sessao
  sem multiplicar `n_ctx`; `offload_kqv=true` usa o default do llama.cpp e so
  `false` acrescenta `-nkvo`. Um binario preexistente
  so e aceito quando o `CMakeCache.txt` prova o mesmo source com CUDA e
  `LLAMA_BUILD_SERVER=ON`; caso contrario o build vai para fora da arvore.
  Quando `mtp_enabled=true`, a configuracao vira `--spec-type draft-mtp`,
  `--spec-draft-n-max` e `--spec-draft-p-min`; o fork usa a cabeca `nextn` do
  proprio GGUF, sem modelo draft separado.
  Os artefatos vao por padrao para `/Users/jairpereira/scripts/reports` e a
  CLI, o mapa de flags e as limitacoes estao no relatorio operacional da
  tarefa. Se o `nvcc` nao estiver no `PATH` remoto, o harness autodetecta
  `/usr/local/cuda/bin/nvcc`, `/usr/local/cuda-13.0/bin/nvcc` ou
  `/usr/local/cuda-12.9/bin/nvcc`; `--cuda-compiler` e
  `LLAMACPP_CUDA_COMPILER` permitem fixar outro caminho.
- `migrate_pom_db.py` - ferramenta descartável e explícita para copiar, uma
  única vez, implantações, auditoria, API keys, preferência térmica e token
  das fontes antigas para um `pom.db` já criado pelo binário. O arranque nunca
  chama o script; ele valida conflitos, não imprime credenciais e não apaga ou
  altera as fontes.
- `run-docker.sh` - `docker compose down` + `docker compose up -d`. Nao recria
  a imagem; use `build-docker.sh` para isso.
- `build-enterprise-plugin.sh` - compila um crate `cdylib` do workspace
  (default: `enterprise-plugin-fixture`, o único que resta aqui) para uma
  plataforma explícita, localiza a extensão correta e copia o artefato com
  SHA-256 e tamanho para o diretório de saída. O cdylib de produção
  (`enterprise-plugin`, domínio de auditoria de Logs/Estatísticas) foi
  extraído para o repositório `pom-plugin-logs`, que tem seu próprio script de
  build, empacotamento, workflows de release e publicação no servidor de
  licenças - ver `docs/subsystems/licenciamento-enterprise.md`.
- `resolve-release-version.py` - lê a resposta de `GET /internal/releases` e
  escolhe o maior SemVer entre todas as plataformas, incrementando o metadado
  `build.N` para o `pom-release.sh`.
- `release-ksia.sh` - build de release na ksia para `linux-cpu`,
  `linux-cuda` (CUDA 12.9, lista larga de arquiteturas) ou `windows-cpu`
  (cruzado com `cargo xwin` + `clang-cl`/`lld-link` do LLVM 20; o STL do
  MSVC exige Clang >= 19), com estado em
  `~/.cache/pom-release` e pacote em `dist-release-ksia/`; `--dry-run` só
  imprime os comandos; `--debug-symbols` (windows-cpu) inclui `pom.pdb`. Toda
  variante compila `-p node -p pom-plugin-host` e põe o host ao lado do `pom`
  no pacote. Chamado por `.github/workflows/release-ksia.yml`.
- `test-release-ksia.sh` - dry-run de cada variante (inclusive a cópia do
  `pom-plugin-host`) e checagem de que o workflow da ksia não usa armazenamento
  do GitHub.
- `test_pom_release.sh` - smoke test local do incremento automático, override
  manual e `--dry-run`, usando `curl` e `cargo` falsos. Cobre só `pom_core`
  (binário `pom`/`pom.exe`) nas três plataformas.

O `pom-release.sh` fica na raiz porque opera sobre o checkout remoto do POM:
busca `origin/feat/enterprise`, cria worktree descartável e envia multipart
para `/internal/releases/upload`. Sem `--version`, consulta
`/internal/releases?plugin=pom_core&status=all` com o token
`POM_RELEASE_TOKEN`, agrupa todas as plataformas pela versão SemVer e
incrementa `+build.N`; `--version` é um override manual e não faz essa consulta.
Libera somente `pom_core` (compila o binário real do pacote `node` com `cargo
build --release -p node --target ...`): os defaults são `cuda,vulkan` no Linux
(mesmo conjunto do `Dockerfile`), `metal` no macOS (workflow de release) e
nenhuma feature no Windows CPU. `--features` substitui as features do build e
`--feature-set` altera apenas o metadado do upload (default `core`). Os
artefatos são `pom` em Linux/macOS e `pom.exe` no Windows; o wrapper `cargo`
do operador deve encaminhar o alvo Windows MSVC para `cargo xwin` quando o
host não for Windows. Antes do Cargo, o script remove
`CMAKE_CUDA_ARCHITECTURES` e força `CUDAARCHS=''`: o CMake trata o valor vazio
como não definido e o `CMakeLists.txt` vendorizado escolhe a lista ampla de
arquiteturas CUDA, nunca a GPU da máquina de build. O servidor exige
`plugin_abi` positivo também para `pom_core`, então o script envia `1` como
metadado de compatibilidade de transporte, não como ABI de cdylib. O
`feature_set=core` é apenas metadado e não cria entitlement. `--dry-run` não
exige token nem faz chamada HTTP e mostra `+build.1` como prévia quando a
versão não é informada.

O plugin `enterprise` (Logs de Requisições/Estatísticas) não é mais liberado
por este script: seu release (build, empacotamento, GitHub Actions e
publicação no servidor de licenças) vive inteiramente em `pom-plugin-logs`.

## Python (nao esta na raiz - esta em old/python-inference/)

`inference_server.py` e `split_model.py` **nao vivem na raiz do repo**.
O runtime suportado hoje e Rust
(`crates/node/src/inference/infer_server.rs`, `crates/node/src/models/splitter.rs`);
os scripts Python sao legado preservado em `old/python-inference/` e nao sao
invocados pelo binario (ver o `README.md` de la).

- `old/python-inference/inference_server.py` - servidor FastAPI por no
  (llama-cpp-python), pipeline por estado oculto: primeiro no tokeniza e roda
  eval, nos do meio injetam via `llama_batch.embd`, o ultimo amostra. Rotas:
  `POST /v1/infer`, `POST /v1/prefill`, `POST /v1/step`,
  `POST /v1/session/release`, `GET /health`. Estado oculto cru em f16, mesmo
  formato de `src/inference/wire.rs` (`[4 bytes de tamanho][JSON][tensor]`).
  Captura o residual real da ultima camada via callback do ggml scheduler
  (`_capture_callback`) em vez de `llama_get_embeddings_ith`, que devolve h
  **depois** do output_norm. Pool de sessoes (`_MAX_SESSIONS = 8`) divide o
  `--n-ctx` em `n_ctx_seq` (contexto recriado com `n_seq_max`); `_rollback_seq_kv`
  rebobina o KV antes de refazer um prefill reenviado (retentativa idempotente).
  `_load_ggml` liga `ggml_backend_tensor_get` por ctypes (CPU/CUDA/Metal).
- `old/python-inference/split_model.py` - splitter GGUF so com stdlib (struct,
  mmap, pathlib). Seleciona `blk.<N>.*` da faixa (renomeados para `blk.i.*`),
  sempre `token_embd.weight`; `output_norm`/`output.weight` reais so na ultima
  fatia, e nas demais injeta um `output_norm` fake all-ones (`_fake_output_norm`).
  Atualiza `block_count` do KV. `_GGML_TYPE_INFO` e a tabela de tamanhos ggml e
  **tem que bater** com `ggml_type_info` em `src/models/gguf.rs`
  (`split_model_type_table_matches` quebra o build se divergirem; tipo
  desconhecido e recusa, nunca estimativa). Imprime `RDMAP2P_PROGRESS <pct>`
  no stderr.
- `old/python-inference/split.py` - divide em N partes iguais (`--qtd`). Caminho
  legado, nao usado pelo fatiamento por orcamento (`plan.rs`).
- `old/python-inference/inspect_model.py`, `inspect_gguf.py` - utilitarios de
  inspecao do modelo e do cabecalho GGUF.
- `old/scripts/` - lancadores legados (`run-p1.sh`, `run-p2.sh`, `run-inf.sh`,
  `request.sh`, `run.sh`, `run-p0.sh`, `p2.sh`), para referencia do operador
  apenas (o procedimento de subida esta em `docs/subsystems/operacao.md`).
  `run-inf.sh`
  invoca `old/python-inference/inference_server.py` explicitamente (unico
  caminho de compatibilidade).

## Infra de build e dev na raiz

- `Dockerfile` - estagios compartilhados de dependencias, Rust, frontend e
  runtime. Salem e Ksia compilam um executável `pom` com CPU, CUDA e Vulkan,
  mais `pom-plugin-host`; os builds usam cache mounts e o frontend é
  independente. O commit do host vira `ENV
  POM_BUILD_COMMIT` no runtime, sem invalidar as camadas Cargo/npm. O
  `docker-entrypoint.sh` faz `exec` do `pom` e exporta `POM_PLUGIN_HOST`; na
  ativacao, o `pom` inicia o host como filho com o plugin baixado e IPC por
  stdin/stdout, sem socket ou volume de IPC.
- `docker-entrypoint.sh` - valida que `pom` e `pom-plugin-host` estao
  executaveis, fixa o caminho do host e preserva os argumentos do container.
  Nao inicia um host vazio no boot porque o host precisa do caminho do plugin
  Enterprise, que so existe depois da ativacao.
- `docker-compose.yml` - cluster local de 4 servicos (`node-gpu`, `node-gpu2`,
  `node-gpu-3`, `node-cpu`) com `network_mode: host`, uma ancora comum de
  build/imagem e estado persistido em `tests/fixtures/<nome>`. O padrao
  mapeia a topologia da Salem; a Ksia sobe pelo repositório
  `pom-cluster-config`. `docker-compose.salem.yml`
  tambem e selecionavel diretamente. Os dois estendem a definicao comum em
  `docker-compose.common.yml`, mantendo os mesmos servicos, contexto, imagem,
  volumes e variaveis. Todos os nós Salem/Ksia usam o mesmo executável; o
  backend é descoberto a partir dos dispositivos visíveis. A UI é
  reencontrada por
  `POM_UI_DIST=/app/apps/frontend/dist` porque `POM_HOME=/data` nao e o
  `/app` da imagem.
- `.github/workflows/release.yml` - release so manual (`workflow_dispatch` com
  `version` e `prerelease`). Job `ui` builda a interface uma vez; matriz de 4
  alvos (macOS arm64 Metal, Linux x86_64 CPU, Linux x86_64 CUDA 12.6, Windows
  x86_64 CPU experimental) com `continue-on-error` e `fail-fast: false`; o job
  `release` (`if: always()`) publica com o que compilou. Pacote = binario +
  `apps/frontend/dist` + README, para `config::project_root` achar a UI a partir
  do diretorio do executavel. Windows empacota com 7z.
- `.github/workflows/release-ksia.yml` - release manual compilado só no runner
  `ksia` (sem cota do GitHub): jobs `linux-cpu`, `linux-cuda`, `windows-cpu`
  ligados por entrada, cada um chama `scripts/release-ksia.sh` e anexa o pacote
  à tag. Ver `build-execucao.md`, "Release na ksia".
- `.cargo/config.toml` - `BINDGEN_EXTRA_CLANG_ARGS` aponta o bindgen do
  llama-cpp-sys para os headers do GCC no Ubuntu (o clang do apt nao os traz);
  perfil release com `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`,
  `incremental = true` (laco quente do sampler: ~30x medido); o linker mold
  entra no Linux via `[target.'cfg(target_os = "linux")']`.
- `.devcontainer/` - container de dev Ubuntu 24.04 com Rust, node, docker CLI +
  buildx e sshd (`root:123`, sandbox local, nao internet). `devcontainer.json`
  monta o docker.sock e a chave ssh do host; `install-buildkit.sh` baixa o
  binario `linux-amd64` do buildx (arm64 precisa de `docker-buildx-plugin`).
- `.dockerignore` - corta `target/`, `target-*/`, `.fm-secondmate/`,
  `.git/`, `venv/`, `models/`, `*.gguf`, `*.key`, `pom.json` e
  `docs/superpowers/` do contexto. Mantem `llama.cpp/` porque o link em
  `vendor/llama-cpp-sys-2/llama.cpp` precisa do codigo-fonte no build.

## Sobre os nomes e o estado das fixtures do docker-compose

`docker-compose.yml` e `docker-compose.salem.yml` estendem `docker-compose.common.yml` para usar os
mesmos quatro servicos nas duas maquinas,
mas preservam os diretorios de estado ja usados
por cada no. `gen-node-configs.py` continua gravando em
`tests/fixtures/node-<N>`; Compose monta as pastas nomeadas por papel de
hardware. Nao troque os caminhos dos volumes como parte do alinhamento dos
servicos: isso mudaria a localizacao da persistencia e exige uma migracao
separada. `tests/` fica fora do contexto da imagem, mas seus diretorios sao
montados pelo host em runtime.

## migrar-para-pom.sh

Migra uma maquina do nome antigo do projeto para POM. O binario nao conhece
mais o nome antigo - ele procura `pom.json`, `.pom-node.key` e `.pom.key` e le
as variaveis `POM_*` -, entao quem carrega a compatibilidade e este script:
renomeia config (inclusive variantes como `-cpu.json`), a anotacao
`*.last-slice.json` e as duas chaves de identidade. Nao toca em modelo nem em
fatia. Depois de rodado em todas as maquinas, pode ser apagado.

Fatia cortada antes do rename perde o `original_block_count` (o metadado agora
se chama `pom.original_block_count`) e volta a ser "total de camadas
desconhecido", caso que o codigo ja trata; refatiar restaura.

`--quiet` so fala quando renomeou alguma coisa e cala o lembrete do fim: e o
modo que os scripts de deploy do operador usam, porque eles rodam a migracao em
toda subida e numa maquina ja migrada nao ha o que dizer.

Cobre tambem as **variantes por papel** (`rdma-p2p-cpu.json`,
`.rdma-p2p-node-cpu.key`): o `.gitignore` do projeto sempre usou os padroes
`rdma-p2p*.json` e `.rdma-p2p-node*.key`, e a ksia tinha as duas variantes no
disco - migrar so os nomes exatos deixava chave orfa.

## make-macos-app.sh

Monta `POM.app` em volta do binario ja compilado (padrao:
`target/release/pom` -> `dist-release/POM.app`) e copia o `pom-plugin-host`
irmao dele para `Contents/MacOS/`, recusando montar o bundle sem ele. E o unico jeito de um
executavel ter icone no macOS: o icone pertence ao bundle, nao ao arquivo.
Nome, slug e caminho do `.icns` saem do `brand.json`; o Info.plist marca
`LSUIElement` (o no nao abre janela - a interface e web, na porta do gateway) e
o bundle continua chamavel por linha de comando em
`POM.app/Contents/MacOS/pom`. O workflow de release chama este script no alvo
macOS e copia o `dist` da interface para `Contents/Resources/apps/frontend/`,
que e onde `config::bundled_ui_dist()` a procura. O script também copia
`assets/brand/macos/pom-status-template{,@2x,@3x}.png` e as localizações
`assets/brand/macos/*.lproj` para `Contents/Resources`, e recusa montar o
bundle se a arte do item de barra faltar - um item sem arte cai para um rótulo
de texto, em silêncio. O mesmo bundle abre `/admin-ui/chat` no painel de
390 x 844 points e oferece a janela maior pelo WebKit do sistema.

## derive-macos-status-icon.py

Deriva a arte do `NSStatusItem` a partir de `pom-icon-src.png`. A barra de
menus usa **template images**, que só têm canal alfa: a marca do POM é um
quadrado azul opaco e, usada como está, viraria um retângulo preto. O script
converte o traço branco em alfa, descarta o wordmark (ilegível a 18 pontos) e
grava 1x, 2x e 3x em `assets/brand/macos/`. Corra-o depois de mexer na marca;
um teste em `macos_status.rs` falha se a arte sumir.

## derive-dark-logo.py

Deriva `assets/brand/pom-logo-dark.png` a partir da arte clara. Nao e arte
nova: espelha a luminancia do letreiro (azul-petroleo chapado, desenhado para
fundo claro, que sumia no painel escuro) preservando matiz e saturacao, e
**nao toca na ilustracao do cerebro** - recolori-la achatava o volume, e ela ja
tem contraste proprio no escuro. A regiao preservada e o retangulo do brilho
dourado, com borda suave para nao deixar costura. Quando existir uma arte
escura desenhada de verdade, sobrescreva o PNG e apague este script.

## generate-brand-assets.py

Regenera os derivados quadrados de `assets/brand/pom-icon-src.png`: tamanhos
web, `pom-mark.png`, ícones Linux, `favicon.ico`, `pom.ico` e `pom.icns`.
`pom-logo-light.png` e `pom-logo-dark.png` são lockups-fonte e não são
redimensionados por este script. Rode `python3 scripts/generate-brand-assets.py`
depois de atualizar a arte manual para manter todas as plataformas alinhadas.

## Defeitos anotados

- `test-docker-build-cache.sh`, cenário "Rust toolchain changed": falha também
  sem mudança no Dockerfile. O `RUN` que baixa a release do fork vem depois do
  rustup, reconstrói quando a toolchain muda e cai no grupo `base`, que o
  cenário não espera. E como a toolchain de teste é fixa (`1.90.0`), a partir
  da segunda execução no mesmo builder a camada já está em cache e o grupo
  `toolchain` some do resultado.
