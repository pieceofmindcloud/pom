# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build --release          # Output: ./target/release/rdma-p2p
cargo build                    # Debug build

# List models (built-in profiles + Ollama models if running)
./target/release/rdma-p2p --list-models

# Run a single node (built-in profile)
./target/release/rdma-p2p --model gemma3 --memory 10GB

# Run with an Ollama model (auto-resolves profile from Ollama API)
./target/release/rdma-p2p --model llama3:8b --memory 10GB

# Interface web + API OpenAI — servidas por TODO no na --gateway-port.
# Abra http://localhost:8080/ ; ver "Interface web, API OpenAI e monitor" abaixo.
# Atencao: a interface e lida do disco em apps/frontend/dist em tempo de execucao
# (nao embutida), entao rode `npm run build` la depois de editar; `cargo build`
# nao precisa de Node/npm e nao reconstroi a tela.

# Multi-node test on same machine (different ports)
./target/release/rdma-p2p --model gemma3 --memory 4GB --port 42777
./target/release/rdma-p2p --model gemma3 --memory 4GB --port 42778 --bootstrap 127.0.0.1:42777

# Gateway node (HTTP API + Ollama integration)
./target/release/rdma-p2p --model llama3:8b --memory 10GB --gateway --gateway-port 8080
# Then: curl -s localhost:8080/generate -d '{"prompt":"Hello"}' | jq
#       curl -s localhost:8080/peers | jq
#       curl -s localhost:8080/health | jq

# ── Distributed pipeline inference ───────────────────────────────────────────

# 1. Split a full GGUF model into per-node slices
python split_model.py \
    --model ~/.ollama/models/blobs/sha256-<hash> \
    --start-layer 0 --end-layer 15 \
    --output split_0_15.gguf --is-first

python split_model.py \
    --model ~/.ollama/models/blobs/sha256-<hash> \
    --start-layer 16 --end-layer 31 \
    --output split_16_31.gguf --is-last

# 2. Start the inference server on each node
#    (run alongside the rdma-p2p binary)
python inference_server.py \
    --model split_0_15.gguf \
    --start-layer 0 --end-layer 15 --port 8181

python inference_server.py \
    --model split_16_31.gguf \
    --start-layer 16 --end-layer 31 --port 8181

# 3. Start rdma-p2p nodes, advertising the inference port
./target/release/rdma-p2p --model llama3:8b --memory 8GB \
    --port 42777 --inference-port 8181

./target/release/rdma-p2p --model llama3:8b --memory 8GB \
    --port 42778 --bootstrap 127.0.0.1:42777 --inference-port 8181

# 4. Start the gateway (any node can be the gateway)
./target/release/rdma-p2p --model llama3:8b --memory 8GB \
    --port 42779 --bootstrap 127.0.0.1:42777 \
    --gateway --gateway-port 8080

# 5. Send a request — gateway auto-plans the pipeline
curl -s localhost:8080/api/generate -d '{"prompt":"Explain RDMA"}' | jq

# Run tests
cargo test

# Debug logging
RUST_LOG=debug ./target/release/rdma-p2p --model gemma3 --memory 4GB
```

## Architecture

Single-binary Rust application for decentralized LLM layer coordination with Ollama integration and an optional HTTP gateway.

**Module responsibilities:**
- `main.rs` — CLI parsing, startup orchestration, model resolution (built-in → Ollama fallback), gateway startup, main refresh loop
- `model.rs` — `ModelProfile` for 8 built-in LLMs + `from_ollama_model()` for auto-profiling; gap-filling layer assignment algorithm
- `network.rs` — UDP P2P: peer discovery (broadcast + bootstrap), gossip protocol (5s), peer lifecycle (30s timeout). `PeerInfo` carries `ollama_port` and `inference_port`
- `identity.rs` — Ed25519 keypair; node ID derived from SHA256 of pubkey
- `display.rs` — Terminal UI: banner, layer map grid, peer table, coverage percentage
- `ollama.rs` — `OllamaClient` wrapping Ollama's REST API (`/api/tags`, `/api/generate`)
- `pipeline.rs` — `LoadTracker` (RAII per-peer counters), `PipelinePlan` / `PipelineStep`, `plan_pipeline()` greedy planner
- `gateway.rs` — `axum` HTTP server; owns the single generation path (`generate_stream`), `POST /api/generate`, `GET /peers`, `GET /health`
- `openai.rs` — OpenAI-compatible routes on top of `generate_stream` (protocol only)
- `webui.rs` — control API (`/api/ui/*`). The interface itself is `apps/frontend/` (React),
  built with `npm run build` and read from disk at runtime by `webapp.rs`
  (`config::ui_dist_dir()`, `$RDMA_P2P_UI_DIST` overrides); `web/` no longer exists
- `catalog.rs` + `gguf.rs` — installed-model discovery over the directories in `rdma-p2p.json`, and a GGUF header/tensor-table reader
- `plan.rs` — layer planning: `plan_split` from per-peer memory budgets, `plan_by_size` from per-slice target sizes, `even_split`/`plan_equal` for the "cut it in N" shortcut; on-demand slicing with progress
- `sysmon.rs` — RAM / VRAM sampling per platform
- `runtime.rs` — mutable node config; owns and supervises the `inference_server.py` child
- `inference_server.py` — FastAPI server per peer node; handles `POST /v1/infer` for partial forward passes (hidden-state threading)
- `split_model.py` — Pure-Python GGUF splitter; extracts layer range, remaps names, rewrites a valid GGUF slice; reports `RDMAP2P_PROGRESS <pct>` on stderr

**Data flow:**
1. Identity generated → model resolved (built-in profile or queried from Ollama) → UDP socket bound
2. Bootstrap discovery (2s wait) → peer HashMap populated
3. `calculate_assignment()` reads covered layers from peers → finds best contiguous gap → assigns layer range
4. Two background tasks: receive loop (continuous) + gossip loop (5s re-announce, dead peer eviction)
5. If `--gateway`: axum server starts in a spawned task, routes inference to local Ollama first then peers
6. Main loop re-renders TUI every N seconds

**Concurrency:** Tokio async runtime; shared state via `Arc<RwLock<>>` for peer HashMap and assignment.

**Network protocol:** JSON-tagged enums over UDP port 42777 (default). Types: `Announce`, `PeerRequest`, `PeerList`, `Heartbeat`, `LocalDiscover`. `Announce` now carries `ollama_port: Option<u16>`.

**Multi-instance local testing:** LAN broadcast goes to `255.255.255.255:42777` — only nodes on port 42777 receive it. For multiple instances on the same machine, use `--bootstrap 127.0.0.1:<first_port>`. Peer discovery converges quickly because `PeerRequest` now triggers an immediate `Announce` reply (not just a `PeerList`).

**Layer assignment algorithm** (`model.rs` — `calculate_assignment` + `find_best_start`): subtracts memory overhead, computes max layers that fit, scans all possible start positions to maximize uncovered layers in the allocated range.

**Ollama model resolution:** `--model` checks built-in profiles first; if not found, queries `--ollama-url` (`/api/tags`) and calls `ModelProfile::from_ollama_model(name, size_bytes)` to estimate layer count from file size.

**Gateway routing:** `POST /api/generate` three-tier fallback:
1. **Distributed pipeline** — `plan_pipeline()` builds an ordered `PipelinePlan` from peers advertising `inference_port`; `execute_pipeline()` chains HTTP calls threading `hidden_state` tensors (base64-encoded float32); load tracked via RAII `LoadGuard`
2. **Local Ollama** — direct call to local Ollama instance
3. **Peer Ollama** — iterates alive peers with `ollama_url()` and forwards the request

**Pipeline planner** (`pipeline.rs`): greedy sweep from layer 0 to total_layers; at each position picks the peer scoring `layers_count * 100 - current_load * 10`; returns `None` (falls back) if any gap exists.

**`--inference-port`**: when set, the node advertises itself as a pipeline participant. Run `inference_server.py` on the same port alongside the rdma-p2p binary.

**`inference_server.py`**: FastAPI + llama-cpp-python. First peer tokenizes → runs eval → extracts hidden state via `llama_get_embeddings`. Middle peers inject via `llama_batch.embd` → run decode → extract. Last peer samples tokens → returns text.

**`split_model.py`**: pure stdlib GGUF reader/writer. Selects `blk.<N>.*` tensors for the assigned range, plus `token_embd.weight` (first), `output_norm/output.weight` (last). Remaps names `blk.(start+i).*` → `blk.i.*`, updates `llama.block_count` KV, writes new GGUF with recalculated offsets.

---

## Subindo o cluster de dois peers (MacBook + ksia)

Procedimento verificado em 2026-08-07 com os shards `models/qwen/qwen_0_15.gguf`
(peer 1) e `models/qwen/qwen_16_31.gguf` (peer 2).

### Como as peças se encaixam

- **Um processo por máquina.** O binário Rust sobe o `inference_server.py`
  sozinho quando `--inference-port` é passado (ver `src/main.rs`, bloco
  "Start Inference Server"). Não se roda o Python à mão.
- Ele procura o interpretador em `./venv/bin/python3` relativo ao **cwd**;
  se não existir, cai para `python3` do sistema. Por isso os scripts fazem
  `cd "$(dirname "$0")"` e o venv tem que ficar na raiz do repo.
- **As camadas saem do nome do arquivo.** `parse_layers_from_filename()` lê os
  dois últimos campos separados por `_`: `qwen_0_15.gguf` → 0-15,
  `qwen_16_31.gguf` → 16-31. Não é preciso `--start-layer`.
- **O nome do modelo tem que bater entre os peers.** `ModelProfile::guess_from_name()`
  reduz o nome do arquivo a um rótulo (qualquer nome contendo `qwen` vira
  `qwen35`). O planejador (`pipeline.rs`) só encadeia peers com `model` idêntico -
  shards com rótulos diferentes nunca formam pipeline.
- **Portas:** UDP P2P `--port` (42777), libp2p TCP = UDP + 10000 (52777),
  gateway HTTP `--gateway-port` (8080, servido por *todos* os nós),
  inference server `--inference-port` (8181). Como as máquinas são distintas,
  ambas usam os mesmos números.
- **Contexto por sessão:** `_MAX_SESSIONS = 8` no `inference_server.py` divide
  o `--n-ctx` em 8 slots. `--n-ctx 8192` → 1024 tokens por requisição.

### Dependências reais

**MacBook (Apple Silicon / Metal)**

```bash
python3 -m venv venv
./venv/bin/pip install --upgrade pip wheel setuptools cmake ninja
./venv/bin/pip install fastapi uvicorn pydantic numpy
CMAKE_ARGS="-DGGML_METAL=on -DGGML_ACCELERATE=on" \
  ./venv/bin/pip install llama-cpp-python --no-cache-dir
cargo build --release
```

**ksia (Ubuntu 24.04 + RTX 5060 Ti, Blackwell sm_120)**

O `nvidia-cuda-toolkit` do apt do Ubuntu é 12.0 e **não compila para sm_120**.
Use o repositório oficial da NVIDIA e um toolkit >= 12.8:

```bash
curl -fsSLO https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb
dpkg -i cuda-keyring_1.1-1_all.deb && apt-get update
apt-get install -y cuda-toolkit-12-9 build-essential cmake python3-venv python3-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal

cd /home/jairpereira/workspace/rdma-p2p
python3 -m venv venv
./venv/bin/pip install --upgrade pip wheel setuptools
./venv/bin/pip install fastapi uvicorn pydantic numpy
export PATH=/usr/local/cuda-12.9/bin:$PATH CUDA_HOME=/usr/local/cuda-12.9
CMAKE_ARGS="-DGGML_CUDA=on -DCMAKE_CUDA_ARCHITECTURES=120 -DCMAKE_CUDA_COMPILER=$CUDA_HOME/bin/nvcc" \
  ./venv/bin/pip install llama-cpp-python --no-cache-dir
PATH=/root/.cargo/bin:$PATH cargo build --release
```

Confira o build antes de subir o peer — sem isto `--n-gpu-layers 99` roda em CPU:

```bash
./venv/bin/python -c "import llama_cpp.llama_cpp as lc; print(lc.llama_supports_gpu_offload())"
# True + linha "Device 0: NVIDIA GeForce RTX 5060 Ti, compute capability 12.0"
```

Mantenha a **mesma versão de `llama-cpp-python` nas duas máquinas** (0.3.34 na
validação): o protocolo entre peers troca hidden states crus, e mudança de
layout entre versões corrompe silenciosamente a saída.

### Subida

```bash
# MacBook (peer 1, camadas 0-15, gateway):
./run-p1.sh

# ksia (peer 2, camadas 16-31):
ssh root@192.168.100.138 'cd /home/jairpereira/workspace/rdma-p2p && ./run-p2.sh'
# BOOTSTRAP=<ip-do-peer1>:42777 sobrescreve o endereço do peer 1.
```

O carregamento do modelo leva ~1 min; espere `/health` do inference server antes
de mandar requisição:

```bash
until curl -sf localhost:8181/health; do sleep 5; done
```

### Verificação

```bash
curl -s localhost:8080/peers | python3 -m json.tool   # 2 peers, camadas 0-15 e 16-31
ssh root@192.168.100.138 'nvidia-smi --query-compute-apps=pid,used_memory --format=csv'
./request.sh "Explique em uma frase o que e RDMA."
# → "source": "pipeline:<id-peer1>→<id-peer2>"
```

`source` começando com `pipeline:` e com **dois** hops é a única prova de que a
inferência foi conjunta. Se aparecer só um hop, ou `"source": "none"`, o
planejador não fechou a cobertura 0..total_layers.

### Pegadinhas encontradas

- **`git pull` no ksia.** Os arquivos são do usuário `jairpereira` e o acesso é
  como `root`: rode
  `git config --global --add safe.directory /home/jairpereira/workspace/rdma-p2p`.
  O repositório é privado, então o ksia usa uma *deploy key* read-only
  (`/root/.ssh/id_ed25519`) registrada no GitHub; o remoto tem que ser `git@github.com:...`.
- **Python do sistema no macOS é 3.9** e não tem numpy. Sem o venv na raiz do
  repo o inference server morre em loop com `ModuleNotFoundError: numpy` — e o
  Rust só registra isso em `inference_server_<porta>.log`, não na TUI.
- **`--memory` é dispensável** quando `--inference-port` está setado (o processo
  Python é dono da memória do modelo).
- **Shards sem metadata `rdmap2p.*`** (splits antigos, como os `qwen_*`) não
  corrigem `total_layers` via `/health`; nesses casos o valor vem do palpite de
  `guess_from_name` (32 para `qwen`), que só por sorte bate. Shards novos
  (`split_model.py`) gravam `rdmap2p.original_block_count` e são a via confiável.
  Quantas camadas o modelo tem é **`Option`, não número**, do catálogo até o
  plano: `catalog.rs` deduz o total da cobertura do conjunto quando a fatia não
  declara (e marca `total_layers_inferred`), e `deploy::model_size` faz o mesmo
  no cluster inteiro. Zerar esse "não sei" foi o bug de 2026-08-10: o plano se
  dizia completo e todo `apply` morria em `NodeConfig::validate`, depois de
  `launch` já ter parado a inferência das máquinas alvo.
- **A TUI limpa a tela a cada `--refresh` segundos.** Para ver logs, redirecione
  para arquivo (`./run-p1.sh > /tmp/p1.out 2>&1`) e leia de lá.
- **`pkill -f "target/release/rdma-p2p"` por ssh mata o próprio shell do ssh** —
  o padrão casa com a linha de comando do `bash -c` remoto, e o ssh volta com
  exit 255 sem ter matado o peer. Use um padrão que não se auto-inclua, ou
  `pgrep -fa` primeiro para conferir o que vai morrer.
- **Se a porta de inferência ainda estiver ocupada quando o peer sobe**, o
  supervisor tenta `MAX_RESTARTS` vezes, desiste em silêncio e a TUI continua
  mostrando "Mem loaded: inference_server" como se estivesse tudo bem. O peer
  aparece no cluster, com cobertura 100%, e só quebra na hora da inferência. A
  prova está em `inference_server_<porta>.log`
  (`[Errno 98] address already in use`), não na TUI. Confira as rotas de verdade:
  `curl -s localhost:8181/openapi.json` tem que listar `/v1/prefill` **e**
  `/v1/step`.

### Dois peers na mesma máquina — a armadilha é a identidade

Serve para dar janela grande sem arriscar a RAM do MacBook: as 32 camadas do
qwen cabem nos 16 GB de VRAM do ksia (medido: **7.636 MiB de 16.311** com
`n_ctx 32768` nos dois). O que se perde é a validade da medição de rede — o
salto vira loopback. Bytes por salto continuam valendo; tempo de rede, não.

Portas diferentes **não bastam**. A identidade fica em
`project_root()/.rdma-p2p-node.key` e `project_root()` é o diretório do projeto,
então dois processos no mesmo diretório carregam a mesma chave, anunciam o mesmo
`node_id` — e o cluster indexa peers por `node_id`. O segundo peer precisa de
outra *casa*, via `RDMA_P2P_HOME`:

```bash
B=/home/jairpereira/workspace/rdma-p2p-b
mkdir -p $B && cd $B
for f in models venv inference_server.py split_model.py apps; do ln -s ../rdma-p2p/$f $f; done
# a config nao e herdada: sem ela o no sobe em modo setup e nao carrega modelo.
# copie a do peer A trocando gateway_port, p2p_port e bootstrap_nodes.
RDMA_P2P_HOME=$B ../rdma-p2p/target/release/rdma-p2p --model ./models/qwen/qwen_16_31.gguf \
    --port 42778 --bootstrap 127.0.0.1:42777 --gateway-port 8091 --inference-port 8182 ...
```

A UI é lida do disco em `<RDMA_P2P_HOME>/apps/frontend/dist`, daí o symlink de
`apps` — sem ele o segundo peer sobe, mas responde 500 na interface. Confira
que deu certo com `/api/ui/cluster`: dois `node_id` **distintos**.

### Estado do cenário "peers em redes diferentes, pela internet"

Já existe em `src/discovery.rs` e `src/pkarr_node.rs`: Circuit Relay v2 (cliente
e `--serve-relay`), DCuTR para hole punching, AutoNAT, Kademlia, gossipsub e
publicação/resolução de endereço via DHT pkarr (`--pkarr-peers`, `--public-addr`,
`--relay-peers`, `--no-default-relays`). Ver os specs em `docs/superpowers/specs/`.

Falta para funcionar de fato pela internet:

1. **O endereço anunciado no gossipsub é literal `0.0.0.0:<porta>`**
   (`src/main.rs`, task de announce periódico). Na LAN não incomoda porque o
   caminho UDP reescreve `addr` com o IP de origem (`network.rs`,
   `handle_message`), mas `insert_peer()` (caminho libp2p) não reescreve nada -
   um peer que só chegue por gossipsub fica com `inference_url` = `http://0.0.0.0:8181`,
   que resolve para o *próprio* nó. Pela internet esse é o caminho principal.
2. **O tráfego de inferência não passa pelo libp2p.** O gateway fala HTTP direto
   com `http://<ip-do-peer>:<inference-port>` (`pipeline.rs`/`gateway.rs`). Relay
   e DCuTR só carregam a descoberta; sem IP roteável ou porta liberada, o hop
   HTTP falha mesmo com os peers "conectados".
3. **`--public-addr` só alimenta o pkarr**, não o `addr` do `Announce`.
4. Sem autenticação nem TLS no `/v1/infer` e no gateway - aceitável em LAN
   confiável, não na internet.

## Arquitetura Principal/Nó e as armadilhas que ela trouxe

Verificado em 2026-08-07 com as duas máquinas configuradas **pela interface**,
`./request.sh` devolvendo `"source": "pipeline:<hop1>→<hop2>"`.

### O fluxo

Sem `rdma-p2p.json`, o nó sobe em **modo setup** (`start_setup_mode`, `src/main.rs`):
serve só `web/setup.html`, onde se escolhe Principal ou Nó. O Principal gera a
**chave de conexão**; o Nó recebe essa chave mais `<ip-do-principal>:42777`.
`POST /api/setup/save` grava a config, respawna o processo e sai — o reinício
funciona e a tela volta sozinha para o painel.

Com config e **sem `--model`**, entra `start_gateway_only`: P2P e todas as rotas
no ar, modelo escolhido depois pela aba Configurar. É o caminho normal do produto;
`--model`/`--memory` são só para subida por linha de comando.

A chave de conexão é **a mesma em todo o cluster** e serve de credencial de
operador: autentica as rotas `/admin/*`, `/api/ui/{plan,apply,launch}` e também
as rotas `/v1` (ver `ApiKeyAuth` em `src/auth.rs`). Chamada entre nós carrega
essa chave (o `launch` repassa o `apply` ao peer com `bearer_auth`).

### Armadilhas de rede P2P — onde este projeto mais tropeça

Três defeitos reais, todos com o mesmo sintoma ("os peers não se veem"):

1. **Todo caminho de transmissão era condicionado a existir `LayerAssignment`.**
   Um nó configurado pela interface não tem assignment, então nunca anunciava,
   nunca respondia `PeerRequest` e `start_gateway_only` nem chamava
   `bootstrap_discovery`. Dois nós assim ficam mudos. Hoje `Network::start`
   chama o bootstrap e o gossip anuncia sempre (`self_announce`, `src/network.rs`).
2. **Socket dual-stack + destino IPv4 = EINVAL no macOS.** Com o socket em
   `[::]`, o Rust manda `sockaddr_in` e o macOS recusa
   (`Failed to send to <ip>: Invalid argument (os error 22)`); o Linux aceita.
   Resultado: descoberta funcionava num sentido só e o outro lado expirava o
   peer em 30s. `Network::normalize` mapeia para `::ffff:a.b.c.d`; há teste em
   `src/network.rs` porque a regressão é silenciosa e só aparece numa das
   plataformas.
3. **Broadcast v4 não sai de socket IPv6** - mesmo mapeamento resolve.
4. **`IPV6_V6ONLY` só vale antes do `bind`.** O socket UDP do P2P e o listener
   do servidor de inferência são montados com `socket2` justamente por isso.
   No Linux e no macOS o padrão do sistema é 0 e o antigo `setsockopt` pós-bind
   passava por acidente; no Windows o padrão é 1, e um nó ligado em `[::]` não
   receberia anúncio nenhum de peer IPv4 - a LAN inteira. Cada um leva um teste
   que fala IPv4 de verdade (`network.rs`, `infer_server.rs`).

Ao diagnosticar: `RUST_LOG=info` já mostra `Contacting bootstrap node`,
`🆕 New peer`, `Failed to send to` e `🧹 Removed N timed-out peers`. A ausência
de `Contacting bootstrap node` significa que o nó não transmite nada.

### Transferência de fatias entre máquinas

`src/transfer.rs`. Dois sentidos, ambos terminando no mesmo download: **puxar**
(o nó pede) e **oferecer** (o dono manda uma oferta, que aparece no sino do
outro lado e só começa depois de aceita — nunca se escreve no disco alheio sem
convite). Os bytes vão pelo gateway HTTP com `Range`, não pelo bitswap do
libp2p, porque é o caminho já comprovado entre as máquinas e é o que dá
retomada.

O que não é acabamento e sim requisito, porque uma fatia de 10 GB leva ~17 min
no enlace real: arquivo `.part` com retomada por `Range`, MD5 conferido antes
de o arquivo tomar o nome final (se não bater, o parcial é apagado), progresso
com velocidade e tempo restante, checagem de espaço em disco antes de começar,
e leitura/escrita em blocos de 1 MB.

Chegada dos arquivos: `<shard_dir>/recebidos/` (`models/recebidos/` por padrão).

**O que pode sair da máquina depende de quem escolheu o arquivo.** Os dois
sentidos tinham o mesmo guarda de caminho, e por isso um GGUF do LM Studio ou
solto numa pasta qualquer não podia sair — mesmo estando na máquina.

- **Puxar** é um peer pedindo um caminho que ninguém do outro lado escolheu:
  continua preso a `models/`, via `safe_model_path`. Tem teste.
- **Enviar** é o dono da máquina escolhendo na própria tela: aceita qualquer
  caminho legível (`local_send_path`). O peer recebe uma **concessão** opaca
  (`grant`, id → caminho, em `TransferState::grants`) e busca por
  `?grant=<id>`; um caminho nosso nunca viaja. Assim "mandar qualquer coisa"
  não vira "ler qualquer coisa": `?path=/etc/passwd` e um `grant` inventado são
  recusados, e há prova disso nos dois.

Na tela, três entradas para o mesmo transporte:

- **Fatias nesta máquina** (aba Modelos): lista **toda** fatia local, sempre, e
  o destino é um `select` só com os nós que ainda não a têm ("todas já têm"
  quando não falta em ninguém).
- **Fatias entre as máquinas** (aba Nós): lista de *diferença* — só o que falta
  do outro lado.
- **Enviar um arquivo desta máquina** (aba Nós): caminho digitado, para o que a
  varredura não acha.

A diferença acima é o motivo de "não existe opção de enviar uma fatia": o painel
da aba Nós é uma lista de diferença, e as máquinas do captain tinham as mesmas
fatias dos dois lados — sobravam só modelos inteiros, e nenhuma linha de fatia
para clicar (ver commit 2edfa1b). O transporte nunca faltou; faltava a entrada.

Peers da mesma máquina compartilham o hostname, então destino e etiqueta de
disponibilidade levam o começo do `node_id` quando o nome se repete
(`uniqueName`, `web/app.js`).

**Aceite automático — só para desenvolvimento, desligado por padrão.**
`RDMA_P2P_DEV_AUTO_ACCEPT_TRANSFERS=1` faz o nó **receptor** aceitar toda oferta
que chega, sem clique. É variável de ambiente do processo que recebe, lida uma
vez no arranque (`transfer::dev_auto_accept`), e o remetente não tem como pedi-la:
quem abre o portão é a máquina cujo disco vai ser escrito. Ausente, vazia, `0` e
`false` continuam exigindo confirmação — o padrão de produção — e há teste disso.
Existe porque provar o sentido "oferecer" à mão exige dois navegadores para mover
um arquivo. **Não ligue numa máquina cujo disco importa**: com ela ligada,
qualquer peer do cluster escreve gigabytes ali sem perguntar. Um aceite
automático que não consegue começar o download volta a ficar na prateleira, para
o operador pegar à mão, em vez de sumir com um erro só no log.

### Identidade do nó

`node_id` é como o cluster nomeia uma máquina, e é a chave de `blocked_nodes`.
Ele vive em `<project_root>/.rdma-p2p-node.key` (`identity::load_or_create`);
apagar esse arquivo cria uma máquina nova aos olhos dos outros peers. Não
confunda com `.rdma-p2p.key`, que é a chave do pkarr.

### Diretório de trabalho: nunca lance o binário por caminho absoluto e pronto

`models/`, `rdma-p2p.json` e `venv/` saem do **project root**
(`config::project_root`, `src/config.rs`): `$RDMA_P2P_HOME` → cwd, se parecer o
projeto → diretório do executável, subindo de `target/release`. Um
`ssh host '/caminho/rdma-p2p'` roda em `$HOME` e antes disso lia outra config e
não achava fatia nenhuma — a ksia aparecia sem os shards que estavam no disco
dela. A linha `Project root: ...` no arranque diz qual foi escolhido; confira-a
antes de suspeitar do catálogo. Use `run-p1.sh`/`run-p2.sh`, que fazem `cd`.

### Porta de inferência

Nó configurado pela interface não passa `--inference-port`, e sem porta o
`apply` recusava todo modelo. O padrão é `config::DEFAULT_INFERENCE_PORT` (8181),
anunciado desde o arranque; o servidor Python só sobe quando uma fatia é
atribuída.

## Interface web, API OpenAI e monitor

Verificado em 2026-08-07 com MacBook + ksia.

### Como a interface sobe

Não sobe separado: **todo nó já serve a UI** na `--gateway-port`, junto da API.
Abra `http://<peer>:<porta>/`. A interface é o build de `apps/frontend/`, lido do
disco em tempo de execução por `webapp.rs` (`config::ui_dist_dir()`) — não
embutido no binário. **Editar `apps/frontend/` exige `npm run build` lá dentro**,
não `cargo build`; recarregar o navegador depois basta.

- MacBook: `./run-p1.sh` → <http://localhost:8080/>
- ksia: `./run-p2.sh` → <http://192.168.100.138:8090/>. **A 8080 do ksia é do
  `code-server`**; por isso o peer 2 usa 8090. Nada precisa ser ajustado no
  outro lado: `Announce`/`PeerInfo` carregam `gateway_port` e o painel de
  qualquer máquina encontra o peer sozinho.

`run-p1.sh` e `run-p2.sh` aceitam `N_CTX`, `N_BATCH`, `MAX_NEW_TOKENS`, `MODEL`,
`BOOTSTRAP` e `GATEWAY_PORT` por variável de ambiente.

O menu está em `apps/frontend/src/components/nav.ts` (as rotas, em `App.tsx`):
Painel, Chat de teste, Modelos, Modelos fatiados, **Implantar na rede**,
Transferências, Logs e Configurar.

### O chat de teste

`apps/frontend/src/routes/Chat.tsx`. Fala com o cluster por
`POST /v1/chat/completions` com `stream: true`, igual a qualquer harness — o que
aparece ali é o que um harness receberia. O histórico é do navegador
(`lib/chatStore.ts`), não do nó.

Três coisas que a conversa guarda além das falas, e por quê:

- **A telemetria da execução chega fechada.** São quatro linhas de números por
  salto, embaixo de toda resposta; abrir é um clique, rolar por cima em todo
  turno não era. Há teste (`Chat.telemetry.test.tsx`).
- **Trocar de modelo no meio vira um evento no fluxo** (`role: "event"`, que
  `turnsOnly` filtra antes de a conversa viajar na requisição). As respostas
  antes e depois vieram de máquinas diferentes; sem a marca o transcript se lê
  como um modelo se contradizendo. O seletor fica **embaixo do campo de texto**,
  porque é parte de escrever a mensagem, não do cabeçalho.
- **O título se edita clicando nele** e fica gravado (`titled`, que impede a
  primeira mensagem de sobrescrever o nome escolhido). O captain nomeia a
  conversa com o modelo em teste para reencontrá-la quando for ler a telemetria.

### Onde os modelos são procurados — é configuração, não código

Autoritativo: `src/catalog.rs` (varredura) e `src/config.rs` (as pastas).
`rdma-p2p.json` manda em tudo, pela aba **Modelos** ou pelo arquivo:

- `model_dirs` — lista de pastas de **modelos inteiros**. Ausente ou `null`
  significa "detecte" (LM Studio + Ollama, como sempre foi); lista **vazia**
  significa "não procure em lugar nenhum". Um `rdma-p2p.json` antigo, sem os
  campos, se comporta exatamente como antes — há teste disso.
- `shard_dir` — pasta única das **fatias**: destino do fatiamento local, dos
  arquivos recebidos (`<shard_dir>/recebidos/`) e a **única** que um peer pode
  puxar (`transfer::safe_model_path`). Padrão `<project_root>/models`.

Cada pasta é lida pelo que ela é (`catalog::detect_source`): store do Ollama
(manifestos + blobs), biblioteca do LM Studio (nome `publisher/repo`), ou pasta
qualquer de `.gguf` (nome = arquivo). **LM Studio também tem `blobs/` e
`manifests/`, vazios** — por isso o nome da pasta é conferido antes do layout,
senão a biblioteca dele volta vazia. Detectados hoje no MacBook:
`~/.lmstudio/models` e `~/.ollama/models` (que é *symlink* para dentro da
biblioteca do LM Studio). O captain está movendo os `.gguf` para
`/Volumes/sdcard/models`, que entra como "pasta".

`GET /api/ui/models` devolve os diretórios varridos junto com o resultado, então
uma lista vazia é sempre explicável. `GET/POST /api/ui/paths` lê e grava a
configuração; gravar recusa caminho que não exista ou não seja diretório.

**Varredura de volume lento**: o cartão SD do captain leva ~2 min na primeira
varredura (ler o cabeçalho de GGUFs de 14 GB). O catálogo por isso serve o
resultado anterior e revalida por trás (`webui::local_catalog`); só a primeira
varredura, e a troca de pastas, esperam.

**A primeira varredura acontece no arranque, não no primeiro clique**
(`webui::warm_catalog`, chamada de `gateway::start_gateway`, que é por onde os
dois caminhos de subida passam). Medido em 2026-08-14 com as fatias num volume
externo: `/api/ui/models` frio custava **11,9 s** e o `/api/ui/cluster/models`
que a tela de implantar faz custava **13,2 s** frio contra **0,15 s** com o
catálogo quente. **O fan-out para os onze peers nunca foi o custo** - era a
caminhada local, e cada peer pagava a sua na primeira vez que alguém perguntava.
Com o aquecimento no arranque a mesma primeira carga fica em **0,18 s**. Nada
mais mudou: mesmo cache, mesmo TTL, mesmo stale-while-revalidate; mudou *quando*
a caminhada acontece. Ela é deliberadamente não esperada - um volume lento não
pode segurar a porta do gateway fechada.

### Fatiar pela tela — por tamanho, não em partes iguais

Decisão do captain em 2026-08-08: as fatias **não** têm o mesmo tamanho. Quem
manda no tamanho de uma fatia é a memória da máquina que vai recebê-la, e as
máquinas do cluster não se parecem. Então a aba Modelos monta uma lista fatia a
fatia, cada uma com um **alvo em GB**, e a última pode simplesmente levar "o
resto".

`split_model.py` continua trabalhando por camada inicial e final; quem converte
GB em faixa de camadas é `plan::plan_by_size`, somando o tamanho real de cada
bloco da tensor table (`gguf::read_layout`) — não `total / n_camadas`, porque o
embedding viaja em toda fatia e os blocos não são iguais entre si. O
arredondamento é para camada inteira e fica **visível**: a tela mostra lado a
lado o alvo pedido, a faixa que saiu e o tamanho real.

- `POST /api/ui/slice/plan` — prévia (a tela chama a cada tecla, com o layout em
  cache por arquivo). Com `equal_parts: N` em vez de `slices`, devolve o atalho
  "dividir em N iguais" já preenchido com o tamanho real de cada faixa, que o
  usuário continua podendo editar.
- `POST /api/ui/slice` — executa. Refaz o plano em vez de confiar na prévia, e
  recusa lista vazia, fatia que não cobre nem uma camada, e fatia que ficou sem
  camada nenhuma. Cobertura incompleta **não** é recusa: fatiar só parte do
  modelo é legítimo, a tela diz quanto falta.

Números medidos no Qwen3.5-9B (32 camadas, 5,0 GB): alvo de 1,5 GB vira camadas
0–7 e 1,4 GB reais; "o resto" vira 8–31 e 4,0 GB. A camada 0 sozinha pesa 653 MB,
então um alvo de 100 MB é recusado com esse número na mensagem.

### A tabela de tipos ggml existe em dois lugares e tem que bater

`_GGML_TYPE_INFO` (`split_model.py`) e `ggml_type_info` (`src/gguf.rs`) dizem
quantos bytes tem um bloco de cada tipo. **Não escreva esses números de
memória** — leia do llama.cpp que está em `./venv`: `ggml.h` para os códigos e
`ggml_type_size`/`ggml_blck_size` (via `ctypes` no `libggml-base`) para os
tamanhos. Foi assim que se descobriu que o código 16 era `IQ2_XXS` e não BF16
(que é 30), que o Q5_K estava com 64% do tamanho, e que no Rust todo o bloco IQ
estava três casas fora do lugar.

Errar aqui **não dá erro**: com o tipo na tabela e o tamanho errado, a fatia sai
e só a resposta do modelo fica errada. Por isso um tipo desconhecido é recusado
em vez de estimado, e por isso `split_model_type_table_matches` (`src/gguf.rs`)
lê a tabela do Python e quebra o build se as duas divergirem.

Detalhe que parece descuido e não é: `tensor_size()` usa a diferença entre
offsets, mas `t.byte_size()` é avaliado do mesmo jeito, porque Python calcula o
default do `.get` sempre. Essa avaliação é o **único** ponto que recusa um quant
que não sabemos medir; deixar o lookup preguiçoso fatiaria tipo desconhecido em
silêncio.

As fatias saem em `<shard_dir>/<base>/`. O progresso vem do próprio script: ele imprime
`RDMAP2P_PROGRESS <pct>` no stderr (lido por `plan::parse_progress`), e é a única
coisa que ele ganhou além de `from __future__ import annotations` — sem esse
import o script nem carrega no Python 3.9, que é o único do MacBook.

Duas coisas que a barra ensina e o log não: **a maior parte do tempo é antes de
gravar** (empacotar o cabeçalho passa duas vezes pelo tokenizer; num modelo de
5 GB foram ~48 s de preparo para ~2 s de escrita), por isso a etapa aparece como
"lendo o modelo" com barra indeterminada e só depois vira percentual; e o
percentual é do arquivo, não do trabalho — 100% de uma fatia é 1/N do serviço.

### Divisão assimétrica por orçamento

Decisão do captain em 2026-08-07: **não** se divide em partes iguais. Cada peer
informa quanto de memória quer usar e `plan.rs` calcula quantas camadas cabem,
somando o tamanho **real** de cada bloco lido da tensor table do GGUF
(`gguf::read_layout`) — não a heurística de `model.rs`. A tela mostra o
orçamento, as camadas resultantes, a cobertura e quanto falta quando não fecha.

Fatiar é local e sob demanda: cada peer extrai só a sua faixa da própria cópia
do GGUF com `split_model.py --start-layer/--end-layer`, que sempre aceitou faixa
arbitrária. **Nenhum peso trafega pela rede.** (`split.py`, o wrapper `--qtd`,
é que divide em partes iguais e não é usado por este caminho.)

Como as bibliotecas divergem entre as máquinas, a tela marca em quais peers o
modelo escolhido existe e avisa quando falta em algum.

### Implantar na rede - o plano, a fusão e a ordem

"Implantar na rede" (`apps/frontend/src/routes/DeployModel.tsx`, rotas
`/api/ui/deploy/plan` e `/api/ui/deploy` em `crates/node/src/backend/deploy.rs`)
põe em serviço um conjunto de fatias que já existe. É **entrada de primeiro
nível do menu**, em `/implantar` (`/fatias/implantar` redireciona, para não
quebrar link antigo): pôr em serviço é o ato para o qual a tela existe, e estava
dois cliques fundo atrás de uma seção que se lê como listagem de arquivos.
O nó sugere a distribuição; a escolha do operador (`selection`) viaja na
requisição e o `deploy` **re-varre o cluster em vez de confiar na prévia** - o
`plan` é barato o bastante para ser chamado a cada edição, o `deploy` não.

**O que é por máquina e o que é do cluster.** `RuntimeParams` é do pipeline
inteiro - contexto, batch e pedaço decidem o orçamento de prompt que o gateway
cobra, e a amostragem só roda no último peer -, então mudar um deles só num nó
quebraria a cadeia. Duas coisas são propriedade do hardware e viajam por máquina,
em `SelectedSlice`/`Step`/`LaunchTarget` e aplicadas em `webui::params_for`:
`n_gpu_layers` e `offload_kqv` (onde mora o KV cache). O cache é dimensionado
pela janela de contexto, e uma caixa com 16 GB de VRAM e uma sem GPU no mesmo
pipeline não podem estar as duas certas sobre onde ele cabe - um interruptor só
para o cluster fazia a menor máquina decidir pela maior. Os dois se editam na
tabela de máquinas, ao lado da máquina; em branco / não marcado herdam o número
do cluster.

**Os ajustes ficam abertos, à direita.** Não há mais "expandir": são os onze
números que governam a implantação que o operador está montando à esquerda, e
escolher máquinas com eles escondidos é escolher às cegas.

**A última implantação de cada modelo fica no `localStorage`**
(`apps/frontend/src/lib/deployMemory.ts`): máquinas, ordem, ajustes e as decisões
por máquina. Ao voltar e escolher o mesmo modelo, tudo volta preenchido. A
restauração acontece **depois do primeiro plano, não antes** - a memória é de
outro dia, e uma faixa que ninguém mais tem ou uma máquina que saiu do cluster
abririam a tela num plano bloqueado que o operador não escolheu; `usableSelection`
poda contra o plano e o que não sobrevive simplesmente não volta.

Três regras saem da arquitetura, não do gosto:

- **Ordem do pipeline = ordem das camadas.** Quem fica com a camada 0 recebe o
  prompt inteiro de cada requisição. "Escolher a ordem" é o mesmo ato que
  escolher quem fica com qual faixa - não há segundo botão.
- **Um nó serve uma faixa.** Duas faixas na mesma máquina são recusa, **exceto**
  com `merge` ligado: faixas adjacentes viram uma *unidade fundida*, a máquina
  junta as fatias num arquivo só (`splitter::merge`, `split_model.py --merge`)
  e serve a união. O passo fundido carrega `parts`, uma por fatia, cada uma com
  sua fonte.
- **Sobreposição é recusa, buraco é aviso.** Duas faixas sobre a mesma camada
  rodariam a camada duas vezes e corrompem a resposta; um buraco é meia-implantação
  legítima e só impede o pipeline de planejar.

A fusão escreve em `<shard_dir>/<base>/` como temp com sufixo único e renomeia
no fim (nunca deixa arquivo pela metade com nome final), e um arquivo já
existente é reaproveitado sem reescrever. A checagem de disco da implantação
conta a **união das fatias mais o que falta** - a máquina precisa de espaço para
o modelo junto *e* para as partes que ficam no disco. O transporte é o mesmo da
tela de Transferências (Range com retomada, MD5 antes do nome final); o
progresso fica no sino e vem dos nós, então fechar a aba não interrompe nada.

### API compatível com OpenAI

Servida por **qualquer** peer, em `src/openai.rs`:

    GET  /v1/models          GET  /v1/models/{id}
    POST /v1/chat/completions   (stream: true → SSE)
    POST /v1/completions

É só camada de protocolo sobre `gateway::generate_stream`; a inferência não foi
tocada. A **chave de conexão do cluster vale como chave de API**: sem
`Authorization: Bearer <connection_key>` as rotas `/v1` respondem 401 (conferido
nos dois peers). `x_rdma_p2p.source` vai junto na resposta
não-streaming com a cadeia de hops, que é a prova de execução distribuída.

Como `inference_server.py` já embrulha o que recebe no template do Qwen
(`_apply_chat_template`), `render_prompt` **achata** a conversa em um único texto
em vez de aplicar um segundo template. Uma única mensagem `user` passa intacta,
igual ao que `/api/generate` receberia.

### Estado oculto em trânsito — formato e custo real

`src/wire.rs` é o **único** lugar que conhece o dtype e a codificação do tensor:
`[4 bytes de tamanho][JSON][tensor cru]`, f16, corpo binário. Nada de base64 e
nada de f32 na rede. `inference_server.py` fala o mesmo formato em
`POST /v1/prefill` e `POST /v1/step`; o prompt vai em pedaços
(`--prefill-chunk-tokens`, padrão 512), e `_rollback_seq_kv` rebobina o cache KV
antes de refazer um pedaço reenviado, o que torna a retentativa idempotente.

Medido com `tcpdump` no peer 2, uma requisição de 35 tokens de prompt + 34
gerados, prompt curto:

| direção | payload TCP |
|---|---|
| mac → ksia (estado oculto) | 565.336 bytes |
| ksia → mac (tokens amostrados) | 6.715 bytes |

565.336 ÷ 69 tokens = **8.192 bytes por token por hop**, que é exatamente
`hidden_dim` 4096 × 2 bytes de f16. O overhead do enquadramento some no ruído.
O formato antigo (f32 em base64) custava 4096 × 4 × 4/3 = 21.846 bytes por token
por hop, ou seja **62,5% a menos de bytes** — a meta do captain, batida. Só a
metade f16 foi medida; a razão 4/3 do base64 é aritmética do próprio
enquadramento, não estimativa.

Tempo até a primeira palavra pelo SSE: **290 ms**; 33 tokens em 2.237 ms.

int8 foi recusado pelo captain: degradaria a resposta em silêncio.

### Limite de prompt

`min(--n-batch, --n-ctx / --max-sessions − --max-new-tokens)`. O `/8` fixo saiu;
`--max-sessions` é configurável e vale 1 por padrão.

Passar disso fazia o llama.cpp **abortar o processo inteiro**
(`GGML_ASSERT(n_tokens_all <= cparams.n_batch)`), derrubando o peer. O gateway
agora mede o prompt antes e recusa com uma mensagem citando as flags. Números
medidos: o OpenCode manda ~2200 caracteres com um agente mínimo (sem
ferramentas) e ~8000 no agente padrão.

Config que fez o OpenCode funcionar: `--n-ctx 32768 --n-batch 3072
--max-new-tokens 256` → orçamento de 3072 tokens.

**Quem fica com a camada 0 processa o prompt inteiro de cada requisição**, então
essa fatia deve ir para a máquina com mais memória — no par atual, o ksia. A
tela nomeia os papéis ("processa o prompt" / "gera os tokens") e deixa escolher.

### Conectando um harness

OpenCode (verificado): `opencode.json` com provider `@ai-sdk/openai-compatible`,
`baseURL` `http://<peer>:<porta>/v1`, e um agente com prompt curto e ferramentas
desligadas — o prompt do agente padrão não cabe no orçamento acima.

```bash
OPENCODE_CONFIG=/caminho/opencode.json opencode run --pure --agent curto "..."
```

O Pi só expõe base URL para Azure; não há como apontá-lo para um endpoint
OpenAI-compatível arbitrário por env var.

### Monitor de carga

`src/sysmon.rs`. Linux: RAM de `/proc/meminfo`, VRAM e processos de `nvidia-smi`
(conferido contra `nvidia-smi` na mão: bate exato). macOS: RAM de `sysctl` +
`vm_stat`; memória unificada **não** reporta VRAM dedicada — a tela diz isso e
mostra a memória wired, que é onde ficam os buffers do Metal, em vez de inventar
um número.

### Atualizar um nó pela própria interface (só em desenvolvimento)

`src/devupdate.rs`. O card de cada peer ganha "Atualizar e reiniciar" quando
aquele processo subiu com `RDMA_P2P_DEV_UPDATE=1` - variável de ambiente, por
processo, porque a config é reescrita pela própria interface e um campo lá
deixaria o portão ligável pela rede. `RDMA_P2P_BUILD_FEATURES=cuda` (ou `metal`)
diz com que features a máquina compila; sem isso a ksia perde a GPU no primeiro
clique, que foi o engano já cometido à mão em 2026-08-10.

O painel só fala com o próprio nó (caminhos relativos em `web-ui/src/lib/api.ts`),
então agir sobre outro peer é **repasse**: `POST /api/ui/dev/update` com
`{"node_id": "..."}` faz o nó local encaminhar ao peer, como `webui::launch` já
faz com `stop-inference`. O portão é verificado **no destino**, nunca em quem
repassa - checá-lo antes da decisão impedia o painel de um nó comum de atualizar
um peer que tinha o portão aberto, enquanto a tela mostrava o botão baseada no
`dev_update` do alvo.

A sequência recusa branch fora do `main`, HEAD destacado e árvore suja, e só
então faz `git pull --ff-only` e `cargo build --release`. **Nenhum comando
destrutivo entra nisso** - não há `reset`, `checkout`, `clean` nem `stash` -
então numa máquina onde alguém está codificando o botão falha e explica, em vez
de arrumar a casa alheia. Falhou em qualquer passo, ninguém reinicia.

Cada nó reinicia **a si mesmo** (`/api/ui/dev/restart`): nenhum consegue
respawnar o irmão, porque não conhece o `RDMA_P2P_CONFIG` nem o
`RDMA_P2P_NODE_KEY` dele. Irmão é peer com o mesmo hostname **e** o mesmo host de
rede (`same_machine`) - hostname sozinho casaria duas VMs chamadas `ubuntu` e
reiniciaria máquina alheia; peer com `addr` `0.0.0.0`/`::` é descartado, porque
esse endereço não reescrito aponta para o próprio nó. A fatia servida é anotada
em `<config>.last-slice.json` (derivado da config, senão dois nós da mesma
máquina se sobrescrevem) e reaplicada no arranque.

`/api/ui/node` devolve `build_commit`, carimbado pelo `crates/node/build.rs`, com
sufixo `-sujo` quando a árvore tinha alterações. É como se descobre um peer
desatualizado sem shell na máquina: em 2026-08-10 um peer respondia
`residual capture did not fire`, erro já corrigido, e achar que era binário
velho custou uma tarde.

**Em container o `git` não existe e o `.git` não chega**: o `.dockerignore` corta
`.git/` do contexto e a imagem builder não instala `git`, então as duas leituras
do `build.rs` falhavam e todo contêiner carimbava `desconhecido` — justo onde
"que binário é esse?" é mais difícil de responder de outro jeito. Por isso o
`build.rs` prefere a variável `RDMA_P2P_BUILD_COMMIT` quando ela vem preenchida,
o `Dockerfile` a recebe como `ARG` e o `scripts/build-docker.sh` lê o commit **no
host** e exporta. Construa a imagem por esse script; `docker compose build` na
mão volta a carimbar `desconhecido`, de propósito, porque inventar um valor seria
pior que dizer que não se sabe.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
