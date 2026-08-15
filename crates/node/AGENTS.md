# crates/node - o nó rdma-p2p

Crate `node` (binário `rdma-p2p`), o processo completo do cluster: P2P,
inferência distribuída, gateway HTTP, UI web e monitor. É um binário de raiz
única (`src/main.rs`) que declara os módulos via `#[path]` e um `lib.rs` do
workspace que faz o papel de crate-raiz para os módulos auxiliares
(`routing-contract` em `../routing-contract`, vazio para o runtime).

## Cargo.toml

- Binário único: `rdma-p2p` em `src/main.rs`.
- Features `cuda`/`metal` repassadas a `llama-cpp-sys-2` (compilação do GGML com
  GPU). Usadas por `scripts/build-docker.sh`/`RDMA_P2P_BUILD_FEATURES` no
  devupdate.
- Dependências pesadas: `libp2p` 0.56 (tokio, tcp, dns, noise, yamux, identify,
  kad, mdns, gossipsub, relay, dcutr, autonat, request-response), `pkarr` 6,
  `axum` 0.7, `reqwest` (json+stream), `tokio` full, `clap` 4 (derive),
  `socket2`, `zstd`, `tower-http` (compression-full), `routing-contract` (path).
- `build = "build.rs"`.

## build.rs

- Carimba `RDMA_P2P_BUILD_COMMIT` no binário (`cargo:rustc-env`). Precedência:
  variável `RDMA_P2P_BUILD_COMMIT` (o Dockerfile não tem `git` nem `.git`) →
  `git rev-parse --short HEAD` + sufixo `-sujo` se `git status --porcelain`
  mostrar mudanças → `desconhecido`.
- `rerun-if-changed` em `.git/HEAD` e `.git/index`, e `rerun-if-env-changed` na
  variável - sem isto o carimbo congela no primeiro build.
- O comentário no topo avisa: a UI **não** é embutida aqui, é lida do disco
  (`webapp.rs` + `config::ui_dist_dir`); `npm run build` em `apps/frontend` é
  um passo separado.
- Lido em `main.rs:1` (`BUILD_COMMIT`) e exposto em `/api/ui/node` - é como se
  descobre um peer com binário velho sem shell na máquina.

## src/main.rs - fluxo de arranque e CLI

Responsabilidades: parse de CLI, orquestração de startup, resolução de modelo,
subida do gateway HTTP e do servidor de inferência (Rust, em-processo), spawn
das tasks de fundo, loop da TUI.

**Flags (clap derive, `Args`):** `-m/--model`, `-M/--memory`, `-p/--port`
(42777), `-b/--bootstrap`, `--advertise-addr`, `--refresh` (3), `--list-models`,
`--ollama-url`, `--gateway` (deprecated: só decide se o `ollama_port` é
anunciado), `--gateway-port` (8080), `--start-layer`/`--end-layer`,
`--inference-port`, `--n-gpu-layers` (0), `--n-ctx` (8192), `--n-batch` (512),
`--max-new-tokens` (512), `--max-sessions` (1), `--kv-cache-ram`,
`--prefill-chunk-tokens` (512), `--temperature`/`--top-p`/`--top-k`/
`--repeat-penalty`/`--repeat-last-n`, `--no-thinking`, `--single-peer`,
`--pkarr-peers`, `--public-addr`, `--relay-peers`, `--serve-relay`,
`--no-default-relays`, `--no-discovery`.

**Fluxo (`main`):**
1. `respawn::remember_argv` + tracing.
2. Sem `rdma-p2p.json` → `start_setup_mode`: só `/api/setup/key`, `/api/setup/save`
   + a UI (`webapp::routes`); gravar a config respawna o processo.
3. Com config: porta P2P/HTTP vêm da config; `inference_port` = flag ou default 8181.
4. `--list-models` imprime perfis + modelos do Ollama e sai.
5. **Sem `--model`** → `start_gateway_only`: P2P + todas as rotas no ar, sem
   modelo; restaura a fatia anotada (`lastslice::take()`) **se** a anotação
   estiver `Serving` (a única que o `/health` do servidor viu carregada) e
   serve. `NeverLoaded` → `note_unloadable_slice` (o nó sobe vazio e diz por
   quê). É o caminho normal do produto.
6. **Com `--model`**: exige `--memory` (a menos que `--inference-port` esteja
   setado). Resolução do perfil, nesta ordem: `ModelProfile::find` (nome do
   arquivo, depois nome cru) → Ollama `/api/tags` (perfil por tamanho real,
   `from_ollama_model`) → `guess_from_name`.
7. Identidade `Identity::load_or_create`; chave pkarr em `.rdma-p2p.key`
   (relativa ao cwd - ver Defeitos).
8. Bootstrap = config + `--bootstrap`, ambos passando por
   `config::normalize_peer_addr`. `--advertise-addr` vence a config.
9. `Network::new` + `start` (receive, gossip 5 s, medição de rotas 30 s) +
   `bootstrap_discovery`. `--no-discovery` → `detach`.
10. Faixa de camadas, por precedência: `--start-layer/--end-layer` → nome do
    arquivo (`parse_layers_from_filename`, ex. `qwen_0_15.gguf`) →
    `calculate_assignment` (gap-filling). `lastslice::take()` é consumido
    apenas no caminho sem `--model`; aqui ele só limpa o arquivo órfão.
11. Com `--inference-port`: monta `runtime::NodeConfig` e `NodeRuntime::apply`
    (sobe o servidor de inferência Rust **em-processo** e o supervisa).
12. `GatewayState` + `start_gateway` spawnado (todo nó serve HTTP/UI).
13. `Discovery::new` (libp2p, TCP = UDP+10000) + task de publish no gossipsub
    a cada 5 s (`Network::gossip_announce_bytes`, o mesmo `self_announce` do
    UDP, com ou sem assignment) + diala `--pkarr-peers` + pkarr initial publish e republish a
    cada 30 min.
14. Loop da TUI: limpa a tela a cada `--refresh`, imprime assignment, mapa de
    camadas, peers, health, aviso de buraco anterior.

`start_setup_mode` e `start_gateway_only` estão em `main.rs` (não num módulo
separado). `parse_ollama_port` extrai a porta do `--ollama-url` só quando
`--gateway` está ligado.

## src/ subpastas

- `network/` - P2P UDP + libp2p/pkarr. Ver `crates/node/src/network/AGENTS.md`.
- `routing/` - planejador do pipeline. Ver `crates/node/src/routing/AGENTS.md`.
- `utils/` - config, TUI, respawn, sysmon. Ver `crates/node/src/utils/AGENTS.md`.
- `models/` - perfis de modelo e catálogo de GGUFs: `model.rs`
  (`ModelProfile`, `calculate_assignment`), `catalog.rs` (varredura de
  `model_dirs`), `gguf.rs` (leitor de cabeçalho/tensor table), `plan.rs`
  (fatiamento por orçamento/tamanho), `splitter.rs` (split e merge),
  `memory.rs` (carregar camadas), `lastslice.rs` (fatia anotada com estado
  Pending/Serving), `ollama.rs` (`OllamaClient`).
- `inference/` - servidor Rust e wire format: `infer_server.rs`
  (rotas de inferência + `should_draft`/`kv_can_shift`), `runtime.rs`
  (`NodeRuntime`/`NodeConfig`, supervisão em-processo + anotação da fatia),
  `wire.rs` (`[len][JSON][tensor]`, f16/zstd), `relay.rs` (chain relay),
  `prefill.rs`, `llm.rs`, `session.rs`, `spec_lab.rs`.
- `backend/` - HTTP e estado: `gateway.rs` (`start_gateway`,
  `generate_stream`), `webapp.rs` (serve a UI do disco), `webui.rs`
  (rotas `/api/ui/*`), `auth.rs` (`ApiKeyAuth`, connection key), `admin.rs`,
  `deploy.rs`, `transfer.rs` (fatias por HTTP Range), `devupdate.rs`
  (atualizar/reiniciar), `openai.rs` (rotas OpenAI-compatíveis), `messages.rs`,
  `reqlog.rs`, `session.rs`, `tool_call.rs`.

## Ligações

- **Depende de**: `routing-contract` (crate irmão), `config` do workspace.
- **Chamado por**: ninguém (é o binário). Os scripts Python
  (`old/python-inference/`) são legado - o servidor de inferência e o splitter
  são Rust, em-processo.

## Defeitos anotados

- **`--gateway` deprecated ainda muda comportamento** (`main.rs:564-568`): sem
  a flag, `ollama_port` não é anunciado, então peers não conseguem rotear para
  o Ollama local deste nó mesmo servindo a API.
