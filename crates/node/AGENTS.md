# crates/node - o nó pom

Crate `node` (binário `pom`), o processo completo do cluster: P2P,
inferência distribuída, gateway HTTP, UI web e monitor. É um binário de raiz
única (`src/main.rs`) que declara os módulos via `#[path]` e um `lib.rs` do
workspace que faz o papel de crate-raiz para os módulos auxiliares
(`routing-contract` em `../routing-contract`, vazio para o runtime).

## Cargo.toml

- Binário único: `pom` em `src/main.rs`.
- Features `cuda`/`metal` repassadas a `llama-cpp-sys-2` (compilação do GGML com
  GPU). Usadas por `scripts/build-docker.sh`/`POM_BUILD_FEATURES` no
  devupdate. O build CUDA mantém CUDA Graphs desligado por padrão; habilitar
  com `POM_CUDA_GRAPHS=1` exige reconstruir o binário e VRAM livre adicional.
- Dependências pesadas: `libp2p` 0.56 (tokio, tcp, dns, noise, yamux, identify,
  kad, mdns, gossipsub, relay, dcutr, autonat, request-response), `pkarr` 6,
  `axum` 0.7, `reqwest` (json+stream), `url`, `tokio` full, `clap` 4 (derive),
  `socket2`, `tower-http` (compression-full), `routing-contract` (path).
- O provider Hugging Face usa HTTP nativo via `reqwest`; não há executável `hf`,
  runtime Python ou instalação externa no caminho normal.
- `build = "build.rs"`.

## build.rs

- Carimba `POM_BUILD_COMMIT` no binário (`cargo:rustc-env`). Precedência:
  variável `POM_BUILD_COMMIT` (o Dockerfile não tem `git` nem `.git`) →
  `git rev-parse --short HEAD` + sufixo `-sujo` se `git status --porcelain`
  mostrar mudanças → `desconhecido`.
- `rerun-if-changed` em `.git/HEAD` e `.git/index`, e `rerun-if-env-changed` na
  variável - sem isto o carimbo congela no primeiro build.
- O comentário no topo avisa: a UI **não** é embutida aqui, é lida do disco
  (`webapp.rs` + `config::ui_dist_dir`); `npm run build` em `apps/frontend` é
  um passo separado.
- Lido em `main.rs:1` (`BUILD_COMMIT`) e exposto em `/api/ui/node` - é como se
  descobre um peer com binário velho sem shell na máquina.
- O build nativo também exige o fork local de `llama.cpp`: em um worktree novo,
  preparar `<worktree>/llama.cpp` com um checkout que contenha `ggml` antes de
  rodar `cargo build -p node`. O procedimento completo está em
  `docs/subsystems/build-execucao.md`, seção "Preparar um worktree local".
- Com a feature `cuda`, `build.rs` compila os kernels MoE com `CUDACXX`; em uma
  validação da ksia, `CUDAARCHS=120` limita a arquitetura e `CCACHE=ccache`
  envolve o `nvcc`. O procedimento e o target separado estão em
  `docs/subsystems/build-execucao.md`.

## src/main.rs - fluxo de arranque e CLI

Responsabilidades: parse de CLI, orquestração de startup, resolução de modelo,
subida do gateway HTTP e do servidor de inferência (Rust, em-processo), spawn
das tasks de fundo, loop da TUI.

**Flags (clap derive, `Args`):** `-m/--model`, `-M/--memory`, `-p/--port`
(42777), `-b/--bootstrap`, `--advertise-addr`, `--refresh` (3), `--list-models`,
`--ollama-url`, `--gateway` (deprecated: o único efeito que resta é marcar o nó
como `primary` na config de inferência montada pela linha de comando),
`--gateway-port` (8080), `--start-layer`/`--end-layer`,
`--inference-port`, `--n-gpu-layers` (0), `--n-ctx` (8192 por sessão), `--n-batch` (512),
`--max-new-tokens` (512), `--max-sessions` (1), `--kv-cache-ram`,
`--prefill-chunk-tokens` (512), `--temperature`/`--top-p`/`--top-k`/
`--repeat-penalty`/`--repeat-last-n`, `--no-thinking`, `--single-peer`,
`--pkarr-peers`, `--public-addr`, `--relay-peers`, `--serve-relay`,
`--no-default-relays`, `--no-discovery`.

**Fluxo (`main`):**
1. `respawn::remember_argv` + tracing.
2. Sem `pom.json` → `start_setup_mode`: só `/api/setup/key`, `/api/setup/save`
   + a UI (`webapp::routes`); gravar a config respawna o processo.
3. Com config: porta P2P/HTTP vêm da config; `inference_port` = flag ou default 8181.
4. `--list-models` imprime perfis + modelos do Ollama e sai.
5. **Sem `--model`** → `start_gateway_only`: P2P + todas as rotas no ar, sem
   modelo; restaura a fatia anotada (`lastslice::take()`) **se** a anotação
   estiver `Serving` (a única que o `/health` do servidor viu carregada) e
   serve. `NeverLoaded` → `note_unloadable_slice` (o nó sobe vazio e diz por
   quê). É o caminho normal do produto. O flag `--no-default-relays` também é
   respeitado neste caminho ao montar a rede, evitando que um gateway local de
   teste disque relays públicos por engano.
6. **Com `--model`**: exige `--memory` (a menos que `--inference-port` esteja
   setado). Resolução do perfil, nesta ordem: `ModelProfile::find` (nome do
   arquivo, depois nome cru) → Ollama `/api/tags` (perfil por tamanho real,
   `from_ollama_model`) → `guess_from_name`.
7. Identidade `Identity::load_or_create`; chave pkarr em `.pom.key`
   (relativa ao cwd - ver Defeitos).
8. Bootstrap = config + `--bootstrap`, ambos passando por
   `config::normalize_peer_addr`. `--advertise-addr` vence a config.
9. `Network::new` recebe a política TLS inicial antes de `start` (receive,
   gossip 5 s, medição de rotas 30 s) e `bootstrap_discovery`. Isso garante que
   a primeira sonda use o mesmo esquema anunciado pela UI; `--no-discovery` →
   `detach`.
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
separado). `advertised_ollama_port` (sobre `parse_ollama_port`) extrai a porta do
`--ollama-url` e a anuncia **sempre**, com ou sem `--gateway`.

## src/ subpastas

- `network/` - P2P UDP + libp2p/pkarr. Ver `crates/node/src/network/AGENTS.md`.
- `routing/` - planejador do pipeline. Ver `crates/node/src/routing/AGENTS.md`.
- `utils/` - config, TUI, respawn, sysmon. Ver `crates/node/src/utils/AGENTS.md`.
- `models/` - perfis de modelo e catálogo de GGUFs: `model.rs`
  (`ModelProfile`, `calculate_assignment`), `catalog.rs` (varredura de
  `model_dirs`), `gguf.rs` (leitor de cabeçalho/tensor table), `plan.rs`
  (fatiamento por orçamento/tamanho), `splitter.rs` (split e merge),
  `memory.rs` (carregar camadas), `lastslice.rs` (fatia anotada com estado
  Pending/Serving), `ollama.rs` (`OllamaClient`), `provider.rs` (contrato
  agnóstico de provider) e `huggingface.rs` (`HuggingFaceProvider` HTTP
  nativo).
- `inference/` - servidor Rust e wire format: `infer_server.rs`
  (rotas de inferência, scheduler local + `should_draft`/`kv_can_shift`), `runtime.rs`
  (`NodeRuntime`/`NodeConfig`, supervisão em-processo + anotação da fatia),
  `wire.rs` (frame inicial único `[len][JSON][itens][tensor]`, f16 e batch
  validado), `relay.rs` (chain relay + agrupamento por próximo hop),
  `prefill.rs`, `llm.rs`, `scheduler.rs`, `session.rs`, `spec_lab.rs`, `mtp.rs` +
  `llama_mtp_shim.cpp` (predição multi-token pelo bloco `nextn` do modelo -
  outro mecanismo, e outra chave, que a especulação por lookup).
- `backend/` - HTTP e estado: `gateway.rs` (`start_gateway`,
  `generate_stream_with_stops`), `webapp.rs` (serve a UI do disco), `webui.rs`
  (rotas `/api/ui/*`), `auth.rs` (`ApiKeyAuth`, connection key), `admin.rs`,
  `deploy.rs`, `transfer.rs` (fatias por HTTP Range), `devupdate.rs`
  (atualizar/reiniciar), `openai.rs` (rotas OpenAI-compatíveis), `messages.rs`,
  `reqlog.rs`, `session.rs`, `tool_call.rs`.

O protocolo de inferência ainda não foi lançado. `frame/v1-batch` é o primeiro
e único contrato oficial: não manter fallback, adapter ou código de
compatibilidade para formatos v1/v2 anteriores. Mudanças futuras devem
evoluir este contrato explicitamente.

## Ligações

- **Depende de**: `routing-contract` (crate irmão), `config` do workspace.
- **Chamado por**: ninguém (é o binário). Os scripts Python
  (`old/python-inference/`) são legado - o servidor de inferência e o splitter
  são Rust, em-processo.

## Defeitos anotados

Nenhum em aberto nesta pasta.
