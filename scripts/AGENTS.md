# AGENTS.md - scripts/

Scripts operacionais (build e geracao de config de cluster) e o endereco real
do codigo Python. Autoritativo: leia os arquivos; isto e um mapa.

## scripts/

- `build-docker.sh` - constroi a imagem do cluster: le o commit **no host**
  (`git rev-parse --short HEAD`, sufixo `-sujo` se a arvore tem alteracoes),
  exporta `RDMA_P2P_BUILD_COMMIT`, roda `npm run build` em `apps/frontend` e
  `docker compose build`. O commit nao pode ser lido dentro da imagem builder
  (`.dockerignore` corta `.git/` e a imagem nao tem `git`); sem a variavel o
  `crates/node/build.rs` carimba "desconhecido" em todo container. Construa a
  imagem por este script, nunca `docker compose build` na mao.
- `gen-node-configs.py` - gera um `rdma-p2p.json` por no em
  `tests/fixtures/node-<N>/`. As portas de cada no vem da config, nao do CLI
  (com `network_mode: host`, colisao de porta e o erro tipico). No 1 =
  principal (guarda `connection_key` e `api_keys`); demais = node com
  `bootstrap_nodes` para `127.0.0.1:42777`. Recusa gerar sem `connection_key`;
  `--force` sobrescreve config existente. Nunca gera a chave de conexao - herda
  da config base (`--base`, padrao `rdma-p2p.json` na raiz) para nao partir um
  cluster existente.
- `run-docker.sh` - `docker compose down` + `docker compose up -d`. Nao recria
  a imagem; use `build-docker.sh` para isso.

## Python (nao esta na raiz - esta em old/python-inference/)

O AGENTS.md da raiz descreve `inference_server.py` e `split_model.py` como se
vivessem na raiz do repo. **Nao vivem.** O runtime suportado hoje e Rust
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
  apenas; o AGENTS.md da raiz os cita como se estivessem na raiz. `run-inf.sh`
  invoca `old/python-inference/inference_server.py` explicitamente (unico
  caminho de compatibilidade).

## Infra de build e dev na raiz

- `Dockerfile` - dois estagios: builder `nvidia/cuda:13.0.3-devel-ubuntu24.04`
  compila `--features cuda` com cache mounts (registry/git/target) e monta a UI
  (`apps/frontend/dist`); runtime e a base CUDA 13.0.3 com `LD_LIBRARY_PATH`
  no `compat/` e `RDMA_P2P_HOME=/app`. `ARG RDMA_P2P_BUILD_COMMIT` recebe o
  commit do host (ver `build-docker.sh`).
- `docker-compose.yml` - cluster local de 4 servicos (`node-gpu`, `node-gpu-2`,
  `node-cpu-1`, `node-cpu-2`) com `network_mode: host`; estado em
  `tests/fixtures/<nome>` e modelos montados de `./models`. GPU so via
  `deploy.resources` (exige NVIDIA Container Toolkit no host). UI reencontrada
  por `RDMA_P2P_UI_DIST=/app/apps/frontend/dist` porque `RDMA_P2P_HOME=/data` nao
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
  `*.key`, `rdma-p2p.json`, `docs/` do contexto de build; e por isso que o
  commit precisa chegar por variavel e a config nao entra na imagem.

## Sobre os nomes das fixtures do docker-compose

Os dois itens abaixo ja foram considerados defeito, mas o captain recusou
trata-los. A razao: sao dois ambientes de gente diferente -
`gen-node-configs.py` gera a configuracao da maquina do captain, e o
`docker-compose.yml` serve a maquina do salem. Os dois esquemas de nome
(`tests/fixtures/node-<N>` e `tests/fixtures/node-gpu`/`node-gpu-2`/etc.)
**nao precisam se encontrar** - cada um atende a um operador distinto.

Nao e defeito. Nao voltar a propor essa "correcao".
