# Web UI (CODAI-SERVER)

Interface web administrativa do rdma-p2p, um SPA React 18 + TypeScript + Vite
em `apps/frontend/`. É a interface do produto: todo nó serve a UI junto com a
API na porta do gateway (`--gateway-port`, 8080 por padrão), então abrir
`http://<peer>:<porta>/` em qualquer máquina mostra o painel daquele nó. A
marca é só da interface; crate, binário, config (`rdma-p2p.json`) e rotas
continuam `rdma-p2p`.

## Como a interface sobe

- **O bundle é lido do disco a cada requisição**, não embutido no binário
  (`crates/node/src/backend/webapp.rs`). `cargo build` nunca toca este
  diretório; editar a UI só exige `npm run build` em `apps/frontend` (e
  recarregar o navegador). O comentário no topo de `build.rs` registra a mesma
  separação: build de UI e build Rust são passos independentes.
- O diretório é decidido por `config::ui_dist_dir()` (`crates/node/src/utils/
  config.rs`): `$RDMA_P2P_UI_DIST` se preenchida, senão
  `<project_root>/apps/frontend/dist`. A imagem Docker usa a variável porque
  `RDMA_P2P_HOME=/data` não é o `/app` da imagem; dev que guarda o bundle em
  outro lugar aponta direto nela.
- Sem `index.html` no diretório o nó sobe normalmente e responde **500** nas
  rotas de UI, com a mensagem dizendo onde procurou e o comando de remédio
  (`npm run build` ou apontar `RDMA_P2P_UI_DIST`). `dist/` e `dist-release/`
  são artefatos locais ignorados pelo git.

### Roteamento no axum (`webapp.rs`)

O bundle é montado por último no roteador e o fallback é estreito de propósito:
arquivo real ganha (com `mime_guess`); prefixos de API (`/api`, `/v1`,
`/admin`, `/peers`, `/health`, `/generate`) **nunca caem no app** (rota de API
desconhecida responde 404 em JSON, não uma página com 200); um miss com
extensão é 404 (`asset não encontrado`); o que sobra recebe `index.html`, para
recarregar o navegador em `/config/keys/api` funcionar. `resolve()` rejeita
`..`, caminho absoluto e `Prefix` (há teste). Cache: `index.html` com
`no-cache` (é o único arquivo que muda sob nome fixo), assets com hash de
conteúdo `immutable` por 1 ano.

## Build e desenvolvimento

- Scripts `package.json`: `dev` (`vite --port 5199 --strictPort`),
  `build` (`tsc -b && vite build`, type-check via project references),
  `preview` (`vite preview --port 5199 --strictPort`), `test` (`vitest run`).
- Vite 6 + `@vitejs/plugin-react`; Tailwind 3.4 via PostCSS; React 18;
  TypeScript 5.7. `cacheDir` pinado em `node_modules/.vite` porque o fallback
  do Vite perde a config em silêncio (sem PostCSS, sem proxy) quando um
  `git rebase`/troca de branch reinicia o dev server no meio da reescrita.
- **Não há backend de exemplo**: toda tela lê um nó real. Em desenvolvimento o
  dev server encaminha `/api`, `/v1`, `/admin`, `/peers`, `/health` e
  `/generate` para `VITE_API_TARGET` (padrão `http://127.0.0.1:8080`; aponte
  para um nó de verdade, ex. `VITE_API_TARGET=http://192.168.100.138:8090`).
  Um nó destacado (`--no-discovery`) é o alvo de desenvolvimento legítimo.
- Vitest 3: sem arquivo de config; testes que precisam de DOM optam por arquivo
  com `// @vitest-environment jsdom`.

## Menu e rotas

`src/components/nav.ts` tem o menu como dados (`NAV`: seções e folhas com
`labelKey`, rota e `end`), consumido pela Sidebar e pelo roteador de
`src/App.tsx`. O `App` decide setup vs painel com `useIsSetupMode()` (probe de
`/api/ui/node` com `auth: false`; 404 = só existe o router de setup). Rotas:

- `/` -> Dashboard; `/chat` -> Chat (fora do `<Routes>`/transição porque é
  dono de um stream vivo e do scroll; desmontar para animar derrubaria tokens)
- `/modelos` -> WholeModels; `/modelos/fatiar` -> SliceModels
- `/fatias` -> Shards; `/fatias/andamento` -> SlicingJobs;
  `/fatias/implantar` -> redireciona para `/implantar` (link antigo)
- `/implantar` -> DeployModel (entrada de primeiro nível para pôr fatias em
  serviço)
- `/transferencias` -> Transfers; `/transferencias/enviar` -> SendFile
- `/logs` -> LogsByToken; `/logs/ao-vivo` -> LogsLive
- `/config/rede` -> NetworkSettings; `/config/keys/api` -> TokenKeys;
  `/config/keys/conexao` -> ConnectionKeys; `/config/pastas` -> ModelPaths
- `*` -> NotFound

Sidebar e Topbar vivem em `ErrorBoundary` próprio, então uma tela quebrada não
derruba o menu. O rodapé da Sidebar mostra EN/PT-BR, tema claro/escuro e o
`build_commit` do nó (de `useNode`). O PipelineRail ainda é renderizado oculto
no Dashboard (`hidden`, para uma superfície futura).

## Arquitetura do frontend

`src/` tem quatro subpastas, cada uma com seu `AGENTS.md` (mapa autoritativo):

- **`src/lib/`** - cliente HTTP, hooks react-query, persistência em
  `localStorage`, stream/estado do chat, formatação, toasts e todos os tipos.
  `api.ts` é o único caminho de fetch: caminho **relativo** (o nó serve API e
  UI na mesma porta, o navegador nunca aprende endereço de peer), assinado com
  `Authorization: Bearer <connectionKey>`, e também o leitor de SSE
  (`streamSse`). `queries.ts` concentra as leituras com polling: `useNode` 4s
  (guarda a `connection_key`), `useCluster` 5s, catálogo local 30s, catálogo
  do cluster 60s, `useJobs`/`useTransfers`/`useOffers` 2s, admin nodes 8s,
  log de requisições 3s. `optional()` engole só 404 (nó antigo sem rota não é
  erro para mostrar). `types.ts` é a fonte única das formas que o nó serve
  (comentários apontam para o Rust).
- **`src/components/`** - chrome (`Sidebar`, `Topbar`, `Notifications`, `nav`)
  e widgets compartilhados. `ui.tsx` é o design system num arquivo de propósito
  (painéis, botões, inputs, badges, meters, estados) em classes por token CSS,
  sem estilo em runtime. Visualizações do cluster: `ModelNetworkCanvas` (grafo
  SVG pan/zoom com RAM/VRAM/carga e arestas principal/paralela/inativa,
  posições persistidas por fingerprint de implantação) e `PipelineRail` (o
  caminho que a requisição percorre). Guardas de ação remota/destrutiva:
  `Confirm`/`Modal`, `ReleaseSlice`, `RemoveNode`, `LeaveNode`,
  `UndeployPipeline`, `AddNodeDialog`.
- **`src/i18n/`** - en + pt-BR em `locales.ts` (≈620 chaves por idioma, uma
  tabela plana com `Message = (values, locale) => string`, pluralização via
  `Intl.PluralRules`). `provider.tsx` expõe `useI18n().t` e o
  `LanguageSelect`. `index.html` tem script pré-paint que carimba `lang` do
  `codai.locale`/locale do navegador, para o primeiro render já estar no
  idioma certo. `localizeServerText` mapeia strings em português que um
  binário antigo possa enviar para o idioma da UI.
- **`src/routes/`** - uma tela por rota, todas por react-query falando com o
  gateway HTTP do nó; nenhuma guarda estado do cluster no navegador. As telas
  principais estão descritas abaixo.

## Autenticação

`GET /api/ui/node` é a única rota `/api/ui/*` sem `ConnectionAuth`: ela devolve
a `connection_key` do cluster, que o `useNode()` guarda em `localStorage`
(`codai.connectionKey`) e a partir daí todo request assina com
`Authorization: Bearer <key>` - o mesmo `ApiKeyAuth`/`ConnectionAuth` do
`auth.rs`, que também aceita `x-api-key`. A chave de conexão vale como chave de
API nas rotas `/v1` e como credencial de operador em `/admin/*`. `POST
/api/setup/key` e `/api/setup/save` **não** estão no router normal de
propósito: só o modo setup (sem `rdma-p2p.json`) as serve, porque `GET
/api/setup/key` rotacionaria a credencial num nó já configurado.

## Janelas modais: portal e pilha de Esc

`Modal`/`Confirm` (`src/components/Confirm.tsx`) vão por `createPortal` para
`document.body`. Não é preferência: `position: fixed` deixa de valer contra a
janela quando algum ancestral tem `transform`, `filter` ou `backdrop-filter`
(esse ancestral vira o bloco de contenção); a barra do topo e o cabeçalho do
Painel têm `backdrop-blur`. `Modal` mantém uma pilha dos abertos e só o de
cima responde ao Esc, porque uma janela pode abrir outra (a confirmação de
sair da rede nasce de dentro do card). Há teste (`Confirm.test.tsx`) que
afirma a causa (a janela é filha do `body`) porque o jsdom não calcula layout.

## O Painel: Fluxo e Detalhes mostram a mesma máquina

`PeerDetailCard` (`src/components/PeerDetailCard.tsx`) é o card de uma
máquina, e é **um só**: a aba Detalhes empilha um por máquina e o duplo clique
num card do Fluxo abre o mesmo componente numa `PeerDetailDialog`. Duplicar a
apresentação nos dois lugares é o que se está evitando.

Detalhes lista a **rede inteira**, não a pipeline: `networkPeers`
(`src/lib/networkPeers.ts`) separa quem serve o modelo ativo na ordem das
camadas (`pipelinePeers`) de quem está conectado sem fatia (`others`, por
nome). Máquina sem fatia é `model` vazio com `total_layers` zerado
(`hasSlice`), **não** uma faixa "0-0", e o card não escreve faixa nenhuma
nesse caso. Cada card tem as duas saídas lado a lado (`RemoveNode` - sai do
modelo, só quando serve; `LeaveNode` - sai da rede, sempre) e o botão de
atualizar/reiniciar só quando o peer anuncia `dev_update`. Ao lado de
"Adicionar nó" há `UndeployPipeline` quando há modelo no ar. `PipelineRail`
ainda exporta `pipelinePeers`/`trackOf` usados pelo Dashboard e DeployModel.

## Chat de teste

`src/routes/Chat.tsx` fala com o cluster por `POST /v1/chat/completions` com
`stream: true`, igual a qualquer harness - o que aparece ali é o que um
harness receberia. O histórico é do navegador (`chatStore`, `localStorage`,
cap 50 sessões, sincronizado entre abas), não do nó. Três coisas a conversa
guarda além das falas:

- **A telemetria da execução chega fechada** (`ExecutionTelemetry`): quatro
  linhas de números por salto, embaixo de toda resposta; abrir é um clique.
- **Trocar de modelo no meio vira um evento no fluxo** (`role: "event"`, que
  `turnsOnly` filtra antes de a conversa viajar na requisição). O seletor fica
  embaixo do campo de texto.
- **O título se edita clicando nele** e fica gravado (`titled` impede a
  primeira mensagem de sobrescrever o nome escolhido).

`chatStream` separa `<think>` do conteúdo visível sem vazar o delimitador
entre pedaços; `finishChatStream` grava tokens/s, duração e o trace por hop.

## Fatiar, implantar e transferir na tela

- **Fatiar** (`SliceModels`): plano por tamanho (alvo em GB por fatia, vazio =
  "o resto"). Cada tecla posta `POST /api/ui/slice/plan` (debounce 260 ms) e
  mostra alvo vs faixa/tamanho reais lado a lado; o atalho "dividir em N
  iguais" passa pelo mesmo plano. `POST /api/ui/slice` roda o job e navega
  para `/fatias/andamento`. `asPlan` valida o corpo do POST para um build
  velho não derrubar a tela. Só modelos inteiros (`isWholeModel`) podem ser
  fatiados.
- **Implantar** (`DeployModel`): escolhe um conjunto de fatias, edita quais
  faixas vão para qual máquina (troca em colisão, `merge` para faixas
  adjacentes numa máquina), reordena trocando máquinas, e afina os onze knobs
  de runtime do cluster mais `n_gpu_layers`/`offload_kqv` por máquina
  (propriedade do hardware). Cada edição volta ao nó para validação; o deploy
  (`POST /api/ui/deploy`) **re-varre o cluster** em vez de confiar na prévia,
  e o progresso vem de `GET /api/ui/jobs`. A última implantação de cada modelo
  fica no `localStorage` (`deployMemory`) e volta preenchida após o primeiro
  plano, podada contra o que ainda existe.
- **Transferir** (`Transfers`/`SendFile`/`Notifications`): ofertas pendentes
  (aceitar/rejeitar direto no sino), transferências ativas (bytes, velocidade,
  ETA, cancelar) e concluídas. O envio ("enviar um arquivo desta máquina")
  oferece qualquer caminho legível via `POST /api/transfer/send`; o receptor
  só vê uma concessão opaca (`grant`), então "mandar qualquer coisa" nunca
  vira "ler qualquer coisa". O progresso vem dos nós, então recarregar a aba
  não perde nada. `RDMA_P2P_DEV_AUTO_ACCEPT_TRANSFERS=1` (variável do processo
  receptor, lida no arranque) aceita toda oferta sem clique - só dev.

## Defeitos anotados

- DEFEITO ANOTADO: o doc do módulo `crates/node/src/backend/webui.rs` (linhas
  1-9) afirma que os assets da UI são compilados no binário com `include_str!`.
  Não existe `include_str!` em lugar nenhum do backend: o bundle é lido do
  disco por `webapp.rs` em `config::ui_dist_dir()`. O doc engana quem procurar
  de onde a UI sai.
- DEFEITO ANOTADO: strings em português fixas no código furam o i18n. Com o
  idioma EN ativo continuam em português: `deleteModel.ts` e `toast.tsx`
  (confirmações de apagar, `aria-label`), `Shards.tsx`/`WholeModels.tsx`/
  `Transfers.tsx` ("Excluir", "apagado", "Cancelar", "enviando para" ...),
  a tabela inteira de telemetria do Chat (`ExecutionTelemetry`) e os botões/
  estados vazios do `ModelNetworkCanvas`. Cosmético, mas inconsistente com o
  resto do app.
- DEFEITO ANOTADO: `AddNodeDialog` nunca reinicializa as escolhas a partir das
  props (`nodeId`/`sliceIndex` semeados uma vez no primeiro `peers[0]`/primeira
  fatia, dialog sempre montado com `open` controlado). Reabrir depois que um
  peer saiu pode submeter um `node_id` que não está mais no dropdown.
- DEFEITO ANOTADO: `networkLayout.ts` escreve `localStorage` sem `try/catch`
  (`writePositions`/`clearPositions`), diferente de todo helper de storage da
  pasta - exceção de quota/segurança lançaria do handler de drag do canvas.
- DEFEITO ANOTADO: `ModelNetworkDemo.tsx` é código morto (nada o importa) e
  mantém cópia própria da lógica de layout/medidores, que vai divergir do
  canvas real.
- DEFEITO ANOTADO: `transferStatus.ts` exporta o tipo `TransferState` que nada
  fora do módulo usa (os predicados tipam em `TransferJob["state"]` direto).
- DEFEITO ANOTADO: `LeaveNode` reusa o rótulo do gatilho no botão de
  confirmar - a janela mostra dois botões com o mesmo texto ("Sair da rede").
  Cosmético, mas um usuário pode confundir o gatilho com a confirmação.
