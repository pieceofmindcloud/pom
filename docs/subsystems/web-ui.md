# Web UI

Interface web administrativa do POM, um SPA React 18 + TypeScript + Vite
em `apps/frontend/`. É a interface do produto: todo nó serve a UI junto com a
API na porta do gateway (`--gateway-port`, 8080 por padrão), então abrir
`http://<peer>:<porta>/` em qualquer máquina mostra o painel daquele nó.

O nome e a logo que a tela mostra vêm do `brand.json` da raiz, copiado para o
bundle por `scripts/build-brand.mjs` - o mesmo arquivo que o binário embute.
`Logo.tsx` desenha a marca de `/brand/pom-mark.png` e escreve nome e slogan a
partir dele; o título da aba e os ícones entram no `index.html` pelo plugin
`brandHtml` do `vite.config.ts`. Nenhum dos dois tem o nome escrito à mão.

## Como a interface sobe

- **O bundle é lido do disco a cada requisição**, não embutido no binário
  (`crates/node/src/backend/webapp.rs`). `cargo build` nunca toca este
  diretório; editar a UI só exige `npm run build` em `apps/frontend` (e
  recarregar o navegador). O comentário no topo de `build.rs` registra a mesma
  separação: build de UI e build Rust são passos independentes.
- O diretório é decidido por `config::ui_dist_dir()` (`crates/node/src/utils/
  config.rs`): `$POM_UI_DIST` se preenchida, senão
  `<project_root>/apps/frontend/dist`. A imagem Docker usa a variável porque
  `POM_HOME=/data` não é o `/app` da imagem; dev que guarda o bundle em
  outro lugar aponta direto nela.
- Sem `index.html` no diretório o nó sobe normalmente e responde **500** nas
  rotas de UI, com a mensagem dizendo onde procurou e o comando de remédio
  (`npm run build` ou apontar `POM_UI_DIST`). `dist/` e `dist-release/`
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
`src/App.tsx`. Dentro de `Modelos`, a ordem é `Modelos inteiros`, `Fatiar
modelos`, `Fatias`. O menu lateral pode ser minimizado: nesse estado mostra
somente ícones, e as seções com folhas exibem um submenu ao passar o mouse ou
receber foco. A escolha fica persistida no navegador. O `App` decide setup vs painel com `useIsSetupMode()` (probe de
`/api/ui/node` com `auth: false`; 404 = só existe o router de setup). Rotas:

- `/` -> Dashboard; `/chat` -> Chat (fora do `<Routes>`/transição porque é
  dono de um stream vivo e do scroll; desmontar para animar derrubaria tokens)
- `/modelos` -> WholeModels; `/modelos/fatiar` -> SliceModels, com o painel de
  jobs e progresso de fatiamento embutido
- `/fatias` -> Shards;
  `/fatias/implantar` -> redireciona para `/implantar` (link antigo)
- `/implantar` -> DeployModel (entrada de primeiro nível para pôr fatias em
  serviço)
- `/implantacoes` -> Deployments (registro das implantações, ativação da padrão,
  reconfiguração de parâmetros e desimplantação por `deploy_id`)
- `/transferencias` -> Transfers; `/transferencias/enviar` -> SendFile
- `/logs` -> LogsByToken; `/logs/ao-vivo` -> LogsLive
- `/documentacao` -> Documentation (tela cheia; ver a seção própria abaixo)
- `/config/rede` -> NetworkSettings; `/config/keys/api` -> TokenKeys;
  `/config/keys/conexao` -> ConnectionKeys; `/config/armazenamento` -> ModelPaths
  (`/config/pastas` continua como alias)
- `*` -> NotFound

A **Topbar** traz, antes do sino de notificações, o selo de criptografia do
cluster (`TlsChip`): `ShieldCheck` em `aqua` com "TLS" quando a política está
ligada, `ShieldOff` em `amber` com "sem TLS" quando não, e um link para
`/config/rede`. Ele fica em toda tela porque "o cluster está cifrado?" é uma
pergunta que o operador faz a qualquer momento, e uma virada pela metade -
enquanto os peers ainda não adotaram a política nova - precisa ser vista sem
ninguém ir procurar. O dado sai do `useNode()`, que já é polled: nenhuma chamada
nova. Em tela estreita fica só o ícone. **Um selo que sumisse quando o TLS
caísse seria pior que nenhum**: a ausência se lê como "a tela não mostra isso",
não como "está em texto claro" - por isso ele diz os dois estados.

Sidebar e Topbar vivem em `ErrorBoundary` próprio, então uma tela quebrada não
derruba o menu. O **cabeçalho** da Sidebar traz só o lockup da marca, numa faixa de borda a
borda (`LogoFull variant="band"`) fechada por uma linha divisória. O **rodapé**
traz o idioma em **bandeiras** (`brand.flags`, do `brand.json`) em vez de
"EN"/"PT-BR" - com o nome do idioma no `aria-label`/`title`, porque bandeira não
é idioma para leitor de tela -, o `build_commit` do nó (só o hash, em mono, com
a explicação no tooltip) e o tema claro/escuro. O PipelineRail ainda é renderizado oculto
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
propósito: só o modo setup (sem `pom.json`) as serve, porque `GET
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

## Configurações / Rede: quem decide o TLS

O painel "Criptografia entre nós" (`TlsPanel`, em `routes/NetworkSettings.tsx`)
mostra o estado do cluster e, **só no nó principal**, o interruptor que o vira.
Nos demais ele aparece desabilitado, com um rodapé dizendo que a decisão é do
principal e vale para a rede inteira. O estado continua visível em todo nó de
propósito: esconder o painel faria o operador de um nó comum não ter onde ver se
o tráfego da máquina dele está cifrado.

A autoridade que vale é a do servidor - `POST /api/ui/network/tls` responde 403
fora do principal (ver `api-http.md`). A tela usa `NodeInfo.tls_authority` só
para não mentir antes do clique.

Virar não pede reinício e vale nos dois sentidos; o toast diz que os demais nós
adotam em poucos segundos, que é o ciclo de gossip mais a busca autenticada da
catraca (ver `rede-p2p.md`, seção 5).

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

## Documentação: dois públicos na mesma rota

`apps/frontend/src/routes/Documentation.tsx`, com as abas em `?tab=`.

- **Guias** (`src/guides/`) - a metade escrita para humanos: páginas ordenadas
  com diagramas SVG, geradas de dados (`Block[]`, `content.ts`) por
  `GuideView`. Cinco páginas hoje: o que é a plataforma, o caminho de uma
  pergunta, do modelo ao cluster servindo, operar e diagnosticar, acesso e
  exposição. É a aba padrão: um humano não pode aterrissar em
  `AGENTS.md - crates/routing-base/`. Nenhum número aparece ali sem estar no
  markdown - quando divergirem, o markdown vence.
- **Referência** - o markdown vivo do repositório, escrito para agentes,
  embutido no bundle por `apps/frontend/scripts/build-docs.mjs` (não há rota no
  nó; a tela lê `manifest.json` + `import.meta.glob`).

Três decisões que não são estética:

- **A tela é de largura total** (`App.tsx` lista `/documentacao` junto de `/`
  em `isFullBleed`) e rola por dentro: a coluna de navegação e a de leitura são
  filhos flex com `min-h-0`, cada uma com o próprio scroll. Sem esse `min-h-0`
  o filho cresce com o conteúdo e o overflow nunca engata - era assim que a
  busca corria para fora da tela sem barra de rolagem.
- **Nome de arquivo não aparece na navegação.** O manifesto carrega `label`
  (nome humano curto) e `kind`: o `AGENTS.md` de uma pasta é a **visão geral
  daquela seção**, então o cabeçalho da seção é que o abre (clicar em "docs/"
  mostra `docs/AGENTS.md`) e ele nunca é listado como folha. O leitor também
  descarta o `# ` do próprio documento, que só repetiria o título do cabeçalho
  em outra redação.
- **Recolher funciona.** O chevron alterna, o rótulo abre a visão geral, e a
  seção que contém o documento aberto não é mais forçada a reabrir - esse era
  o defeito: categoria que ficava sempre expandida.

## A interface com o cluster em TLS

Com a política ligada, abrir `http://<peer>:8080/` devolve **301 para
`https://`** na mesma porta, e o painel funciona normalmente a partir daí. O
certificado que o navegador recebe é um ECDSA auto assinado, diferente do
Ed25519 que os peers recebem - navegador não verifica Ed25519. Ver
`rede-p2p.md`, "Dois certificados na mesma porta".

**O navegador avisa na primeira visita de cada máquina**, porque o certificado é
auto assinado e não há CA para validá-lo. É esperado; o operador aceita uma vez
por máquina.

## O canvas do Fluxo: arranjo por camadas

`ModelNetworkCanvas` desenha a rede, e `lib/networkLayout.ts` decide onde cada
card fica. A regra é **uma coluna por faixa de camadas, na ordem do pipeline, e
réplicas da mesma faixa empilhadas dentro da coluna, centradas no mesmo eixo**
(`autoLayout`). É isso que faz a bifurcação parecer uma bifurcação e a aresta
virar uma linha curta.

A precedência é: posição arrastada > arranjo calculado > grade
(`fallbackPosition`, que só sobra para um card que o arranjo não conheça).
**Uma implantação nova já nasce arrumada**, sem ninguém clicar em nada.

Dois botões, e a diferença entre eles importa:

- **Organizar** (`LayoutGrid`) - aplica o arranjo por camadas **mantendo quem o
  operador arrastou**. Um arranjo feito à mão é uma decisão, e o botão que
  apagasse decisões seria o botão que ninguém aperta.
- **Redefinir** (`RotateCcw`) - descarta tudo, inclusive os arrastos.

`autoLayout` é estável quanto à ordem de entrada (desempate por `start_layer`,
`end_layer`, `id`): a lista de peers se reordena a cada poll do cluster, e um
arranjo que dependesse dela tremeria na tela.

**Defeito corrigido:** `deploymentLayoutKey` incluía o `changed_at` de cada
peer. Ele muda a cada reinício de nó, o que mudava a chave, o que fazia
`readPositions` devolver vazio - o arranjo do operador não era reorganizado, era
**perdido**, e a tela caía na grade toda vez que uma máquina reiniciava. Hoje a
impressão digital é só modelo + conjunto de faixas.

## Fatiar, implantar e transferir na tela

- **Fatiar** (`SliceModels`): plano por tamanho (alvo em GB por fatia, vazio =
  "o resto"). Cada tecla posta `POST /api/ui/slice/plan` (debounce 260 ms) e
  mostra alvo vs faixa/tamanho reais lado a lado; o atalho "dividir em N
  iguais" passa pelo mesmo plano. `POST /api/ui/slice` roda o job e mantém a
  tela aberta; `SlicingJobsPanel` mostra abaixo a barra de progresso, estado e
  saídas de cada job consultado por `GET /api/ui/jobs`. `asPlan` valida o corpo
  do POST para um build velho não derrubar a tela. Só modelos inteiros
  (`isWholeModel`) podem ser fatiados.
- **Implantar** (`DeployModel`): abre o Deploy Studio em tela ampla, com abas de
  Fatias/Modelo inteiro/Tensor, modelo técnico e nome operacional separados,
  estratégias Simples/Completa, mapa central e inspetor compacto de propriedades.
  Todos os peers aparecem na faixa inferior e podem ser arrastados para o mapa;
  o planner recalcula a sugestão considerando cortes existentes, memória, RTT e
  carga. A barra de cobertura mostra camadas faltantes e sobrepostas; o mapa
  exibe hops, capacidade e latência conhecida, além de rotas alternativas e
  distribuição projetada de carga. O Tensor usa o modelo inteiro em todos os
  ranks e não apresenta a seleção antiga de fatias. As propriedades globais
  (contexto, batch, amostragem e sessões) ficam separadas das propriedades por
  máquina (`n_gpu_layers`, KV e capacidade local); ao selecionar uma máquina, o
  inspetor também mostra as camadas que ela executará. Não existe uma segunda
  tabela legada abaixo do canvas: o mapa e a faixa inferior concentram a
  topologia, os RTTs direcionais e a inclusão de peers. Cada edição volta ao nó para validação; o deploy
  (`POST /api/ui/deploy`) **re-varre o cluster** em vez de confiar na prévia,
  e o progresso vem de `GET /api/ui/jobs`. O mapa usa os mesmos primitives do
  canvas do Painel: pontos no fundo, cards arrastáveis, zoom, Fit, Organizar e
  Redefinir, com posições persistidas por implantação. O inspetor tem 19rem,
  pode ser recolhido pela borda esquerda, separa visualmente propriedades
  globais e da máquina selecionada, e permite atribuir ao peer uma faixa que
  ele já possui trocando a faixa ativa daquele peer sem mover outra máquina. A mesma faixa em dois
  peers é uma rota paralela válida, não uma troca. Peers que já estão no mapa
  deixam de aparecer na barra inferior; arrastar ou dar duplo clique num peer
  disponível adiciona o card sem redesenhar a topologia até que uma faixa seja
  atribuída. Em modelo inteiro, incluir um peer no mapa cria uma réplica explícita
  da faixa completa; a mesma faixa em peers diferentes é aceita pelo planner e
  pode ser escolhida pelo roteador por RTT/carga. No Tensor, somente peers que
  estão no mapa entram como primary/worker, mesmo que outros tenham o modelo.
  GPU, RAM e carga são atualizados pela visão do cluster a cada 5 segundos; os
  RTTs vêm do survey direcional do planner e não são substituídos pela medição
  HTTP do gateway. Uma GPU ausente aparece como `sem GPU`, sem barra
  indeterminada. Em Completa, todos os peers entram no canvas e na lista de candidatos. A
  cobertura fica reduzida no rodapé junto da barra de peers.
  A última implantação de cada modelo
  fica no `localStorage` (`deployMemory`) e volta preenchida após o primeiro
  plano, podada contra o que ainda existe.
  **O botão cinza sempre diz por quê.** A comparação entre o plano do nó e as
  linhas da tela (`sameRows`) ordena os dois lados por camada, porque a tela
  mantém as linhas nessa ordem e o nó monta a resposta do jeito dele; e os
  painéis de explicação renderizam sob `answered` (o nó respondeu sobre *estas*
  linhas), nunca sob `validated`. Pendurar a explicação na comparação foi o
  defeito de 2026-08-16: no estado em que o operador mais precisava do motivo -
  duas faixas na mesma máquina - a tela ficava muda e o botão só cinza. `whyNot`
  ainda põe a razão em uma linha ao lado do botão, inclusive nos dois casos sem
  painel (nada selecionado, resposta a caminho).
- **Transferir** (`Transfers`/`SendFile`/`Notifications`): ofertas pendentes
  (aceitar/rejeitar direto no sino), transferências ativas (bytes, velocidade,
  ETA, cancelar) e concluídas. O envio ("enviar um arquivo desta máquina")
  oferece qualquer caminho legível via `POST /api/transfer/send`; o receptor
  só vê uma concessão opaca (`grant`), então "mandar qualquer coisa" nunca
  vira "ler qualquer coisa". O progresso vem dos nós, então recarregar a aba
  não perde nada. `POM_DEV_AUTO_ACCEPT_TRANSFERS=1` (variável do processo
  receptor, lida no arranque) aceita toda oferta sem clique - só dev.
  Peers da mesma máquina compartilham o hostname, então o destino e a etiqueta
  de disponibilidade levam o começo do `node_id` quando o nome se repete
  (`uniqueName`, `routes/Shards.tsx`).

## Defeitos anotados

- `AddNodeDialog` re-semeia as escolhas **na transição para aberto**, não a cada
  props nova: o Painel remonta `peers`/`slices` a cada polling do cluster, e um
  efeito dependente desses arrays descartava a escolha do operador no meio da
  edição (a combo "voltava" para outra máquina sozinha). Um segundo efeito só
  troca a escolha que sumiu da lista; a fatia é identificada pela faixa
  (`keyOf`), não pelo índice. Testes em `AddNodeDialog.test.tsx`.
Nenhum outro em aberto nesta tela.
- DEFEITO ANOTADO: `LeaveNode` reusa o rótulo do gatilho no botão de
  confirmar - a janela mostra dois botões com o mesmo texto ("Sair da rede").
  Cosmético, mas um usuário pode confundir o gatilho com a confirmação.
