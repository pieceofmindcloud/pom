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

- `build.rs` carimba o fallback `POM_BUILD_COMMIT` no binário:
  `git rev-parse --short HEAD` + sufixo `-sujo` se `git status --porcelain`
  mostrar mudanças → `desconhecido`. `build_commit()` prefere a variável de
  runtime `POM_BUILD_COMMIT` quando não vazia. No Docker, o commit do host é
  exportado só no estágio runtime para não invalidar Cargo.
- `rerun-if-changed` em `.git/HEAD` e `.git/index` atualiza o fallback local.
- O comentário no topo avisa: a UI **não** é embutida aqui, é lida do disco
  (`webapp.rs` + `config::ui_dist_dir`); `npm run build` em `apps/frontend` é
  um passo separado.
- `build_commit()` em `main.rs` é exposto em `/api/ui/node` e usado na checagem
  de atualizações - identifica peer ou executável sem shell local.
- `main.rs` lê `POM_DESENV` uma vez no startup; somente o valor exato `1` habilita
  controles de desenvolvimento e as rotas locais de rebuild de plugins, e
  `NodeInfo.pom_desenv` publica a decisão para a interface. A flag não altera
  a política de licença nem os `RuntimeParams` recebidos pela rede; o modo
  ausente usa `dense` (`llama.cpp`) como padrão.
- O build nativo prepara automaticamente o fork de `llama.cpp` em um worktree
  novo, clonando a branch `pom-tensor-parallel` quando o symlink versionado
  aponta para uma árvore ausente. `LLAMA_SOURCE` permite reutilizar um checkout
  local que contenha `ggml`; falhas de preparo exibem o comando de recuperação.
  O procedimento completo está em `docs/subsystems/build-and-run.md`, seção
  "Preparing a local worktree".
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
  validação no servidor CUDA, `CUDAARCHS=120` limita a arquitetura e `CCACHE=ccache`
  envolve o `nvcc`. O procedimento e o target separado estão em
  `docs/subsystems/build-and-run.md`.
- No alvo macOS, `build.rs` compila `src/macos_status.m` e liga somente os
  frameworks AppKit e WebKit do sistema. Linux e Windows não recebem essa
  ponte.

## src/main.rs - fluxo de arranque e CLI

Responsabilidades: parse de CLI, orquestração de startup, subida do gateway
HTTP e do servidor de inferência (Rust, em-processo) e spawn das tasks de
fundo. **O modelo nunca vem da linha de comando**: ele é implantado pela
interface, e num reinício volta pelo journal da fatia (`lastslice`).

**Flags (clap derive, `Args`):** `-p/--port` (42777), `-b/--bootstrap`,
`--advertise-addr`, `--ollama-url`, `--gateway-port` (8080),
`--setup-bind`, `--cycle-detection` (desligado por padrão), `--pkarr-peers`,
`--public-addr`, `--relay-peers`, `--serve-relay`, `--no-default-relays`,
`--no-discovery`. `--relay-peers` soma-se a `relay_peers` do `pom.json`
(`merged_relay_peers`, config primeiro), como `--bootstrap` a
`bootstrap_nodes`. Não há flag de modelo, memória, faixa de camadas, amostragem
nem porta de inferência: isso vem da implantação (`runtime::NodeConfig`) e do
`pom.json` (`inference_port`, padrão 8181; `slot_save_path`).

**Toda flag tem variável de ambiente** (`ARG_ENV`, nome por
`brand::env_var`: `--gateway-port` → `POM_GATEWAY_PORT`), aplicada por
`cli_command` com o builder do clap, porque o derive só aceita literal. A flag
vence a variável, que vence o padrão; as booleanas usam `BoolishValueParser`.
A variável vence o `pom.json`: `apply_env_overrides` troca `p2p_port` e
`gateway_port` gravados quando `parse_args` viu o valor vir do ambiente
(`ValueSource::EnvVariable`). Na linha de comando essas duas continuam cedendo à
config - só valem no modo setup, e é isso que mantém uma troca de porta feita
pela interface depois do respawn, que repete o argv. A variável manual
`POM_FORCE_SETUP` é tratada por `force_setup_if_requested`; a lista que a isola
na prova de cobertura de flags é compilada somente em testes.

Não existe um teto absoluto adicional no `pom.json`. A capacidade de saída é
derivada do `n_ctx` efetivo confirmado por cada runtime e agregada entre os
peers vivos; `max_new_tokens` continua sendo apenas o tamanho de uma perna.
Consulte `/v1/models` depois de todos os peers confirmarem o runtime. O passo
operacional completo está em `docs/subsystems/deployment.md`, seção 1.3.

**Fluxo (`main`):**
1. `respawn::remember_argv` + tracing.
2. Sem `pom.json`, ou com `Config.setup_pending`, → `start_setup_mode`: serve
   somente `/api/setup/*` para hardware, modelos, referência geo, raiz de
   confiança, verificações e save, além da UI (`webapp::routes`); gravar a
   config respawna o processo. Sem config, a porta P2P do setup segue
   `--port`, o que permite isolar um segundo nó local; com config pendente, a
   porta persistida continua sendo a autoridade. O modo setup preserva
   preferências locais após sair da rede. O listener HTTP do setup não tem
   autenticação e liga em `0.0.0.0` por padrão, para o primeiro acesso
   alcançar um nó novo (inclusive num container com porta publicada); o
   endurecimento é escolhido no próprio setup. `--setup-bind <IP>` troca o IP
   (`127.0.0.1` mantém o setup só local). A flag não
   afeta o gateway configurado, que sempre liga em `[::]` dual-stack.
3. Com config: valida a porta da interface e faz o preflight de
   disponibilidade; portas P2P, HTTP e de inferência vêm da config.
4. `start_node`, o único caminho de um nó configurado:
   - Identidade `Identity::load_or_create`; chave pkarr em `.pom.key`, ambas
     resolvidas contra `config::project_root()`.
   - Bootstrap = config + `--bootstrap`, ambos passando por
     `config::normalize_peer_addr` em `merged_bootstrap_nodes`; relés =
     `relay_peers` + `--relay-peers`. `--advertise-addr` vence
     `--public-addr`, que vence a config.
   - `start_p2p_stack`: `Network::new` recebe a política TLS inicial e a
     catraca de admissão antes de `start` (receive, gossip 5 s, medição de
     rotas 30 s) e `bootstrap_discovery`. Rede privada prova `connection_key`
     pelo `/handshake`; rede pública exige `cluster_id` e uma
     principal/concessão da cadeia em `.pom-admin-trust.json`. Isso garante que
     a primeira sonda use o mesmo esquema anunciado pela UI e que nenhum
     Announce não autenticado entre em `peers`; a rede pública ainda conserva
     uma `connection_key` local para a UI do administrador, mas ela não é
     compartilhada nem usada na admissão; `--no-discovery` → `detach`.
     `Discovery::new` (libp2p, TCP = UDP+10000) + task de publish no gossipsub
     a cada 5 s (`Network::gossip_announce_bytes`, o mesmo `self_announce` do
     UDP, com ou sem assignment).
   - pkarr: publicação inicial com `r` igual a `--serve-relay` (é o caminho de
     um relé dedicado, `examples/relay-node/`) e as chaves de `--pkarr-peers`
     resolvidas e discadas em segundo plano. O Mainline DHT usado pelo pkarr
     mantém um ator em thread própria; no Windows o timeout ocioso de leitura
     do socket aparece como WSAETIMEDOUT (10060) a cada 50 ms na dependência,
     e `application_log_filter` oculta somente esse aviso esperado.
   - `ClusterLogs` inicia a captura limitada e as tasks best-effort; o
     histórico usa `INFO` como nível mínimo por padrão, aceita
     `cluster_logs_min_level` no `pom.json`, e o principal poda sete dias ou
     500.000 linhas em lotes com contenção não bloqueante. O `Discovery`
     mantém o behaviour request/response de logs separado do túnel de
     inferência e só envia lotes ao principal.
   - Monitor térmico do `NodeRuntime`, depois `GatewayState` +
     `start_gateway_with_ready` spawnado (todo nó serve HTTP/UI). A rota de
     prévia de capacidade faz somente estimativa analítica, com quatro
     admissões por processo e `429` imediato quando todas estão ocupadas.
   - Só com o gateway no ar inspeciona o journal persistente da fatia
     (`lastslice::inspect()`) e restaura somente `Serving` ou `Idle`. Uma
     fatia `Pending` conta tentativas no disco e, depois de
     `MAX_AUTOMATIC_ATTEMPTS=10`, sobe vazia com `note_unloadable_slice`. A
     implantação pela UI chama `NodeRuntime::apply` com o `NodeConfig` dela
     (sobe o servidor de inferência Rust **em-processo** e o supervisa); o
     `NodeConfig` leva também a reserva configurável do ajuste automático de
     memória, e a consulta de VRAM e a aplicação ficam em `inference/llm.rs`.
     Uma GPU acima do limite remove a atribuição e o estado `thermal` aguarda o
     cooldown configurado. Lock, fit ou carga de modelo não podem fechar a
     porta de controle.

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
inválido. Isso faz um arranque que falha sair sozinho sem engolir o erro. Fora do macOS o ponto de
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

`start_setup_mode` e `start_node` estão em `main.rs` (não num módulo
separado). `advertised_ollama_port` (sobre `parse_ollama_port`) extrai a porta do
`--ollama-url` e a anuncia **sempre**.

No Windows, o processo pai supervisiona o filho nativo: uma saída anormal
relança o POM, mas `Sair do POM` retorna código zero e não relança.
`pom undeploy` grava um bloqueio persistente para impedir retomadas
automáticas de uma implantação problemática.

## src/ subpastas

- `network/` - P2P UDP + libp2p/pkarr. Ver `crates/node/src/network/AGENTS.md`.
- `routing/` - planejador do pipeline. Ver `crates/node/src/routing/AGENTS.md`.
- `utils/` - config, respawn, sysmon. Ver `crates/node/src/utils/AGENTS.md`.
- `models/` - perfis de modelo e catálogo de GGUFs: `model.rs`
  (`ModelProfile`, `LayerAssignment`), `catalog.rs` (varredura de
  `model_dirs`; a varredura rápida publica raízes e arquivos sem calcular SHA-256
  de modelos inteiros, deixando a validação de `pom_id` para o enriquecimento),
  `gguf.rs` (leitor de cabeçalho/tensor table), `plan.rs`
  (fatiamento por orçamento/tamanho), `splitter.rs` (split e merge),
  `lastslice.rs` (fatia anotada com estado
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
- `backend/` - HTTP e estado: `gateway/` (`start_gateway_with_ready`, a
  fronteira de `AdminAuth` para `/api/ui/**` e `/admin/**`, e o preflight da porta),
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

`src/plugins.rs` gerencia plugins licenciados e instalações locais independentes:
`PluginManager` cria `PluginRuntime` para códigos concedidos por licença ou
quando um release GitHub local é instalado. Runtimes licenciados persistem em
`plugin_license:<code>` e mantêm seu ciclo de licença/heartbeat. Repositórios
GitHub cadastrados ficam em `plugin_github_repositories` no `pom.db`; o POM lê
`manifest.json` na raiz ou, na ausência dele, `ui/manifest.json` de contrato
`pom-plugin-ui/v1`, sempre diretamente do GitHub e sem consultar o servidor de
licenças ou checar `requires_license`. Essa segunda forma usa os manifestos de
release `pom-plugin-<os>-<arch>.json` para descobrir e validar o binário. A instalação
pública persiste separadamente em `plugin_public_install:<code>`, sem lease ou
`licensed_plugins`. O manifesto do repositório declara `plugin_code`, ABI,
features, preferências, capturas de tela opcionais e nomes dos assets por
plataforma; o operador escolhe qual release instalar. `GithubRepository`
também carrega `description` por idioma e `screenshot_urls` já resolvidas para
`raw.githubusercontent.com/<owner>/<repo>/HEAD/<caminho>`, que o navegador
carrega direto do GitHub. Salvar preferências cria o diretório de
armazenamento absoluto ausente e envia ao host o evento nativo `host.event`
(`preferences.changed`, envelope `pom-plugin-events/v1` montado por
`plugin_event_envelope`). `PluginManager::publish_event` guarda eventos do nó
numa fila curta (`GET /api/ui/plugins/events`) e os entrega por `host.event` a
todo host em execução: `deployment.started` sai dos handlers de deploy e
redeploy, `model.serving`/`model.stopped` de `spawn_serving_watcher`. A ativação em lote de licenças valida uma vez, depois
ativa/instala separadamente cada código concedido. As features de plugin vêm
do manifesto, NÃO da licença. O catálogo público do servidor continua sendo a
vitrine comercial; `GET /api/ui/plugins` publica os estados locais conhecidos
pelo nó.
`PersistedPluginState.installed_release_id` (chave JSON `plugin_release_id`,
preservada por compatibilidade) só avança quando um download é de fato
instalado (`activate_and_install`/`apply_update`) - o heartbeat não escreve
nela, só usa seus próprios `release_changed`/`available_release_id` para
disparar `detect_update` e alimentar `status.available_update` (2026-09-21:
o heartbeat escrevia ali direto e corrompia a base que `detect_update`
compara contra o manifest, fazendo "Atualizar" nunca mostrar a atualização
real). `PluginStatus.update_checked_at` só avança quando a consulta de manifest
termina com sucesso; `installed_at` só avança quando o host fica pronto após
instalação ou aplicação. `active` significa host pronto, não só licença válida;
`state` preserva a validade da licença e `installation_error` publica a falha
acionável de instalação/handshake, persistida por plugin. Esses campos têm
`serde(default)` para estados existentes e não são derivados de
`last_success_at`, que é atualizado por heartbeat. `public_install` identifica um host GitHub sem lease e
`PluginStatus.licensed_plugins` continua vazio nesse caso. `PluginStatus.license_expires_at` é a validade da LICENÇA
(`GET /v1/licenses/status`, consultada em `restore()` e no clique manual em
"Atualizar" via `refresh_license_status`), nunca o `lease_expires_at` técnico -
confundir os dois já vazou para a UI uma vez, ver
`docs/subsystems/enterprise-licensing.md`. Repositórios GitHub são uma fonte
local independente: `plugin_github.rs` valida o manifesto da raiz ou o
manifesto de UI `ui/manifest.json`, a ABI, o alvo, o digest SHA-256 e o tamanho
do asset selecionado. Instalar a release
escolhida não consulta o catálogo ou o servidor de licenças e não cria lease;
um conflito local de `plugin_code` com uma instalação licenciada impede que um
host seja substituído silenciosamente.
`src/plugin_dev.rs` restricts local builds to the allowlist of known
repositories under `/opt/pom/dev-plugins`, validates canonical paths,
manifests and the package's single `cdylib`, and runs fixed `cargo metadata`/
`cargo build` arguments. AdminOnly routes work only with `POM_DESENV=1`; a
build may install a local plugin without a license or prior installation,
persisting its binary and state in `plugin_dev_install:<code>` without granting
licensed features. If a licensed or GitHub installation already exists, the
local artifact replaces its binary without changing entitlement. The flow
restarts only the plugin host and does not query GitHub/licenses or restart the
node.
As preferências opcionais do manifesto são guardadas localmente por
`plugin_preferences:<code>`, fora do snapshot/licença.
O `PluginStatus` também publica a expiração, os plugins concedidos, candidatos
com release elegível e uso autenticado por `plugin_code`, preservados da
validação/ativação/heartbeat. `GET /api/ui/plugins` é aberta a todo perfil; esse JSON não
inclui tokens de licença. O catálogo comercial permanece público e separado.
Todo heartbeat com SUCESSO atualiza a validade da licença e chama
`ensure_host_running` (a mesma lógica da restauração de boot): se o host daquele
plugin estiver `None`, ele tenta subir de novo sem esperar um restart completo
do processo `pom`. Só o host que conclui o handshake torna `PluginStatus.active`
verdadeiro; falha mantém o estado da licença separado e publica
`installation_error`, que a tela de Plugins exibe. `ingest_audit_event` avisa
em log quando descarta um evento por falta de host.

A restauração de boot (`PluginRuntime::restore`) é uma tarefa concorrente que
sobrescreve o estado em memória com o do banco e pode religar o host por conta
própria. Por isso `from_catalog` recebe `restore_on_boot`: produção passa
`true`, e os construtores de teste passam `false`. Um teste que monta estado à
mão perde a corrida contra ela de forma não determinística - foi o que
fazia `heartbeat_success_restarts_the_stopped_plugin_host` falhar ora por
`plugin_path` sumido, ora por host revivido depois da revogação.

`plugin_host.rs` (`PluginHost`) é o único adaptador do `pom` para o processo
auxiliar `pom-plugin-host` - já suporta várias instâncias concorrentes, uma
por plugin ativo. `PluginHost::start` passa o `plugin_code` para o binário
auxiliar, que resolve `pom_<plugin_code>_plugin_v1` e usa `POM_PLUGIN_DB`.
`PluginRuntime::plugin_database_path` deriva o arquivo do diretório do banco do
POM: `enterprise` recebe `logs.db` (em Docker, `/data/logs.db`) e outros
plugins recebem `<plugin_code>.db`; não há volume ou variável Enterprise
separados. O host usa `kill_on_drop` e mata explicitamente
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
O manifesto pode declarar `icon_image` como PNG do menu,
`documentation` com guias e referência Markdown sob `docs/`, `description` por
idioma, `screenshots` (caminhos do repositório ou `https://`, nunca servidos
pelo nó) e `menu[].children` para submenus; tipos MIME e declarações são
conferidos antes de servir.
Menus e rotas aceitam `roles` opcionais; `profiles` continua aceito como campo
legado, com `roles` prevalecendo quando ambos aparecem. A ausência dos dois
libera todos os perfis antes da regra de acesso.
`PluginRuntime::ui_manifest` reaproveita `query_audit`. Contrato completo em
`docs/subsystems/plugins-ui.md`.

Todo host de plugin recebe o contexto do nó pela operação `host.configure` do
`query`: `{"gateway": {"openai_base_url", "api_key"}, "workspace_root": "..."}`.
`workspace_root` é a raiz absoluta e gravável compartilhada por projetos e
dados dinâmicos namespaced de plugins. `plugins::spawn_host_context`
(iniciado no `main.rs` ao montar o gateway) monta esse JSON com
`http://127.0.0.1:<gateway_port>/v1` e a chave de chat do nó
(`admin::node_chat_key`, a mesma do Chat da interface, porque é a única classe
de chave que um nó sem autoridade registra no principal) e o reavalia a cada
10 min, ou a cada 30 s enquanto a chave não sai. `PluginManager::set_host_context`
reentrega aos hosts em execução quando o contexto muda, e `PluginRuntime::start_host`
o entrega a todo host que sobe. Plugin que não conhece a operação responde
erro, e isso é ignorado. Nada disso vai por variável de ambiente nem argv.

`src/plugin_proxy.rs` é o proxy reverso `/api/ui/plugins/:code/proxy/*` das
interfaces web que um plugin serve em loopback: pergunta `ui.upstream` ao host
(`{"status":"ready","port","token"}`), encaminha HTTP em streaming e upgrades
(WebSocket; por isso `serve.rs` usa `serve_connection_with_upgrades`) para
`127.0.0.1:<port>` com `x-pom-plugin-token` e `x-forwarded-prefix`, e nunca
repassa cookie, `Authorization` ou `x-pom-access-token` nem aceita `Set-Cookie`
do plugin. A autorização é a da superfície administrativa (aberta a todo perfil), a
falsificação entre sites é barrada por `same_origin`, e chamadas entre nós são
recusadas.

`src/pom_core_update.rs` é deliberadamente SEPARADO de `src/plugins.rs`: a
atualização do binário `pom` em si não é um plugin e vai virar GitHub Releases
público sem licença (outra tarefa). Até essa migração, `PomCoreUpdater`
empresta o lease do plugin `enterprise` para autenticar o manifest genérico
`pom_core` e, só com uma confirmação explícita da UI, delega decisão, hash,
troca atômica e reinício a `core_update.rs` - que continua puro e sem
dependência de licenciamento. A fronteira e os limites restantes estão em
`docs/subsystems/enterprise-licensing.md`.

## Ligações

- **Depende de**: `routing-contract`, `llama-engine` e `brand` (crates irmãos);
  `config` é um módulo interno de `node`.
- **Chamado por**: ninguém (é o binário). O servidor de inferência e o
  splitter são Rust, em-processo.

## Defeitos anotados

Nenhum em aberto nesta pasta.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
