# AGENTS.md - scripts/

Scripts operacionais (build e geracao de config de cluster) e o endereco real
do codigo Python. Autoritativo: leia os arquivos; isto e um mapa.

## scripts/

- `build-docker.sh` - constroi a imagem do cluster: le o commit **no host**
  (`git rev-parse --short HEAD`, sufixo `-sujo` se a arvore tem alteracoes),
  exporta `POM_BUILD_COMMIT`, roda `npm run build` em `apps/frontend` e
  `docker compose build`. O commit nao pode ser lido dentro da imagem builder
  (`.dockerignore` corta `.git/` e a imagem nao tem `git`); sem a variavel o
  `crates/node/build.rs` carimba "desconhecido" em todo container. Construa a
  imagem por este script, nunca `docker compose build` na mao.
- `gen-node-configs.py` - gera um `pom.json` por no em
  `tests/fixtures/node-<N>/`. As portas de cada no vem da config, nao do CLI
  (com `network_mode: host`, colisao de porta e o erro tipico). No 1 =
  principal (guarda `connection_key` e `api_keys`); demais = node com
  `bootstrap_nodes` para `127.0.0.1:42777`. Recusa gerar sem `connection_key`;
  `--force` sobrescreve config existente. Nunca gera a chave de conexao - herda
  da config base (`--base`, padrao `pom.json` na raiz) para nao partir um
  cluster existente. `--advertise-addr` atualiza somente o endereco anunciado
  tambem em configs existentes, preservando os outros campos.
- `test_gen_node_configs.py` - testes unitarios do override de
  `advertise_addr`, incluindo a preservacao dos demais campos da config.
- `migrate_pom_db.py` - ferramenta descartável e explícita para copiar, uma
  única vez, implantações, auditoria, API keys, preferência térmica e token
  das fontes antigas para um `pom.db` já criado pelo binário. O arranque nunca
  chama o script; ele valida conflitos, não imprime credenciais e não apaga ou
  altera as fontes.
- `run-docker.sh` - `docker compose down` + `docker compose up -d`. Nao recria
  a imagem; use `build-docker.sh` para isso.

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

- `Dockerfile` - dois estagios: builder `nvidia/cuda:13.0.3-devel-ubuntu24.04`
  compila `--features cuda` com cache mounts (registry/git/target) e monta a UI
  (`apps/frontend/dist`); runtime e a base CUDA 13.0.3 com `LD_LIBRARY_PATH`
  no `compat/` e `POM_HOME=/app`. `ARG POM_BUILD_COMMIT` recebe o
  commit do host (ver `build-docker.sh`).
- `docker-compose.yml` - cluster local de 4 servicos (`node-gpu`, `node-gpu-2`,
  `node-cpu-1`, `node-cpu-2`) com `network_mode: host`; estado em
  `tests/fixtures/<nome>` e modelos montados de `./models`. GPU so via
  `deploy.resources` (exige NVIDIA Container Toolkit no host). UI reencontrada
  por `POM_UI_DIST=/app/apps/frontend/dist` porque `POM_HOME=/data` nao
  e o /app da imagem. Sobre os nomes das fixtures, veja "Sobre os nomes das
  fixtures do docker-compose" abaixo: nao e defeito, sao dois ambientes distintos.
- `.github/workflows/release.yml` - release so manual (`workflow_dispatch` com
  `version` e `prerelease`). Job `ui` builda a interface uma vez; matriz de 4
  alvos (macOS arm64 Metal, Linux x86_64 CPU, Linux x86_64 CUDA 12.6, Windows
  x86_64 CPU experimental) com `continue-on-error` e `fail-fast: false`; o job
  `release` (`if: always()`) publica com o que compilou. Pacote = binario +
  `apps/frontend/dist` + README, para `config::project_root` achar a UI a partir
  do diretorio do executavel. Windows empacota com 7z.
- `.cargo/config.toml` - `BINDGEN_EXTRA_CLANG_ARGS` aponta o bindgen do
  llama-cpp-sys para os headers do GCC no Ubuntu (o clang do apt nao os traz);
  perfil release com `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`,
  `incremental = true` (laco quente do sampler: ~30x medido); o linker mold
  entra no Linux via `[target.'cfg(target_os = "linux")']`.
- `.devcontainer/` - container de dev Ubuntu 24.04 com Rust, node, docker CLI +
  buildx e sshd (`root:123`, sandbox local, nao internet). `devcontainer.json`
  monta o docker.sock e a chave ssh do host; `install-buildkit.sh` baixa o
  binario `linux-amd64` do buildx (arm64 precisa de `docker-buildx-plugin`).
- `.dockerignore` - corta `target/`, `.git/`, `venv/`, `models/`, `*.gguf`,
  `*.key`, `pom.json`, `docs/` do contexto de build; e por isso que o
  commit precisa chegar por variavel e a config nao entra na imagem.

## Sobre os nomes das fixtures do docker-compose

Os dois itens abaixo ja foram considerados defeito, mas o captain recusou
trata-los. A razao: sao dois ambientes de gente diferente -
`gen-node-configs.py` gera a configuracao da maquina do captain, e o
`docker-compose.yml` serve a maquina do salem. Os dois esquemas de nome
(`tests/fixtures/node-<N>` e `tests/fixtures/node-gpu`/`node-gpu-2`/etc.)
**nao precisam se encontrar** - cada um atende a um operador distinto.

Nao e defeito. Nao voltar a propor essa "correcao".

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
`target/release/pom` -> `dist-release/POM.app`). E o unico jeito de um
executavel ter icone no macOS: o icone pertence ao bundle, nao ao arquivo.
Nome, slug e caminho do `.icns` saem do `brand.json`; o Info.plist marca
`LSUIElement` (o no nao abre janela - a interface e web, na porta do gateway) e
o bundle continua chamavel por linha de comando em
`POM.app/Contents/MacOS/pom`. O workflow de release chama este script no alvo
macOS e copia o `dist` da interface para `Contents/Resources/apps/frontend/`,
que e onde `config::bundled_ui_dist()` a procura.

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
