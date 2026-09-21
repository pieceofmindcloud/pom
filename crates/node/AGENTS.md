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
  `socket2`, `tower-http` (compression-full), `rusqlite` bundled para a
  auditoria persistente, `arc-swap` para snapshots lock-free do contexto de
  captura de logs, `h3`/`h3-quinn`/`quinn` para o HTTP/3 opcional dos hops de
  inferência, `routing-contract` (path).
- No Windows, `windows_firewall.rs` usa COM `INetFwPolicy2` num helper elevado
  por UAC para manter quatro regras inbound idempotentes: UDP P2P, UDP QUIC de
  inferência, UDP QUIC do gateway e TCP libp2p. O pedido automático ocorre uma
  vez por porta efetiva e fica no `local_settings` de `pom.db`; a UI oferece
  retry sem bloquear o nó ou `Sair da rede`.
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
- `main.rs` lê `POM_DESENV` uma vez no startup; somente o valor exato `1` habilita
  controles de desenvolvimento, e `NodeInfo.pom_desenv` publica essa decisão
  para a interface. A flag só controla a visibilidade local: os handlers de
  implantação preservam os valores de `RuntimeParams` recebidos pela rede, e
  o modo ausente usa `dense` (`llama.cpp`) como padrão.
- O build nativo prepara automaticamente o fork de `llama.cpp` em um worktree
  novo, clonando a branch `pom-tensor-parallel` quando o symlink versionado
  aponta para uma árvore ausente. `LLAMA_SOURCE` permite reutilizar um checkout
  local que contenha `ggml`; falhas de preparo exibem o comando de recuperação.
  O procedimento completo está em `docs/subsystems/build-execucao.md`, seção
  "Preparar um worktree local".
- Durante o build, `crates/llama-engine/build/mtp_arch.rs` lê o fork local em modo
  somente leitura:
  descobre as verificações efetivas de `LLM_GRAPH_TYPE_DECODER_MTP` em
  `src/models`, associa a verificação ao `build_arch_graph`, cruza a classe com
  `llama-model.cpp` e resolve o nome GGUF em `llama-arch.cpp`. Ele gera
  `OUT_DIR/mtp_graph_architectures.rs`; fork ausente, extração vazia, menor que
  treze entradas ou mapeamento inconsistente interrompe o build.
  As regras do extrator têm testes unitários no próprio arquivo; a dependência
  operacional da extração está em `TODO.md`, item 10.
- Com a feature `cuda`, `build.rs` compila os kernels MoE com `CUDACXX`; em uma
  validação da ksia, `CUDAARCHS=120` limita a arquitetura e `CCACHE=ccache`
  envolve o `nvcc`. O procedimento e o target separado estão em
  `docs/subsystems/build-execucao.md`.
- No alvo macOS, `build.rs` compila `src/macos_status.m` e liga somente os
  frameworks AppKit e WebKit do sistema. Linux e Windows não recebem essa
  ponte.

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
`--repeat-penalty`/`--repeat-last-n`, `--cycle-detection` (desligado por padrão), `--no-thinking`, `--single-peer`,
`--pkarr-peers`, `--public-addr`, `--relay-peers`, `--serve-relay`,
`--no-default-relays`, `--no-discovery`.

Não existe um teto absoluto adicional no `pom.json`. A capacidade de saída é
derivada do `n_ctx` efetivo confirmado por cada runtime e agregada entre os
peers vivos; `max_new_tokens` continua sendo apenas o tamanho de uma perna.
Consulte `/v1/models` depois de todos os peers confirmarem o runtime. O passo
operacional completo está em `docs/subsystems/implantacao.md`, seção 1.3.

**Fluxo (`main`):**
1. `respawn::remember_argv` + tracing.
2. Sem `pom.json`, ou com `Config.setup_pending`, → `start_setup_mode`: serve
   somente `/api/setup/*` para hardware, modelos, referência geo, raiz de
   confiança, verificações e save, além da UI (`webapp::routes`); gravar a
   config respawna o processo. Sem config, a porta P2P do setup segue
   `--port`, o que permite isolar um segundo nó local; com config pendente, a
   porta persistida continua sendo a autoridade. O modo setup preserva
   preferências locais após sair da rede.
3. Com config: valida a porta da interface e faz o preflight de disponibilidade
   antes de carregar modelos; porta P2P/HTTP vêm da config; `inference_port` =
   flag ou default 8181.
4. `--list-models` imprime perfis + modelos do Ollama e sai.
5. **Sem `--model`** → `start_gateway_only`: P2P + todas as rotas no ar, sem
   modelo; inspeciona o journal persistente da fatia e restaura somente
   `Serving` ou `Idle`. Uma fatia `Pending` conta tentativas no disco e,
   depois de `MAX_AUTOMATIC_ATTEMPTS=10`, sobe vazia com
   `note_unloadable_slice`. É o caminho normal do produto. O flag
   `--no-default-relays` também é respeitado neste caminho ao montar a rede,
   evitando que um gateway local de
   teste disque relays públicos por engano.
6. **Com `--model`**: exige `--memory` (a menos que `--inference-port` esteja
   setado). Resolução do perfil, nesta ordem: `ModelProfile::find` (nome do
   arquivo, depois nome cru) → Ollama `/api/tags` (perfil por tamanho real,
   `from_ollama_model`) → `guess_from_name`.
7. Identidade `Identity::load_or_create`; chave pkarr em `.pom.key`, ambas
   resolvidas contra `config::project_root()`.
8. Bootstrap = config + `--bootstrap`, ambos passando por
   `config::normalize_peer_addr`. `--advertise-addr` vence a config.
9. `Network::new` recebe a política TLS inicial e a catraca de admissão antes
   de `start` (receive, gossip 5 s, medição de rotas 30 s) e
   `bootstrap_discovery`. Rede privada prova `connection_key` pelo
   `/handshake`; rede pública exige `cluster_id` e uma principal/concessão da
   cadeia em `.pom-admin-trust.json`. Isso garante que a primeira sonda use o
   mesmo esquema anunciado pela UI e que nenhum Announce não autenticado entre
   em `peers`; a rede pública ainda conserva uma `connection_key` local para a
   UI do administrador, mas ela não é compartilhada nem usada na admissão;
   `--no-discovery` → `detach`.
10. Faixa de camadas, por precedência: `--start-layer/--end-layer` → nome do
    arquivo (`parse_layers_from_filename`, ex. `qwen_0_15.gguf`) →
    `calculate_assignment` (gap-filling). O journal é lido por
    `lastslice::inspect()` e só é removido por uma decisão explícita.
11. Antes de restaurar ou aplicar um modelo, inicia o monitor térmico do
    `NodeRuntime`; com `--inference-port`, monta `runtime::NodeConfig` e chama
    `NodeRuntime::apply` (sobe o servidor de inferência Rust **em-processo** e
    o supervisa). Uma GPU acima do limite remove a atribuição e o estado
    `thermal` aguarda o cooldown configurado. O `NodeConfig` carregado pela
    implantação leva também a reserva configurável do ajuste automático de
    memória; a consulta de VRAM e a aplicação ficam em `inference/llm.rs`.
    O gateway é spawnado antes desse `apply`, então lock, fit ou carga de
    modelo não podem fechar a porta de controle.
12. `GatewayState` + `start_gateway` spawnado (todo nó serve HTTP/UI). A rota
    de prévia de capacidade faz somente estimativa analítica, com quatro
    admissões por processo e `429` imediato quando todas estão ocupadas.
13. `ClusterLogs` inicia a captura limitada e as tasks best-effort; o histórico
    usa `INFO` como nível mínimo por padrão, aceita `cluster_logs_min_level` no
    `pom.json`, e o principal poda sete dias ou 500.000 linhas em lotes com
    contenção não bloqueante. O `Discovery` mantém o behaviour request/response de logs separado do túnel
    de inferência e só envia lotes ao principal. `Discovery::new` (libp2p, TCP = UDP+10000) + task de publish no gossipsub
    a cada 5 s (`Network::gossip_announce_bytes`, o mesmo `self_announce` do
    UDP, com ou sem assignment) + diala `--pkarr-peers` + pkarr initial publish e republish a
    cada 30 min. O Mainline DHT usado pelo pkarr mantém um ator em thread própria;
    no Windows o timeout ocioso de leitura do socket aparece como WSAETIMEDOUT
    (10060) a cada 50 ms na dependência, e `application_log_filter` oculta
    somente esse aviso esperado. Ele não bloqueia o runtime da UI nem o
    carregamento do modelo.
14. Loop da TUI: limpa a tela a cada `--refresh`, imprime assignment, mapa de
    camadas, peers, health, aviso de buraco anterior.

No macOS, `main` resolve a porta persistida do gateway, move o serviço Tokio
para uma thread de fundo e entrega a thread principal ao item de status
(`macos_status.rs` + `macos_status.m`) - `NSApplication` só aceita ser
conduzida da thread principal. O item abre o `/admin-ui/chat` num painel de
390 x 844 points e numa janela de 1120 x 880 points; as medidas e o endereço
saem do Rust pela ABI, não são duplicados no Objective-C. Os dois WebViews
usam o data store padrão do WebKit e, portanto, a mesma sessão do frontend.
O menu de aplicativo do AppKit contém Sobre e Sair. Sair usa a ação padrão
`terminate:` e encerra o processo depois que o laço de eventos volta, porque o
serviço Tokio de longa duração ainda não tem um canal de cancelamento
cooperativo. Quando o serviço termina por conta própria, a ponte apenas para o
laço de eventos e o acorda; ela não encerra o processo, porque o `join()` Rust
precisa propagar a mensagem e o código de saída de falhas como `pom.json`
inválido. Isso mantém `--list-models` saindo sozinho sem engolir erros. Fora do macOS o ponto de
entrada continua um CLI Tokio normal, sem nenhuma dependência de interface
gráfica.

No Windows, `main.rs` marca o executável com `windows_subsystem = "windows"`
para que o processo não crie um console. `windows_status.rs` move o serviço
Tokio para uma thread de fundo, espera o sinal enviado depois do bind do
gateway e só então conduz a janela Tao na thread principal; Wry carrega o
`/admin-ui/chat` no WebView2 instalado pelo sistema e `tray-icon` registra o
menu da área de notificação. O tracing também grava `<project_root>/pom.log`,
porque a redireção de stdout/stderr do PowerShell não é confiável para um
binário GUI. Fechar a janela esconde-a na bandeja, enquanto `Abrir POM`
restaura e `Sair do POM` termina o processo inteiro.

O servidor QUIC mantém o bind em `[::]`, mas cria o `UdpSocket` com
`socket2::Socket::set_only_v6(false)` antes de entregá-lo ao Quinn. Assim o
mesmo endpoint aceita IPv4 e IPv6 no Windows, Linux e macOS; o construtor de
cliente do Quinn já aplica a mesma opção.

`start_setup_mode` e `start_gateway_only` estão em `main.rs` (não num módulo
separado). `advertised_ollama_port` (sobre `parse_ollama_port`) extrai a porta do
`--ollama-url` e a anuncia **sempre**, com ou sem `--gateway`.

No Windows, o processo pai supervisiona o filho nativo: uma saída anormal
relança o POM, mas `Sair do POM` retorna código zero e não relança.
`pom undeploy` grava um bloqueio persistente para impedir retomadas
automáticas de uma implantação problemática.

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
  `cycle.rs` (detector de ciclo de saída, lido pelo gateway),
  `wire.rs` (frame inicial único `[len][JSON][itens][tensor]`, f16/f8/i4
  groupwise e batch validado), `relay.rs` (chain relay + agrupamento por próximo hop),
  `prefill.rs`, `llm.rs`, `scheduler.rs`, `session.rs`, `spec_lab.rs`, `mtp.rs` +
  `llama_mtp_shim.cpp` (predição multi-token pelo bloco `nextn` do modelo -
  outro mecanismo, e outra chave, que a especulação por lookup).
- `backend/` - HTTP e estado: `gateway.rs` (`start_gateway`, a fronteira de
  `AdminAuth` para `/api/ui/**` e `/admin/**`, e o preflight da porta),
  `generate_stream_with_stops` e o resumo agregado de TTFT, prefill, geração e
  total), `webapp.rs` (serve a UI do disco), `webui.rs`
  (rotas `/api/ui/*`), `auth.rs` (`ApiKeyAuth`, connection key), `admin.rs`,
  `deploy.rs`, `transfer.rs` (fatias por HTTP Range), `devupdate.rs`
  (atualizar/reiniciar), `openai.rs` (rotas OpenAI-compatíveis), `messages.rs`,
  `reqlog.rs`, `session.rs`, `tool_call.rs`.

- `macos_status.rs` e `macos_status.m` - ponte específica de macOS para o item
  de barra, o popover compacto e a janela WebKit; não carregam credenciais nem
  implementam uma API paralela.
- `windows_status.rs` - janela WebView2 e bandeja Windows, com a política
  testável de fechar para a área de notificação e sair de verdade.

`profile.rs` é a fonte única do perfil persistido do nó (`admin`, `node` ou
`infer`) e das capacidades de visibilidade/promoção e gestão de implantação.
`ProfileAction::ManageDeployment` limita adicionar máquina e desimplantar ao
perfil `admin`; remover a máquina própria continua sendo uma ação local dos
demais perfis. O perfil não substitui `Role`, que continua descrevendo
autenticação e topologia.

O protocolo de inferência ainda não foi lançado. `frame/v1-batch` é o primeiro
e único contrato oficial: não manter fallback, adapter ou código de
compatibilidade para formatos v1/v2 anteriores. Mudanças futuras devem
evoluir este contrato explicitamente.

O arranque cria o `NetworkChat` junto do `Discovery`, usando um behaviour
libp2p separado do túnel de inferência. O `GatewayState` compartilha o serviço
com as rotas `/api/ui/network-chat`; a política de cluster é aplicada pela
catraca de `network_chat_policy`.

`src/plugins.rs` é o gerenciador genérico de N plugins independentes (decisão
do captain, 2026-09-18): `PluginManager` mantém um `PluginRuntime` por
`plugin_code` do catálogo fixo (`PLUGIN_CATALOG`, hoje só `"enterprise"`), cada
um com seu próprio cliente mockável do servidor de licenças, fingerprint
compartilhado, estado persistido sem a chave (`plugin_license:<code>`),
supervisor de heartbeat e checagem de atualização de versão PRÓPRIA - nenhum
desses estados é compartilhado entre plugins nem com o `pom_core`. Uma
ativação por chave de licença primeiro valida uma vez para descobrir quais
`plugin_code` ela concede (`licensed_plugins`, códigos puros), depois
ativa/instala cada um separadamente. As features de cada plugin vêm do
`feature_set` do manifesto do release, NÃO da licença.
`PersistedPluginState.installed_release_id` (chave JSON `plugin_release_id`,
preservada por compatibilidade) só avança quando um download é de fato
instalado (`activate_and_install`/`apply_update`) - o heartbeat não escreve
nela, só usa seus próprios `release_changed`/`available_release_id` para
disparar `detect_update` e alimentar `status.available_update` (2026-09-21:
o heartbeat escrevia ali direto e corrompia a base que `detect_update`
compara contra o manifest, fazendo "Atualizar" nunca mostrar a atualização
real). `PluginStatus.update_checked_at` só avança quando a consulta de manifest
termina com sucesso; `installed_at` só avança quando o host fica pronto após
instalação ou aplicação. Ambos persistem por plugin, têm `serde(default)` para
estados existentes e não são derivados de `last_success_at`, que é atualizado
por heartbeat. Ver `docs/subsystems/licenciamento-enterprise.md`.
Todo heartbeat com SUCESSO marca
o plugin ativo e chama `ensure_host_running` (mesma lógica usada na
restauração de boot, extraída em um só lugar): se o host daquele plugin
estiver `None` - porque uma revogação temporária o derrubou, ou porque o
handshake `ready` no boot falhou - ele sobe de novo sem esperar um restart
completo do processo `pom`. `ingest_audit_event` avisa em log quando descarta
um evento por falta de host, porque antes esse caminho falhava 100%
silencioso e só dava pra confirmar lendo o banco do plugin direto.

A restauração de boot (`PluginRuntime::restore`) é uma tarefa concorrente que
sobrescreve o estado em memória com o do banco e pode religar o host por conta
própria. Por isso `from_catalog` recebe `restore_on_boot`: produção passa
`true`, e os construtores de teste passam `false`. Um teste que monta estado à
mão perde a corrida contra ela de forma não determinística - foi o que
fazia `heartbeat_success_restarts_the_stopped_plugin_host` falhar ora por
`plugin_path` sumido, ora por host revivido depois da revogação.

`plugin_host.rs` (`PluginHost`) é o único adaptador do `pom` para o processo
auxiliar `pom-plugin-host` - já suporta várias instâncias concorrentes, uma
por plugin ativo. `PluginHost::start` usa `kill_on_drop` e mata explicitamente
o processo quando o handshake `ready` falha ou estoura o timeout de 30s - sem
isso, um handshake lento (máquina ainda se recuperando de um OOM) deixava
`pom-plugin-host` órfão no SO enquanto o `PluginRuntime` continuava sem host,
e a auditoria caía pro fallback legado sem nenhum aviso. O host mantém um
leitor persistente de stdout e correlaciona respostas pelo ID do pedido; um
timeout remove somente o registro pendente, e uma resposta tardia é consumida
sem deslocar o frame da chamada seguinte. `enterprise_p2p.rs`
assina provas de fingerprint por nonce (uso genérico, não específico de um
plugin). O `GatewayState` expõe o `PluginManager` ao backend, `webui` publica
lista/ativação/atualização por plugin e `reqlog` encaminha eventos e consultas
ao plugin `enterprise` especificamente. Os dois lados do IPC usam o limite
compartilhado `enterprise_protocol::MAX_IPC_FRAME_BYTES` de 16 MiB.

`src/plugin_ui.rs` é a interface fornecida por plugins: valida o manifesto
`pom-plugin-ui/v1` que o plugin devolve na operação `ui.manifest` do `query`
(`PluginUiManifest::parse`, `validate_catalog` para os catálogos i18n), e serve
`GET /api/ui/plugins/ui` (só plugins ativos com manifesto válido) e
`GET /api/ui/plugins/:code/assets/*path` (só assets listados, via `ui.asset`).
`PluginRuntime::ui_manifest` reaproveita `query_audit`. Contrato completo em
`docs/subsystems/plugins-ui.md`.

`src/pom_core_update.rs` é deliberadamente SEPARADO de `src/plugins.rs`: a
atualização do binário `pom` em si não é um plugin e vai virar GitHub Releases
público sem licença (outra tarefa). Até essa migração, `PomCoreUpdater`
empresta o lease do plugin `enterprise` para autenticar o manifest genérico
`pom_core` e, só com uma confirmação explícita da UI, delega decisão, hash,
troca atômica e reinício a `core_update.rs` - que continua puro e sem
dependência de licenciamento. A fronteira e os limites restantes estão em
`docs/subsystems/licenciamento-enterprise.md`.

## Ligações

- **Depende de**: `routing-contract`, `llama-engine` e `brand` (crates irmãos);
  `config` é um módulo interno de `node`.
- **Chamado por**: ninguém (é o binário). Os scripts Python
  (`old/python-inference/`) são legado - o servidor de inferência e o splitter
  são Rust, em-processo.

## Defeitos anotados

Nenhum em aberto nesta pasta.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
