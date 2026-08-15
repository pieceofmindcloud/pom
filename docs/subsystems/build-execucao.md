# Build e execução

Como o projeto compila, empacota e roda. Autoritativo é o código; este
documento reflete o estado atual do repositório, não o que o AGENTS.md da raiz
ou o README.md possam afirmar em pontos defasados (ver Defeitos anotados).

## Workspace de crates

`Cargo.toml` na raiz define um workspace com cinco membros e
`default-members = ["crates/node"]`, então `cargo build`/`cargo test` na raiz
compilam e testam o nó. Para todos os crates públicos e o nó, `cargo test
--workspace`. `resolver = "2"`; versão e edition (0.1.0, 2021) vêm de
`[workspace.package]`; dependências compartilhadas são `anyhow`, `serde` e
`serde_json`.

- `node` - o único binário (`rdma-p2p`), o produto inteiro. Depende só de
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
cargo build --release    # binário em target/release/rdma-p2p
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
  (`RDMA_P2P_BUILD_FEATURES`).

## Carimbo de commit (crates/node/build.rs)

O binário carimba o commit de que foi construído, exposto em `/api/ui/node`
(`build_commit`) - é como se descobre um peer com binário velho sem shell na
máquina. Precedência:

1. Variável `RDMA_P2P_BUILD_COMMIT` (é como o commit entra num container: o
   `.dockerignore` corta `.git/` do contexto e a imagem builder não tem `git`,
   então ler dentro dela falha e carimbaria "desconhecido").
2. `git rev-parse --short HEAD` + sufixo `-sujo` se `git status --porcelain`
   mostrar mudanças.
3. `desconhecido` se não der para ler nada.

`rerun-if-env-changed` na variável e `rerun-if-changed` em `.git/HEAD` e
`.git/index` impedem o carimbo de congelar no primeiro build.

## Docker

- **`Dockerfile`**: dois estágios. Builder
  `nvidia/cuda:13.0.3-devel-ubuntu24.04` instala rustup + node, faz
  `npm run build` em `apps/frontend` e `cargo build --release --features cuda`
  com cache mounts (registry/git/target). Runtime é a base CUDA 13.0.3 com
  `LD_LIBRARY_PATH` incluindo `compat/` e `RDMA_P2P_HOME=/app`. O `ARG
  RDMA_P2P_BUILD_COMMIT` recebe o commit do host. (O README.md da raiz ainda
  descreve um build de três estágios com `cargo-chef` sem GPU - defasado, ver
  Defeitos.)
- **`scripts/build-docker.sh`**: leia o commit **no host** (`git rev-parse
  --short HEAD`, sufixo `-sujo` se a árvore tem mudanças), exporta
  `RDMA_P2P_BUILD_COMMIT`, roda `npm run build` e `docker compose build`.
  Construa a imagem por este script; `docker compose build` na mão carimba
  "desconhecido" em todo container, de propósito.
- **`scripts/run-docker.sh`**: `docker compose down` + `docker compose up -d`
  (não recria a imagem).
- **`docker-compose.yml`**: cluster local de 4 serviços (`node-gpu`,
  `node-gpu-2`, `node-cpu-1`, `node-cpu-2`) com `network_mode: host`; estado
  em `tests/fixtures/<nome>`, modelos montados de `./models`, GPU só via
  `deploy.resources` (exige NVIDIA Container Toolkit). Cada serviço define
  `RDMA_P2P_HOME=/data` e `RDMA_P2P_UI_DIST=/app/apps/frontend/dist` (a UI
  não ficaria sob o diretório de dados montado). As portas de cada nó vêm da
  config, não do CLI - daí o gerador abaixo. Ver Defeitos sobre os nomes das
  fixtures.
- **`scripts/gen-node-configs.py`**: gera um `rdma-p2p.json` por nó em
  `tests/fixtures/node-<N>` (nó 1 = principal, guarda `connection_key` e
  `api_keys`; demais = node com bootstrap para `127.0.0.1:42777`). Recusa
  gerar sem `connection_key` (herda da config base `--base`, nunca gera a
  chave) e `--force` sobrescreve. As portas seguem a série: gateway 8080+,
  p2p 42777+, inferência 8181+ (nó 1 herda a histórica 8092).
- `.dockerignore` corta `target/`, `.git/`, `venv/`, `models/`, `*.gguf`,
  `*.key`, `rdma-p2p.json` e `docs/` do contexto de build - por isso o commit
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

Os lançadores citados pelo AGENTS.md da raiz (`run-p1.sh`, `run-p2.sh`,
`request.sh`) **não estão na raiz**: vivem em `old/scripts/` e são referência
de operador (o binário não os usa).

- `old/scripts/run-p1.sh` - peer 1 (MacBook, Metal): camadas 0-15, `--port
  42777`, gateway em 8080, `--inference-port 8181`, `--n-gpu-layers 99`,
  `--no-thinking`. Aceita `MODEL`, `N_CTX`, `N_BATCH`, `MAX_NEW_TOKENS` por
  variável de ambiente.
- `old/scripts/run-p2.sh` - peer 2 (ksia): camadas 16-31, bootstrap para o
  peer 1, `--inference-port 8182`. Faz `cd "$(dirname "$0")"` e assume o
  binário relativo ao cwd.
- `old/scripts/request.sh` - `POST /api/generate` para o gateway com o prompt
  como primeiro argumento (`GATEWAY`/`PORT` sobrescrevem o host/porta).

Não existe `rdma-ksia.sh` em lugar nenhum do repositório (nem em histórico de
git); a referência a ele no plano da etapa não corresponde a artefato
versionado. O papel de "subir a ksia" hoje é do `old/scripts/run-p2.sh`.

## Variáveis de ambiente importantes

| Variável | Efeito |
|---|---|
| `RDMA_P2P_HOME` | Resolução de `project_root()` em primeiro lugar: config, `models/`, `venv/`, UI. Sem ela: cwd que parece o projeto, depois subir do diretório do executável. Dois nós na mesma máquina precisam de casas distintas (identidade `node_id` também mora no project root). |
| `RDMA_P2P_CONFIG` | Caminho do `rdma-p2p.json` (padrão `<project_root>/rdma-p2p.json`). |
| `RDMA_P2P_UI_DIST` | Onde `webapp.rs` lê o bundle da UI (padrão `<project_root>/apps/frontend/dist`). A imagem Docker a seta explicitamente. |
| `RDMA_P2P_DEV_AUTO_ACCEPT_TRANSFERS` | `=1` faz o nó **receptor** aceitar toda oferta de transferência sem clique. Só desenvolvimento; lida uma vez no arranque. O remetente não pode pedi-la. |
| `RDMA_P2P_DEV_UPDATE` | `=1` liga o portão de "Atualizar e reiniciar" no card do peer (`devupdate.rs`). Por processo, nunca na config. |
| `RDMA_P2P_BUILD_FEATURES` | Features com que a máquina compila no devupdate (`cuda`/`metal`); sem ela a ksia perde a GPU no primeiro clique. |
| `RDMA_P2P_BUILD_COMMIT` | Commit carimbado pelo `build.rs` (usado pelo build-docker.sh; `.dockerignore` corta `.git/`). |
| `VITE_API_TARGET` | Alvo do proxy do dev server da UI (padrão `http://127.0.0.1:8080`). |

## Release (CI)

`.github/workflows/release.yml` publica só manualmente (`workflow_dispatch`
com `version`/`prerelease`). O job `ui` builda a interface uma vez (a UI não é
embutida no binário); a matriz de 4 alvos (macOS arm64 Metal, Linux x86_64
CPU, Linux x86_64 CUDA 12.6, Windows x86_64 CPU experimental) usa
`continue-on-error` + `fail-fast: false`, e o job `release` (`if: always()`)
publica com o que compilou. O pacote = binário + `apps/frontend/dist` +
README, para `project_root()` achar a UI a partir do diretório do executável.
Windows empacota com 7z.

## Defeitos anotados

- O `README.md` (raiz) ficou defasado em relação ao código e foi corrigido na
  reconciliação de 2026-08-15: o build Docker agora tem dois estágios
  compilando `--features cuda`, e o roadmap não lista mais "Interface de
  inferência" nem "Dashboard web". Não use o README como fonte de verdade para
  arquitetura.
- O AGENTS.md da raiz já não descreve `inference_server.py` e `split_model.py`
  como se vivessem na raiz nem como se o binário os subisse (corrigido em
  2026-08-15): estão em `old/python-inference/`, legado, e o runtime suportado
  é Rust. Igualmente, os lançadores `run-p1.sh`/`run-p2.sh`/`request.sh` são
  citados como referência de operador, mas vivem em `old/scripts/` (legado).
- DEFEITO ANOTADO: `routing-base::plan` (sua única lógica real) não é chamado
  por ninguém - o planejador do nó
  (`crates/node/src/routing/pipeline.rs::plan_pipeline`) faz o próprio plano
  greedy. `routing-base` é código morto mantido vivo só pelos próprios testes.
