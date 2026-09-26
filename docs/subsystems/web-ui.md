# Web UI

Interface web administrativa do POM, um SPA React 18 + TypeScript + Vite
em `apps/frontend/`. É a interface do produto: todo nó serve a UI junto com a
API na porta do gateway (`--gateway-port`, 8080 por padrão), no mount
`/admin-ui/`; a raiz apenas redireciona para ele. O painel pode nascer somente
em `localhost` ou ser exposto à rede com token e sessão de servidor, enquanto
as rotas de inferência continuam dual-stack para o HTTP de reserva da pipeline.

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

### Chat compacto da barra de menus do macOS

O bundle macOS mantém o processo Rust como dono do gateway e acrescenta um
`NSStatusItem` em `crates/node/src/macos_status.m`. O clique abre o mesmo SPA
em `/admin-ui/chat`, sem sidebar nem topbar, num `WKWebView` de 390 x 844
points - a tela lógica de um iPhone 14/15. Num ecrã que não comporte essa
altura o painel encolhe para o que cabe; a largura de 390 é fixa. O botão
`Janela maior` só aparece quando a ponte WebKit existe, e pede ao processo
nativo uma janela redimensionável de 1120 x 880 points, maior que o painel nos
dois eixos. No navegador comum esse botão não é renderizado.

O bundle instala também o menu comum do aplicativo macOS no AppKit, com Sobre e
Sair. A ação Sair chama o encerramento padrão do aplicativo e termina o processo
inteiro, incluindo o serviço Tokio que fica na thread de fundo.

Quem escolhe as medidas e o endereço é o Rust (`macos_status.rs`), que os
entrega ao Objective-C no arranque: o lado nativo não conhece a configuração do
nó, e duplicar os números nos dois lados seria convite a desencontro.

Quando o serviço Rust termina, a ponte para e acorda o loop do AppKit sem
encerrar o processo. Assim `macos_status.rs` consegue esperar a thread de
serviço e devolver ao shell a mensagem e o código de saída de uma configuração
inválida, em vez de sair silenciosamente com status 0.

### Janela e bandeja do Windows

No Windows, o mesmo binário usa `windows_subsystem = "windows"`, então a
interface não abre um console separado. `crates/node/src/windows_status.rs`
cria uma janela Tao e um WebView2 real via Wry, carregando o mesmo
`/admin-ui/chat` e a porta escolhida pelo Rust. O ícone da área de notificação
usa a arte POM embutida no binário; o menu `Abrir POM` restaura e foca a janela.

O X da janela não encerra o processo: ele a esconde na bandeja, mantendo o
gateway e a sessão administrativa ativos, como o item de status do macOS. O
menu `Sair do POM` é a saída terminal e encerra o processo inteiro. O runtime
WebView2 é uma dependência instalada no Windows, não um executável auxiliar
distribuído pelo POM.

O endereço é `http://127.0.0.1:<gateway_port>/admin-ui/chat`, com a porta do
`pom.json`, o `--gateway-port` como fallback e 8080 como default - o painel
nunca crava 8080. Os dois WebViews usam
`WKWebsiteDataStore.defaultDataStore`, então partilham cookies com o próprio
painel e entre si; a rota fica **depois** do portão de acesso em `App.tsx` e
usa a mesma sessão com cookie `HttpOnly` do navegador, sem credencial paralela.
Se o gateway não responde, o delegate nativo mostra uma página localizada de
indisponibilidade com botão de repetir, em vez de deixar o painel vazio. Os
textos dessa página ficam em
`assets/brand/macos/{en,pt-BR}.lproj/Localizable.strings`.

No Windows, o serviço Tokio envia um sinal depois de o listener do gateway
aceitar a porta; `windows_status.rs` só então constrói o WebView2. Isso evita
que um arranque lento, especialmente com `RUST_LOG=debug`, abra a página antes
de o gateway estar escutando e deixe `ERR_CONNECTION_REFUSED` preso na janela.

O ícone do item é uma **template image**: o AppKit descarta a cor e pinta o
canal alfa com a cor da barra, o que resolve tema claro e escuro de uma só vez.
Por isso ele não pode ser a marca como ela é - `pom-icon-src.png` é um quadrado
azul opaco, e alfa 255 em toda parte viraria um retângulo preto sólido na
barra. `scripts/derive-macos-status-icon.py` extrai o traço branco como alfa,
descarta o wordmark (ilegível a 18 pontos) e grava
`assets/brand/macos/pom-status-template{,@2x,@3x}.png`. As três entram como
representações da mesma `NSImage` de 18 points.

### Temperatura da GPU

Os dois cards mostram **uma linha por GPU**: o do Fluxo mostrava só a maior
leitura da máquina e o medidor de VRAM da primeira placa, então numa caixa com
duas placas metade do hardware sumia da tela - e a segunda podia estar
fervendo sem ninguém ver. A VRAM do card em miniatura é a **soma** das placas
que a reportam (`lib/gpu.ts`), e placas de nome igual ganham `#índice`. As faixas normal, atenção e
crítica usam os tons existentes `aqua`, `amber` e `rose`; máquinas sem GPU
omitem a seção de temperatura, enquanto uma GPU cujo sensor não responde mostra
"temperatura desconhecida" e "sensor indisponível". A API
`GET /api/ui/node` leva a política térmica junto com a máquina, e
`GET/POST /api/ui/thermal` lê ou grava seus três valores.

O painel de proteção térmica fica em Configurações > Preferências. O backend valida a
política antes de persistir, exigindo corte mínimo de 70 °C e cooldown máximo de
60 segundos, e o runtime do próprio nó aplica a histerese e o
cooldown. Quando o modelo é pausado por calor, o estado do servidor aparece
como "aguardando resfriar", distinto de peer indisponível ou falha.

### Roteamento no axum (`webapp.rs`)

O bundle é montado por último no roteador sob `/admin-ui/`; `/` redireciona
para esse mount e nenhum caminho antigo precisa expor o shell. Um arquivo real
ganha (com `mime_guess`), um miss com extensão é 404 (`asset não encontrado`) e
uma rota SPA recebe `index.html`, para recarregar o navegador em
`/admin-ui/config/keys/api` funcionar. `resolve()` rejeita `..`, caminho
absoluto e `Prefix` (há teste). Cache: `index.html` com `no-cache` (é o único
arquivo que muda sob nome fixo), assets com hash de conteúdo `immutable` por 1
ano. Em modo localhost, a fronteira do gateway recusa raiz, mount e assets
vindos de fora com uma página HTML 403 autocontida; `/v1/**`, `/api/generate`,
`/health`, `/peers` e demais rotas da pipeline continuam fora da fronteira.

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
`src/App.tsx`. `Implantações` usa o foguete (`Rocket`) e `Histórico` o
relógio (`History`): as duas seções usavam `Boxes`, o mesmo ícone de
`Modelos`, e duas seções com o mesmo desenho não se distinguem de relance. A
folha `/implantar` mantém `Rocket` sem colidir com a seção, porque a Sidebar
não desenha ícone de folha. Dentro de `Modelos`, a ordem é `Encontrar Modelos HF`, `Modelos
inteiros`, `Fatiar modelos`, `Fatias`. O menu lateral pode ser minimizado: nesse estado mostra
somente ícones, com cabeçalho da marca de 64px para alinhar à toolbar. Seções
com folhas exibem submenu ao passar o mouse, receber foco ou clicar; selecionar
uma folha navega para sua rota. No estado minimizado, a navegação usa
`overflow-visible` sem `overflow-y-auto`, pois a combinação faz o navegador
calcular overflow horizontal como auto e recorta os submenus; `z-index` não
ultrapassa esse recorte. A escolha minimizada fica persistida no navegador. O
`App` decide setup vs painel com `useIsSetupMode()` (probe de
`/api/ui/node` com `auth: false`; 404 = só existe o router de setup). Rotas:

- `/` -> Dashboard; `/chat` -> Chat (fora do `<Routes>`/transição porque é
  dono de um stream vivo e do scroll; desmontar para animar derrubaria tokens);
  `/network-chat` é o Chat da rede em página própria, com o mesmo padrão visual
  do Chat de IA e segue o fim do histórico quando a pessoa está no rodapé;
  `/admin-ui/chat` é o alias compacto usado pelo app de barra do macOS e
  renderiza o mesmo Chat sem o chrome administrativo. As duas superfícies
  exibem `ChatSurfaceTabs`, que permite alternar entre `/chat` e
  `/network-chat` nos dois sentidos.
  A entrada `Chat da rede` mostra o número de mensagens recebidas desde a
  última leitura e entrar na tela zera o badge. Em viewport mobile, os usuários
  online começam ocultos e aparecem pelo botão `Ver online`. As abas IA/REDE
  são a primeira faixa do conteúdo das duas telas de chat, acima do cabeçalho e
  da conversa.
- `/modelos` -> WholeModels; `/modelos/remotos` -> RemoteModels; e
  `/modelos/fatiar` -> SliceModels, com o painel de jobs e progresso de
  fatiamento embutido
- `/fatias` -> Shards;
  `/fatias/implantar` -> redireciona para `/implantar` (link antigo)
- Essas quatro rotas, mais `/transferencias` e `/transferencias/enviar`, são
  tela cheia (`isFullBleed` em `App.tsx`), no mesmo contrato de
  `/implantacoes`/`/implantacoes/logs`: cabeçalho fixo
  (`FullBleedHeader`/`FullBleedToolbar`, `components/ui.tsx`) e uma tabela ou
  o conteúdo próprio rolando por dentro, em vez da coluna central com
  `max-w-[1400px]`. `WholeModels`, `Shards` e a busca de `RemoteModels` viraram
  tabelas de largura total; `RemoteModelCard`, o cartão que a busca usava
  antes, foi removido. `WholeModels` e `Shards` mostram uma coluna/nota de
  visão com o nome do arquivo `mmproj` (`DiscoveredModel.mmproj_path`/
  `ShardSet.mmproj_path`, resolvidos pelo catálogo com identidade GGUF e
  dimensões compatíveis) ou "sem projetor" com o motivo da recusa. A tela nunca
  inventa o nome.
- `/implantar` -> DeployModel (entrada de primeiro nível para pôr fatias em
  serviço)
- `/implantacoes` -> Deployments (registro das implantações, ativação da padrão,
  versões por `deploy_id`, reimplantação por versão ou por parâmetros novos, e
  desimplantação). Tela cheia, como a de Logs, e clicar na linha de uma versão
  abre à direita o **mesmo** inspetor da tela de implantar, em somente
  leitura, com os parâmetros com que aquela versão subiu
- `/implantacoes/logs` -> DeploymentLogs, consola terminal do log de processo de
  todo o cluster, em ordem de chegada ao principal e com filtro por `deploy_id`;
  não confundir com a auditoria de requisições em `/logs`. O histórico é
  limitado no principal aos últimos sete dias ou 500.000 registros, o que
  ocorrer primeiro; `cluster_logs_min_level` em `pom.json` controla o nível
  mínimo capturado e assume `info`.
- `/transferencias` -> Transfers; `/transferencias/enviar` -> SendFile
- `/logs` e `/estatisticas` -> `EnterpriseFeatureShowcase` quando nenhum plugin
  ativo declara o destino. Uma declaração única do manifesto substitui o
  fallback nessa URL, inclusive para Enterprise. As demais telas de plugins
  vêm de `pom-plugin-ui/v1` e ficam sob `/<plugin_code>/<path do manifesto>`;
  por exemplo, `plugin_code: "base"` e `/example` abrem em
  `/admin-ui/base/example`. Menu e matcher usam a mesma tabela normalizada;
  menus novos aparecem na ordem do manifesto, antes de Documentação. Se dois
  plugins reivindicarem `/logs` ou `/estatisticas`, ambos usam seus namespaces
  e o fallback nativo permanece. `/logs/estatisticas` continua como alias
  nativo de `/estatisticas`. `menu[].roles` filtra o menu e o acesso direto à
  rota correspondente; sem `roles` nem o campo legado `profiles`, os três
  perfis são aceitos, sempre respeitando os perfis da rota.
- `/plugins` -> Plugins, vitrine dos itens publicáveis retornados por
  `https://license.pieceofmind.cloud/v1/plugins`, cruzados pelo `code` com o
  estado local de licença e instalação. O browser lê o catálogo público sem
  credenciais do nó, e o cliente só aceita plugins com releases válidas. A tela
  mostra ícone, até quatro screenshots, descrição, preço e release quando o
  servidor fornece esses dados; campos e imagens ausentes têm fallback visual.
  A descrição usa a versão pt-BR se existir, senão a descrição inglesa. Até
  quatro screenshots podem ser ampliados em um diálogo de tela cheia, com
  Escape e foco devolvido ao controle que abriu. Expiração e uso de ativações
  por plugin vêm do estado autenticado publicado pelo nó, nunca do catálogo
  público.
  Plugins ativos no nó continuam listados mesmo quando faltam no catálogo
  público, com as ações locais preservadas. Quando a licença exigida não está
  ativa, Comprar redireciona para a loja pública com o `plugin_code` codificado
  por `lib/pluginPurchase.ts`. Plugins ativos mostram expiração da
  licença, última instalação/aplicação e última checagem de update como datas
  distintas; a tela lê esses horários do estado persistido pelo nó. A chave de
  ativação aparece quando qualquer entrada pública que exige licença não está
  ativa no estado local, mesmo se outro plugin já estiver ativo ou houver
  contexto de licença; ausência de status local, expiração e revogação mantêm o
  formulário visível. Ele some quando todos os plugins licenciáveis conhecidos
  estão ativos. Desativação pede confirmação. A rota usa `FullBleedHeader` e
  rolagem interna como `/modelos`; o painel de Plugins usa
  `overscroll-y-contain` para não propagar a rolagem ao limite para a página.
  O documento permanece sem rolagem; as áreas internas definem seus próprios
  limites.
  Carregamento, erro e catálogo vazio têm estados próprios.
- `/documentacao` -> Documentation (tela cheia; ver a seção própria abaixo)
- `/redes-publicas` -> PublicNetworks, catálogo full-bleed de redes públicas
  descobertas pela camada pkarr. A tabela agrupa por país/cidade e ordena pela
  distância aproximada autorizada no navegador; mostra as métricas calculadas
  quando o cluster local corresponde à rede e marca como indisponível o que o
  catálogo público não fornece. O botão de entrada reaproveita a tela de
  confiança sem segredo de rede pública.
- `/setup` -> `Setup`, o wizard real do primeiro acesso, **em tela inteira**:
  sem Sidebar e sem Topbar, como o bootstrap. Ele mostra hardware antes das
  decisões de capacidade e modelo e conduz `admin`, `node` ou `infer` até o
  save. No admin, a exposição da interface na rede é a única opção que pede
  uma senha administrativa; a criptografia TLS entre peers é opcional e começa
  desligada. A tela de convite baixa o arquivo da raiz com o nome retornado por
  `admin_trust_filename` e mostra a chave de conexão apenas em redes local ou
  privadas. Durante o setup, uma etapa de descoberta local deixa o admin
  esperando e faz node/infer procurarem um admin; a chave só chega depois do
  handshake explícito. `SetupWizard.tsx` continua sendo apenas o mock de
  referência visual.
- `/config/rede` -> NetworkSettings; `/config/rede/participantes` -> a lista de
  participantes para `admin` e `node`; `/config/preferencias` -> Preferences;
  `/config/preferencias/admin` -> preferências administrativas;
  `/config/sobre` -> About, último item de Configurações, com links para o
  `LICENSE` AGPL-3.0 do repositório e a página GNU oficial, aviso de
  privacidade/segurança LGPD e links do site, formulário de contato
  (`https://pieceofmind.cloud/contact.html`) e repositórios oficiais; também
  mostra o estado do plugin associado à atualização do `pom_core`
  e a verificação sob demanda. Quando há atualização disponível, a própria
  tela oferece aplicar e reiniciar; também mostra o catálogo de dependências
  diretas agrupado por licença;
  `/config/confianca` reúne esta máquina, compartilhamento e chaves de conexão;
  `/config/keys/api` -> TokenKeys; `/config/armazenamento` -> ModelPaths
  (`/config/pastas` continua como alias). O wizard de configuração não aparece
  no menu, embora sua rota de teste continue disponível.

Em uma rede pública, `NetworkSettings` local é uma réplica read-only para
`node` e `infer`. O diretório de entrada carrega apenas os dados necessários
para descobrir a rede; depois que o `Announce` autenticado do principal chega,
o backend busca o documento completo, incluindo nome, organização, descrição,
regras, canais e visibilidade. A tela não deve tratar capacidades anunciadas
durante o boot como pipeline pronta: o catálogo só marca cobertura completa
quando cada peer confirmou `/health` ou está `idle`.
- `*` -> NotFound

No perfil `infer`, a Sidebar deixa de ser uma lista de telas: `ChatSessionList`
ocupa o menu principal com `Conversa nova` e as conversas salvas. O botão novo
fica fora da área rolável, a lista usa `min-h-0` + `overflow-y-auto` e a sessão
ativa recebe destaque e `aria-pressed`; assim muitas conversas não escondem a
rede nem fazem o menu crescer além da janela. A rede é o único destino principal
adicional. Documentação e Configurações (a chave API permitida ao infer) são
projeções de rodapé junto das bandeiras. `ChatSessionProvider` mantém a seleção
da Sidebar e o Chat na mesma sessão. Administrador e node continuam recebendo a
projeção normal de `NAV`.

### Wizard de primeiro acesso

`routes/Setup.tsx` é o wizard real, servido pelo router loopback
`/api/setup/*`. Ele detecta hardware antes das decisões, pede localização
país/cidade para todos os perfis, persiste a localização aproximada autorizada
e usa etapas diferentes para `admin`, `node` e `infer`: admin cria a raiz,
configura a rede e convida; node escolhe RAM, CPU, disco de fatias e proteção
térmica; infer só confirma a confiança e entra. País e cidade são obrigatórios
para rede pública e opcionais, mas persistidos, para local/private. O nome
público também é obrigatório para rede pública em todos os perfis e aparece no
passo de confiança de node/infer antes do salvamento. A cidade manual é válida
quando a referência SQLite não a contém.

O wizard não exige ingresso HTTP(S) do diretório federado para uma rede pública.
Quando configurados por `POM_PUBLIC_DIRECTORY` ou posteriormente na
administração, eles são persistidos no documento de confiança e usados para
publicar ou consultar o catálogo. Um nó privado pode conservar esses ingressos
para listar o catálogo por saída.

O passo de modelos combina o catálogo local com referências do Hugging Face,
sem iniciar download, e usa `~/.pom/models` e `~/.pom/models_sliced` como
padrões. O convite do admin mostra e baixa a raiz `.pom.trust.json`, a
fingerprint calculada e a chave de conexão apenas quando a rede é
local/private. A chave pública não é exibida separadamente, pois já está na
raiz. A verificação final cobre GPU, backend disponível, configuração,
descoberta P2P e IPv6 público; a inferência do modelo fica para depois.

`routes/SetupWizard.tsx` e `components/setup-wizard/*` continuam sendo apenas o
mock de referência visual. Não são usados pelo primeiro acesso real.

### Modelos remotos

`RemoteModels` carrega ao abrir os modelos GGUF em alta pela API pública do
Hugging Face (`/api/models?sort=trendingScore&limit=20&filter=gguf`), sem enviar
credenciais. A busca do provider continua usando o nó autenticado; o status de
login fica compacto dentro da barra, junto ao botão Buscar. Depois da busca, o
detalhe resolve a licença pelo
provider e exibe o marcador de modelo gated antes da confirmação de download;
assim a tela não presume uma licença nem inicia uma operação protegida sem
explicar a autenticação exigida.

O título da tela é "Encontrar modelos no Hugging Face". O detalhe consulta
`GET /api/ui/model-provider/storage` e mostra as pastas de modelos resolvidas da
configuração antes de iniciar. O download escolhe uma dessas pastas; o job
persistido em `GET /api/ui/jobs` registra o caminho final e o
`ModelDownloadsPanel` mostra progresso, cancelamento e retry. Esse painel
também aparece em `/transferencias`, junto das transferências de arquivos.

Em `/config/preferencias`, o `HuggingFaceSettingsPanel` oferece o token opcional para
modelos privados e gated. O campo é `password`, começa vazio e mostra apenas a
máscara devolvida pelo backend. Salvar ou remover chama
`/api/ui/model-provider/config`, invalida o status e faz o provider reler a
credencial sem reiniciar o processo; a precedência segue a documentada em
`api-http.md`.

### Perfis: o que cada um enxerga

O nó anuncia um perfil persistido em `pom.json`: `admin`, `node` ou `infer`.
O administrador opera o cluster; o nó cuida da própria máquina; o `infer` tem
uma vista reduzida para conversar com os modelos, ler a documentação, cuidar
da própria API key e pedir entrada no processamento da rede.
`GET /api/ui/node` é a fonte usada pelo browser e a toolbar mostra o rótulo
pela i18n.
O mesmo retrato traz `pom_desenv`, verdadeiro somente quando o processo foi
iniciado com `POM_DESENV=1` exatamente. `DeployModel`, Histórico e a caixa de
reimplantar escondem por completo `Configurações POM (DESENV)` quando a flag é
falsa. Na tela `/implantar`, Tensor, as estratégias Simples/Completa e o botão
"Voltar ao plano do nó" também ficam ocultos quando `pom_desenv` é falso. Isso é
somente visibilidade local: a UI e o backend preservam os valores
da implantação recebidos pela rede, mesmo quando o nó não pode editá-los.

No Dashboard, a leitura é separada por responsabilidade. O `admin` vê os
agregados do cluster, como cobertura, máquinas, modelo ativo e carga em voo.
Os perfis `node` e `infer` veem somente os fatos da máquina local: modelo
servido, faixa de camadas e pedidos em voo. Para o `infer`, a cobertura global
não é um indicador operacional da sua tela, então não é renderizada como um
alarme de pipeline. Se `/api/ui/node` não estiver disponível, esses campos
mostram indisponível; ausência de dados nunca vira modelo vazio, pipeline
incompleto ou zero.

"Pedidos neste nó" mede o número de requisições de inferência atualmente em
voo no `LoadTracker` daquela máquina. Zero é uma medição válida somente quando
a própria máquina respondeu zero; falha de leitura permanece desconhecida.
Da mesma forma, a carga de um peer no mapa de implantação vem do `/api/ui/node`
do próprio peer, e não do tracker do coordenador.

`src/lib/access.ts` combina as rotas nativas com os perfis do registro de
plugins (`lib/pluginUi.ts`); rota nativa ausente da tabela é livre para `admin`
e `node`. Hoje `/implantar` e `/implantacoes` são só do administrador. As
vitrines nativas `/logs` e `/estatisticas` ficam acessíveis para descoberta; as
telas do plugin seguem os perfis declarados no manifesto. Para `infer`, a lista
de rotas nativas permitidas e negadas vive em `INFER_ROUTES` e
`INFER_DENIED_ROUTES`. No Chat, a telemetria da execução e seus resumos também
ficam ocultos. A chave casa **segmento inteiro**, então `/logs` não cobre
`/logs-antigos`.

No Chat, o `infer` recebe um recorte próprio por
`GET /api/ui/infer/heads?model=...`: a caixa **Entrada da inferência** mostra a
cabeça selecionada e as outras faixas que começam em L0, com RTT, idade e carga
em voo. Se a leitura viva da candidata não chega, a carga aparece como
indisponível, não zero. A política fica escrita na tela: cobertura primeiro, score ponderado de
RTT fresco e carga, e empate por id. RTT sem medição fresca aparece como fallback, e um modelo sem cabeça
ou sem caminho completo produz um aviso legível em vez de uma falha crua. Esse
recorte não abre a lista geral de peers nem oferece autoridade de controle.
Ao trocar o combo, o Chat envia o `node_id` escolhido como `inference_head` na
próxima requisição; a pipeline e a telemetria usam essa cabeça, em vez de
reaplicar o score de RTT/carga.

`Logs` consulta `GET /api/ui/requests` com a chave de conexão do operador. A
barra compacta aplica filtros por identidade não secreta da API key, texto do
prompt, nó e período, lista primeiro as entradas mais recentes em páginas e
carrega a próxima quando o sentinela entra na viewport. A tela de estatísticas
consulta `GET /api/ui/requests/stats`, que aceita os mesmos filtros, incluindo o
rótulo de usuário e o modelo, e agrega no servidor por token, nó, dia e peer,
além de tokens de entrada/saída, inferência, TTFT, cache de prefixo e estados
terminais. Clicar em qualquer área não interativa da linha
  abre o inspetor, preserva a lupa e destaca a linha selecionada com cor de
  acento distinta do hover. O cabeçalho do inspetor repete os detalhes da linha.
  ENVIADO mostra a última mensagem de usuário como prompt novo e o restante do
  histórico como contexto, inclusive respostas de assistente/ferramenta que
  aparecem depois dela, em duas superfícies Markdown renderizadas de altura
  dividida; Resposta usa o mesmo visualizador somente para
  leitura, sem barra de edição, HTML, imagens ou links com protocolo inseguro.
  O conteúdo exibido é limitado a 64 KiB para preservar a responsividade. A
  preferência global entre Markdown renderizado e JSON cru fica em `localStorage`
  e é restaurada em visitas futuras. A tela de Estatísticas usa a mesma fonte
  agregada, mas não é uma aba dentro de Logs.
  Cada área Markdown também tem um botão icon-only de expansão no canto
  superior direito; a leitura abre em uma janela grande, acompanha o toggle
  global e mantém a mesma sanitização e censura de credenciais.
  REQUEST e TELEMETRIA mantêm o JSON completo, com TELEMETRIA separando o caminho
  da requisição do cartão visual `ExecutionTelemetry`, compartilhado com o Chat.
  O indicador de carregamento ocupa um slot à esquerda do badge de erros,
  mantendo o badge fixo à direita. A cópia redige credenciais, inclusive `key` de
registros em `api_keys` quando o valor tem formato de segredo conhecido, sem
ocultar campos `key` inocentes.
`in_progress`, `incomplete`, `partial`, erro e abandono recebem estados visuais
distintos. A identidade exibida é o id e o rótulo configurados para a chave;
nenhum segredo é pedido ao endpoint ou renderizado no navegador.

**Uma tabela só, lida em dois lugares.** O `App.tsx` a consulta para barrar a rota
(`Forbidden` no lugar da tela) e o `navForProfile` (`components/nav.ts`) para podar
o menu. Duas listas separadas dariam os dois defeitos clássicos: menu que oferece o
que a rota recusa, ou rota aberta a um endereço digitado que o menu já escondia.
Uma seção com folhas some quando **nenhuma** folha sobra - "Logs" desaparece
inteira para o perfil `node`, "Configurações" continuaria de pé se só uma folha
fechasse.

**Isto não é a única autorização.** O perfil chega do nó em `pom.json` e o
browser o usa para apresentação e navegação; a credencial continua sendo a
chave de conexão, verificada no gateway (ver *Autenticação*). O backend também
impõe a vista local do `infer`: `/api/ui/cluster` e `/api/ui/cluster/models`
não fan-outam para peers, `/admin/nodes` responde 403 e detalhes de outra
máquina respondem 403. Não existe troca de perfil na interface.

Na tela `/config/rede`, um `infer` vê os dados da própria máquina e envia
`POST /api/ui/network/request-node`. O pedido aparece no sino de notificações
dos admins, e qualquer admin principal ou secundário pode aprová-lo ou recusá-lo.
O nó só muda de perfil depois da aprovação do servidor. O administrador
principal pode promover um nó com `POST /admin/nodes/{id}/promote-admin`; o
gateway exige autoridade efetiva de principal, e o alvo valida a concessão
Ed25519 na cadeia de confiança. A `connection_key` continua sendo apenas a
credencial de transporte entre nós.

Na mesma tela, um `node` pode pedir a própria descida com
`POST /api/ui/network/relinquish-node`. A confirmação avisa que novas
inferências são desviadas primeiro, que as inferências em curso drenam sem corte
e que só então o perfil muda e a fatia é liberada. Se não houver substituto
capaz da mesma faixa, a UI avisa que a faixa ficará descoberta e as gerações
que precisarem dela falharão claramente. O admin recebe no sino apenas um aviso
sem botões.

`/config/administracao` mostra o principal, os admins secundários, a cadeia de
promoções e a elegibilidade do sucessor. Só o principal pode promover ou
despromover. A resignação mostra o sucessor determinado pelo último promovido e
recusa explicitamente os casos sem secundários ou sem secundário ativo. A
oferta de transferência do papel de principal é respondida pelo sucessor no
sino; até a aceitação, nada muda e o principal continua principal. A página e
o sino consomem contratos de apresentação, sem carregar decisões de chave,
assinatura ou armazenamento de concessões.

`/config/confianca` é a **raiz de confiança** da máquina, e é o trabalho que
vem antes de tudo o que está acima. Ela mostra o estado - **Ancorado** ou **Por
ancorar** - num relance, e, quando há raiz, a impressão digital SHA-256 em
grupos de oito, com botão de cópia, ao lado do cluster e da máquina da raiz.
Essa é a informação que o operador leva consigo para conferir noutro nó; é
pública e não concede nada.

Num nó ainda sem raiz, a mesma tela mostra o outro lado: um campo para colar o
conteúdo do arquivo de confiança vindo do admin e outro para **digitar** a
impressão digital que o operador leu lá. O segundo campo nunca é preenchido a
partir do primeiro - derivá-lo anularia a conferência, que é a única coisa que
faz o arquivo colado valer alguma coisa. Fora do loopback o formulário dá lugar
à explicação de por que instalar é local, mas a decisão nunca é da tela: o
servidor recusa loopback ausente, nó já ancorado e impressão digital que não
corresponde, e devolve um código estável que a tela traduz (ver
`api-http.md`, "Ancorar a raiz pela interface").

**Ela é aberta aos três perfis de propósito.** Antes de ancorar, um nó com
`profile: admin` não tem capacidade efetiva de admin e não abre a superfície
administrativa - pôr esta tela atrás do perfil de administrador a deixaria
inalcançável justamente no estado em que ela existe para servir. E quem recebe
a âncora costuma ser um `node` ou um `infer`. Por isso `/config/confianca` não
está em `RESTRICTED` (`lib/access.ts`), aparece no menu de Configurações e é
promovida ao menu principal do `infer`.

O cabeçalho da `Topbar` mantém identificação do nó, perfil, notificações e o
atalho de janela nativa; o botão `Este nó` não é renderizado. O indicador TLS
da Sidebar (`components/TlsIndicator.tsx`) também está oculto para todos os
perfis. A política continua disponível em Preferências administrativas; nenhuma
configuração ou comportamento TLS foi alterado.

Na tela `TokenKeys`, node e infer veem apenas as chaves do próprio nó, não
escolhem outro nó e mantêm a criação vinculada a essa identidade. O admin pode
escolher o nó ao criar, vê seus tokens completos e recebe os tokens de outros
nós mascarados. Os tokens do admin ficam primeiro na tabela. No infer, gerar
uma nova chave usa o endpoint atual e, em seguida, revoga a anterior.

Sidebar e Topbar vivem em `ErrorBoundary` próprio, então uma tela quebrada não
derruba o menu. O **cabeçalho** da Sidebar traz só o lockup da marca, numa faixa de borda a
borda (`LogoFull variant="band"`) fechada por uma linha divisória. O **rodapé**
traz o idioma em **bandeiras** (`brand.flags`, do `brand.json`) em vez de
"EN"/"PT-BR" - com o nome do idioma no `aria-label`/`title`, porque bandeira não
é idioma para leitor de tela -, o `build_commit` do nó (só o hash, em mono, com
a explicação no tooltip) e o tema claro/escuro. O PipelineRail ainda é renderizado oculto
no Dashboard (`hidden`, para uma superfície futura).

No `infer`, o rodapé coloca Documentação e Configurações ao lado das bandeiras;
o item principal de rede continua na coluna do histórico para preservar o acesso
ao pedido de entrada no processamento. Essas posições são metadados de `NAV`, não uma segunda lista de
rotas.

## Arquitetura do frontend

`src/` tem quatro subpastas, cada uma com seu `AGENTS.md` (mapa autoritativo):

- **`src/lib/`** - cliente HTTP, hooks react-query, persistência em
  `localStorage`, stream/estado do chat, formatação, toasts e todos os tipos.
  `api.ts` é o único caminho de fetch: caminho **relativo** (o nó serve API e
  UI na mesma porta, o navegador nunca aprende endereço de peer), assinado com
  `Authorization: Bearer <connectionKey>`, e também o leitor de SSE
  (`streamSse`). `api.getConditional()` conserva os snapshots de
  `/api/ui/cluster`, `/api/ui/jobs` e `/api/transfer/offers` por `ETag`,
  aceitando 304 sem reparsear JSON. `queries.ts` concentra as leituras com
  polling: `useNode` 4s (guarda a `connection_key`), `useCluster` 5s,
  catálogo local 30s, catálogo do cluster 60s, `useJobs` 2s enquanto há job
  rodando e 10s quando ocioso, `useTransfers` 2s, `useOffers` 2s enquanto
  há oferta e 10s quando vazio, admin nodes 8s, log de requisições 3s. O
  catálogo usa polling de 2s enquanto `metadata_complete` é falso, depois
  volta ao intervalo normal.
  `optional()` engole só 404 (nó antigo sem rota não é erro para mostrar).
  `types.ts` é a fonte única das formas que o nó serve (comentários apontam
  para o Rust).
- **`src/components/`** - chrome (`Sidebar`, `Topbar`, `Notifications`, `nav`)
  e widgets compartilhados. `ui.tsx` é o design system num arquivo de propósito
  (painéis, botões, inputs, badges, meters, estados) em classes por token CSS,
  sem estilo em runtime. Visualizações do cluster: `ModelNetworkCanvas` (grafo
  SVG pan/zoom com RAM/VRAM/carga e arestas principal/paralela/inativa,
  posições persistidas por fingerprint de implantação; o card também resume
  quantas camadas foram parar na GPU e quantas sobraram na CPU) e `PipelineRail` (o
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

Antes de chegar aos handlers, o gateway aplica a catraca administrativa a
`/api/ui/**` e `/admin/**`. Em modo local ela confere o endereço real da
conexão e aceita somente loopback; em modo de rede exige
`X-POM-Access-Token`. O token de pessoa é diferente da `connection_key`: o
primeiro abre a interface administrativa, enquanto a segunda autentica nós e
clientes da pipeline. Chamadas de controle entre nós usam a segunda com um
marcador interno próprio, que o navegador nunca envia.

`GET /api/ui/node` devolve a `connection_key` somente para redes privadas, depois
da catraca. A inferência usa exclusivamente a API key dedicada `pom-[node_id]`,
obtida em `POST /api/ui/chat-key` e enviada como `X-API-Key` em cada chamada
`/api/generate` ou `/v1`. Se ela for revogada, a próxima tentativa cria uma
substituta e repete a mensagem uma única vez. O primeiro acesso usa o router
de setup (sem `pom.json`), que serve hardware, modelos, prévia da raiz
administrativa, senha administrativa, instalação da raiz, verificações e `POST /api/setup/save`. O save
instala a raiz de confiança separada e grava `pom.json` antes do respawn. O
respawn cria uma sessão independente em Unix, herda os argumentos originais e
redireciona stdout/stderr para `<project_root>/pom.log`, para que uma falha de
arranque após o save fique diagnosticável. No Windows o binário não cria
console, então o tracing também abre `<project_root>/pom.log` diretamente; a
redireção PowerShell `*> arquivo` não é a fonte confiável nesse modo. Para a
reprodução nativa, use `RUST_LOG=debug` e recolha esse `pom.log` junto com
`infer_server_<porta>.log`. A sugestão de `advertise_addr` usa
a enumeração real das interfaces: IPv6 global não temporário primeiro, depois
IPv4; ela é apenas uma sugestão editável e o anúncio continua redetectando os
endereços em cada ciclo. A resposta de nó configurado publica
`addr_candidates`, `libp2p_peer_id` e as
capabilities do túnel, usados pelo deploy TP para transportar o endereço correto
até cada rank.

`GET /api/public/networks` combina cache e ingressos do diretório federado com
a descoberta pkarr quando a rede ativa é pública. O label geo canônico (`com.<país>.<cidade>.<node>`)
é campo do registro, nunca a chave pkarr. O índice local persiste labels já
observados e aceita `geo_prefix`/`geo_suffix`, permitindo filtrar candidatos
antes das sondas HTTP. A página `/redes-publicas` é full-bleed, como Logs, e
mostra uma tabela com nome, organização, país/cidade, label, nós, latência,
VRAM, modelos e endereço. As métricas do cluster local são calculadas de
`ClusterView` quando a rede corresponde; os demais campos aparecem como
indisponíveis porque o catálogo público atual não os fornece. O documento de
confiança viaja dentro do registro federado assinado para o join sem segredo.
Um nó privado consulta o diretório por saída HTTP(S), mas não usa o swarm
privado para buscar candidatos. O endereço listado ainda pode ser inalcançável,
por exemplo quando é IPv6 anunciado sem rota externa. Entrar numa rede pública
encerra a rede privada atual; duas redes P2P simultâneas não são suportadas no
v1.

O **perfil** da interface (*Menu e rotas*) não entra nesta conta: ele é escolha
local do navegador e não vale como credencial. As rotas administrativas usam a
chave de conexão; `/api/generate` e `/v1` usam exclusivamente a API key
dedicada de inferência, seja qual for o perfil na tela.

O Chat da rede gera ids de mensagem e digitação com `src/lib/id.ts`. Ele usa
`crypto.getRandomValues` quando `crypto.randomUUID` não existe, portanto o
composer continua funcionando na implantação HTTP da LAN, sem depender de
secure context. A janela de mensagens segue novas entradas quando está próxima
do fim; ao rolar para cima, preserva a posição até a pessoa retornar ao rodapé.

O Dashboard mantém os indicadores e ações na barra superior compacta. Quando há
uma implantação ativa, o botão de propriedades é apenas um ícone dentro do
mapa, no canto superior direito, alinhado aos controles de zoom e Fit.

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

Esse card é **do administrador**, e `canSeePeerDetails` (`src/lib/access.ts`)
diz quem o vê. Ele traz o estado interno de cada máquina e as ações sobre ela -
liberar fatia, remover do pipeline, tirar da rede -, que é operação do cluster e
não do próprio nó. Fora do `admin` resta o Fluxo, e por isso **o seletor de
vista também não aparece**: onde há uma vista só não há o que escolher. As duas
portas fecham juntas - o `ViewSwitch` some do Painel e o duplo clique no card do
canvas deixa de abrir a `PeerDetailDialog` -, porque esconder o seletor e
continuar abrindo o card pelo desenho seria a mesma tela em dois estados. A
regra fica ao lado da tabela de rotas por ser a mesma pergunta: o que este
perfil enxerga. Para o admin, sem `ClusterView.active_model` a vista inicial é
Detalhes e o botão Adicionar não aparece; com modelo ativo, a vista inicial
continua sendo Fluxo. A escolha manual da vista prevalece sobre esse padrão.

Detalhes lista a **rede inteira**, não a pipeline: `networkPeers`
(`src/lib/networkPeers.ts`) separa quem serve o modelo ativo na ordem das
camadas (`pipelinePeers`) de quem está conectado sem fatia (`others`, por
nome). Máquina sem fatia é `model` vazio com `total_layers` zerado
(`hasSlice`), **não** uma faixa "0-0", e o card não escreve faixa nenhuma
nesse caso.

As ações da máquina ficam numa **barra de ferramentas no topo do card**, junto
do distintivo de MTP: liberar memória (`ReleaseSlice`, só na própria máquina),
`RemoveNode` (sai do modelo, só quando serve), `LeaveNode` (sai da rede,
sempre) e atualizar/reiniciar quando o peer anuncia `dev_update`. Estavam
espalhadas pelo pé do card, uma por seção, e reiniciar uma máquina era rolar
até o fim de tudo o que ela reporta.

O diálogo `AddNodeDialog` consulta `POST /api/ui/deploy/node/options` depois de
cada troca de máquina. Assim, o seletor mostra as fatias e o modelo inteiro
que o alvo realmente tem, além dos controles de camadas na GPU, do booleano de
KV na GPU versus RAM e dos tipos K/V daquela máquina. O POM não expõe uma
seleção de GPU exclusiva para o KV: quando ligado, ele segue o dispositivo da
camada dos pesos. Confirmar chama
`POST /api/ui/deploy/node` somente no alvo e preserva a inferência dos outros
peers. Com um `deploy_id` ativo, a alteração cria uma nova versão; selecionar
um GGUF inteiro ao lado de fatias produz o rótulo `mixed` no Histórico e duas
cabeças de entrada possíveis.

`RemoveNode` também recebe o `deploy_id`. A remoção parcial cria uma versão com
as máquinas restantes e mantém a implantação ativa. A última remoção cria uma
versão vazia, que permanece no Histórico para auditoria, mas deixa de aparecer
como implantação servindo.

O card é denso de propósito, para caber numa tela: o que é útil sem precisar
estar sempre visível mora num `[i]` (a razão de "sem fatia", o aviso do
`gpu_note`, a nota do fit, o caminho do modelo, pid e reinícios), e
"Parâmetros de carga do llama.cpp" e "Processos na GPU" são blocos fechados
que se abrem. Os discos são **um medidor por partição montada** (`disks` do
`/api/ui/cluster`), porque uma máquina com NVMe e HDD, ou com `/` e `/dados`
separados, aparecia como um volume só - e quem decide onde cabe um GGUF de
40 GB não via o disco onde ele caberia. Um nó que ainda não publique a lista
recai no volume do projeto. Ao lado de
"Adicionar nó" há `UndeployPipeline` quando há modelo no ar, mas os dois
controles só são montados quando `/api/ui/node` informa o perfil `admin`. O
servidor também recusa `POST /api/ui/deploy/node` e `POST /api/ui/undeploy` para
os perfis `node` e `infer`, com a mensagem localizada pela UI. O botão de
remover uma única máquina no card continua independente dessa regra. `PipelineRail`
ainda exporta `pipelinePeers`/`trackOf` usados pelo Dashboard e DeployModel.

Numa máquina servindo, o card traz também **"Como o modelo foi carregado"**
(`ModelPlacement.tsx`): uma tira com uma célula por camada - verde inteira na
GPU, âmbar dividida, cinza na CPU (as cores saem dos tokens do produto; a tira
saiu invisível enquanto pedia `bg-lime-400`, que não existe nesta paleta) -, os pesos de cada lado em bytes, onde o
cache KV ficou e a cabeça de saída. É a resposta medida, não a configuração:
uma camada conta pelos **bytes** dela em cada buffer, então os experts de um
MoE empurrados para a CPU aparecem como camada dividida em vez de camada
descarregada. Os dados vêm de `/api/ui/peer/:node_id/runtime`, que repassa o
`/health` daquela máquina, e a busca só acontece com o card montado
(`usePeerRuntime(nodeId, enabled)`) - o Painel recarrega em ciclo curto, e um
`/health` por peer a cada ciclo seria caro por uma seção quase sempre fechada.
Nó que não informa a colocação faz a seção dizer que não sabe. Ver
`inferencia.md`, "Onde as camadas ficaram".
O mesmo card traz "Parâmetros de carga do llama.cpp" (`LoadParamsSection`),
que mostra com que parâmetros o contexto daquela máquina subiu: Flash
Attention, `n_batch`/`n_ubatch`, `n_ctx` total e por sequência, sequências de
KV, threads, camadas na GPU e tipo do cache KV. Serve a quem quer comparar o
nó com uma linha de comando do `llama-cli` sem abrir o `/health` na mão. Os
valores vêm do `load_params` do `/health`, lido do contexto vivo do peer; um
nó cujo `/health` não traga o campo simplesmente não mostra a seção. O Flash
Attention distingue "ligado (auto)" de "ligado (pedido)", porque o padrão do
llama.cpp é sondar o backend, e mostra "não sei" quando a ponte de
diagnóstico não respondeu - nunca "desligado". Quando o fit mexeu nos valores,
uma nota diz que eles podem diferir dos pedidos. Ver `inferencia.md`,
"Flash Attention e os parâmetros de carga".

Quando a configuração da máquina pediu MTP, o mesmo card mostra se a cabeça
`nextn` ficou ativa. Se a criação do contexto MTP falhar, mostra
"MTP indisponível" e o `mtp_unavailable_reason` retornado pelo `/health`; o
operador não precisa inferir a degradação pela ausência de logs.

## Configurações / Rede e Preferências

`/config/rede` concentra as Informações da Rede: nome público, organização,
descrição, regras, canais, visibilidade e localização. Node e infer recebem
leitura somente, enquanto admin pode editar. País e cidade são selects
alimentados por `GET /api/ui/network/geo-reference`, que lê as tabelas locais
`geo_countries` e `geo_cities`; o código do país não é mostrado. A mesma tela
calcula nós, latência média, VRAM conhecida e modelos do `ClusterView`.
`/config/rede/participantes` reaproveita o card de nós conhecidos e fica
disponível para admin e node. Cada peer mostra a **rota em uso**, não o
endereço que anuncia: um selo (Direto, Direto (P2P), Direto (hole punch), Via
relé, Sem rota) e o endereço dessa rota - o do relé, quando o hole punch não
fechou. O anunciado fica no `title` da linha. Ver `rede-p2p.md`, "Diagnóstico".

`/config/confianca` reúne esta máquina, compartilhamento da rede e chaves de
conexão. `/config/preferencias` monta a exposição administrativa, o
`ThermalPanel`, o `HuggingFaceSettingsPanel` e o link para armazenamento.
`/config/preferencias/admin` mantém as preferências administrativas de TLS,
QUIC, logs de processo entre nós, chat da rede e descoberta local. A separação reorganiza as
mesmas operações e conserva a autorização existente.

A exposição é persistida no `pom.json`: localhost é o padrão instalado, e a
rede usa uma senha separada da `connection_key`; somente o hash Argon2id é
persistido, e a senha anterior é exigida para trocá-la. Trocar para localhost
limpa a senha. Um token antigo é migrado para Argon2id no primeiro carregamento,
preservando seu valor como senha. A porta é validada antes de salvar e uma colisão é informada sem
iniciar em outra porta. O `ThermalPanel` lê e grava a política em
`GET/POST /api/ui/thermal`; o painel de Hugging Face usa
`/api/ui/model-provider/config`.


## Configurações / Preferências: quando usar TLS entre nós

O painel "Criptografia entre nós" (`TlsPanel`, em `routes/Preferences.tsx`)
mostra o estado do cluster e, **só no nó principal**, o interruptor que o vira.
Ele pertence à rota de preferências administrativas; node e infer não recebem
o painel nem o ícone de TLS na Sidebar. O estado continua sendo decidido pelo
principal e vale para a rede inteira.

A autoridade que vale é a do servidor - `POST /api/ui/network/tls` responde 403
fora do principal (ver `api-http.md`). A tela usa `NodeInfo.tls_authority` só
para não mentir antes do clique.

Virar não pede reinício e vale nos dois sentidos; o toast diz que os demais nós
adotam em poucos segundos, que é o ciclo de gossip mais a busca autenticada da
catraca (ver `rede-p2p.md`, seção 5).

## Configurações / Preferências administrativas: transporte QUIC

O painel "Transporte QUIC" (`QuicPanel`) fica ao lado do TLS na rota
administrativa. O interruptor fica desabilitado fora do principal; o servidor
também responde 403 se outro nó tentar
`POST /api/ui/network/quic`. A alteração não reinicia o modelo: novos hops
preferem HTTP/3 sobre QUIC, e uma falha cai para o HTTP/HTTPS da política TLS.
O card de telemetria mostra `http`, `https` ou `quic` conforme o transporte que
realmente carregou cada hop, além de marcar o fallback.

## Logs de processo da implantação

`DeploymentLogs.tsx`, em `/implantacoes/logs`, é a consola terminal do processo
do cluster e não a auditoria de requisições de `/logs`. O fundo preto e a fonte
monoespaçada deixam cada linha com o nome da máquina, nível e horário de origem;
as linhas vêm da réplica local do admin na ordem de chegada e a tela consulta
novos lotes a cada segundo. O seletor de implantação envia `deploy_id`,
permitindo abrir os mesmos logs a partir de uma implantação no Histórico.

`Preferences.tsx` mostra o painel "Logs do cluster" junto do TLS. O interruptor
fica desabilitado fora do principal e o backend também rejeita a operação nesses
nós. A leitura dos logs fica disponível a qualquer admin efetivo porque a cópia
é local. O estado e a autoridade vêm de `GET /api/ui/node`; a alteração usa
`POST /api/ui/network/logs` e não exige reinício. O contador de entradas
descartadas pela fila limitada aparece acima do terminal, para a tela não
apresentar uma visão incompleta como se fosse o fluxo integral.

No Dashboard, o admin pode abrir `DeploymentLogDock` no rodapé. A barra
recolhida ocupa uma única linha; aberta, cresce para cima até metade do painel,
consulta `GET /api/ui/cluster-logs` em tempo real e acompanha o fim enquanto o
leitor permanece no final. A versão ativa da implantação fica disponível num
inspetor à direita, somente leitura, usando `DeploymentInspector` e os mesmos
campos da configuração original. Node também pode ler as versões por
`GET /api/ui/deployments`, mas continua sem as ações administrativas.

## Chat

`src/routes/Chat.tsx` fala com o cluster por `POST /v1/chat/completions` com
`stream: true`, igual a qualquer harness - o que aparece ali é o que um
harness receberia. O histórico é do navegador (`chatStore`, `localStorage`,
cap 50 sessões, sincronizado entre abas), não do nó. O `ChatSessionProvider`
também mantém o stream e acumula os deltas em memória, publicando e persistindo
o parcial em uma janela de 50 ms e fazendo flush imediato no fim ou na
interrupção. Assim sair da rota não interrompe a geração: ao voltar, a conversa
mostra o texto já recebido e o estado ao vivo sem forçar um re-render por token.
Só o botão de parar ou o envio de uma nova mensagem aborta o pedido anterior.
Três coisas a conversa guarda além das falas:

- O único campo de ficheiro lê `GET /api/ui/chat/capabilities`, que agrega a
  cabeça ativa e a cobertura sem reutilizar a topologia que o perfil `infer` não
  pode consultar. Ele aceita qualquer tipo de ficheiro, botão e arrastar e
  largar. Imagens exigem `vision: supported`; `unsupported` mantém o modelo sem
  visão e `unavailable` informa que a leitura foi recusada ou não pôde terminar,
  sem inventar um modelo vazio. Imagens são redimensionadas no navegador para
  caber no orçamento efetivo antes de virar base64 no pedido: a estimativa
  mantém quatro bytes por token, reserva 10% para segurança e tenta qualidade
  JPEG e resolução até 2048 px no lado maior, sem passar de 512 px em imagens
  normais. A proporção é preservada, e a bolha e o anexo pendente mostram
  "Redimensionada automaticamente" quando a imagem foi alterada.
- O botão `Think` usa o campo `thinking` da mesma leitura de capabilities e só
  aparece quando o `no_thinking` global da cabeça está desligado. A escolha é
  enviada como `thinking_enabled` em cada chamada de Chat e fica no
  `ChatSession` persistido em `localStorage`, portanto continua entre mensagens
  da mesma conversa até o usuário mudar o toggle. A cabeça ainda impõe o
  desligamento global, mesmo que uma sessão antiga carregue `true`.
- Arquivos de texto continuam sendo recusados antes do envio quando o tamanho
  estimado ultrapassa o contexto efetivo do modelo: primeiro `context_length`
  de `/v1/models`, depois `n_ctx` do nó ativo. Imagens usam o mesmo orçamento,
  mas são reduzidas antes da validação; só uma imagem que não caiba nem em
  512 px gera recusa. O conteúdo textual e as partes multimodais ficam em
  `requestContent`, para o histórico reenviar o mesmo pedido estruturado. Cada
  anexo pendente pode ser removido sem apagar o rascunho.
- O botão de microfone só aparece quando existe `SpeechRecognition` ou
  `webkitSpeechRecognition`; a transcrição usa o locale da interface quando
  possível e entra diretamente no campo de texto.

`ChatSessionProvider` compartilha a sessão ativa e as ações entre o Chat e a
Sidebar. No `infer`, a Sidebar usa `ChatSessionList` como menu principal: a
lista de histórico tem rolagem interna limitada ao espaço disponível, destaca
a conversa ativa e deixa `Conversa nova` fixa acima dela. Trocar ou apagar uma
sessão não aborta uma geração em andamento; apagar a sessão apenas impede que
o stream termine recriando-a.

Esse menu é visto de **toda** rota, então escolher uma conversa ou começar uma
nova **navega para `/chat`** além de trocar a sessão ativa. Sem isso, o clique
feito das configurações ou da documentação mudava o estado compartilhado e
deixava a tela onde estava - o menu parecia ter parado de responder. Quem já
está no chat não é reenviado, para não empilhar entradas iguais no histórico do
navegador.

- **A telemetria da execução chega fechada** (`components/ExecutionTelemetry.tsx`): a tabela
  de fases e as seções densas de rede e ambiente ficam embaixo da resposta;
  abrir é um clique. A linha de resumo mostra `N do cache` ao lado dos tokens
  de entrada quando o turno reaproveitou o prefixo do turno anterior
  (`trace.cached_tokens`); é o que o pipeline **executou**, não o que o cache
  escolheu, e some quando não houve reuso - ver `inferencia.md`, seção 3.1.
  A mesma seção mostra o `trace_id`, perfil, estado cold/warm, timeline dos
  spans locais, `outside_node_total_ms`, `outside_node_accounted_ms` e
  `outside_node_unaccounted_ms`. O gargalo principal é destacado somente
  entre medidas presentes; tempo que não chegou a uma fronteira permanece
  explicitamente como não atribuído.
  No fim do stream, o `Bubble` usa temporariamente o trace, timings e origem de
  `telemetryCapture` quando a captura final chega antes da atualização imutável
  da mensagem. `telemetrySnapshots` guarda a captura completa por `messageId`,
  então mensagens anteriores continuam com seu próprio resumo rápido e card
  fechado. Depois que o histórico recebe `message.trace` e `message.timings`,
  eles continuam sendo a fonte preferida; essa transição mantém o rodapé visível
  sem recarregar a página.
- **Trocar de modelo no meio vira um evento no fluxo** (`role: "event"`, que
  `turnsOnly` filtra antes de a conversa viajar na requisição). O seletor fica
  embaixo do campo de texto. `lib/chatModels.ts` remove desse seletor os ids
  com o seletor de implantação `@d-<deploy_id>`; o catálogo do backend e as
  telas de gerenciamento de implantações continuam vendo os ids completos.
- **O título se edita clicando nele** e fica gravado (`titled` impede a
  primeira mensagem de sobrescrever o nome escolhido).

`chatStream` separa `<think>` do conteúdo visível sem vazar o delimitador
entre pedaços; `finishChatStream` grava o resumo de TTFT, velocidade de
pré-processamento do prompt, velocidade de geração da saída e total ponta a
  ponta, além do trace por hop. O Chat mostra essas linhas no resumo da
  resposta, além de `server_ttft_ms`, `engine_ttft_ms`, `client_ttft_ms` e o
  tempo de rede observado, antes do card fechado. O resumo fica visível sem
  expandir a telemetria e deixa explícito quando uma taxa não é mensurável,
  como no reuso total do prefixo ou numa resposta de um token. Durante o SSE,
  o navegador mede primeiro chunk,
primeiro token, duração, fim do último token, chunks e bytes com
`performance.now()`; o `Date.now()` anexado ao recebimento serve apenas para
correlação com os wall timestamps dos peers. Nenhum relógio de máquinas
diferentes é subtraído.
Se `/v1/chat/completions` enviar um frame SSE com o envelope de erro OpenAI,
`streamErrorMessage` preserva a mensagem real mesmo quando já há texto na
resposta; ela aparece em um bloco que quebra linhas, permite seleção e oferece
um botão de cópia. A mensagem genérica de resposta vazia só aparece quando o
stream termina sem tokens e sem erro.
Cada fase também mostra os bytes de estado oculto enviados e recebidos na
travessia e o `dtype` efetivamente usado, para comparar execuções f16/f8.
No decode, a linha do backend separa a prontidão observada pelo scheduler, a
wall de `ggml_backend_tensor_get` e o pós-processamento da captura, além da
conversão f8 e da serialização do frame residual. O total antigo de
captura permanece identificado como compatível; `tensor_get` pode incluir a
cópia D2H e o sync interno do backend CUDA. A tabela de transporte QUIC também
mostra a espera em `send_data` e `finish`, separada da espera por headers e do
RTT.
Ao terminar o stream, o Chat pede um snapshot de `/api/ui/chat/telemetry` para
os pares adjacentes do trace e `/api/ui/peer/:node_id/runtime` para cada máquina
do trace. O painel mostra o RTT total observado das chamadas, o RTT de cada
aresta com o rótulo origem -> destino, capacidade e configuração pedida,
colocação efetiva por buffers GPU/CPU/mistos e `POM_PROFILE_SYNC` individual de
cada peer. A colocação não é derivada de `n_gpu_layers`, e o estado de sync não
é deduzido de um tempo zero. Se a coleta falhar, a tela diz indisponível ou não
medido em vez de preencher com zero. Em uma pipeline de uma máquina, ela
explica que não existem arestas entre peers.

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

Quatro decisões que não são estética:

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
- **No celular são duas telas com um endereço só.** Abaixo de `sm:` a coluna
  de navegação ocupa a largura inteira e o leitor fica escondido
  (`mobileReaderOpen`); escolher um guia ou documento abre o leitor em tela
  cheia com um botão Voltar fixo no topo (só existe montado enquanto essa
  vista está aberta). Voltar mexe só nesse estado local - `?doc=`/`?guia=`
  continuam intactos, então recarregar a página não perde o lugar. Um deep
  link que já chega com `?doc=`/`?guia=` na URL conta como a mesma seleção e
  abre direto no leitor. A partir de `sm:` as duas colunas voltam a conviver,
  independentemente desse estado.

## A interface com o cluster em TLS

Com a política ligada, abrir `http://<peer>:8080/` devolve **301 para
`https://`** na mesma porta, e o painel funciona normalmente a partir daí. O
certificado que o navegador recebe é um ECDSA auto assinado, diferente do
Ed25519 que os peers recebem - navegador não verifica Ed25519. Ver
`rede-p2p.md`, "Dois certificados na mesma porta".

**O navegador avisa na primeira visita de cada máquina**, porque o certificado é
auto assinado e não há CA para validá-lo. É esperado; o operador aceita uma vez
por máquina.

## O card do Fluxo diz quanto do modelo coube na GPU

Uma camada na CPU é ordens de grandeza mais lenta que na GPU, e esse é o
primeiro motivo de uma inferência arrastada. O card do Fluxo mostra a divisão
(`LayerPlacementMeter`, em `ModelNetworkCanvas.tsx`) na mesma forma dos medidores
de VRAM e RAM, logo abaixo da faixa de camadas: barra âmbar e "12 GPU · 10 CPU"
quando sobra camada na CPU, barra neutra e "22 na GPU" quando não sobra. O
duplo clique continua abrindo o detalhe completo; isto é resumo, não
substituição.

O número **não** sai de `NodeConfig.n_gpu_layers`. Esse é o *pedido*, e o pedido
quase sempre é o sentinela 99 ("descarregue tudo"); quem decide é o `fit` do
llama.cpp, que baixa o número até caber na VRAM. Derivar a divisão do pedido
responde "todas na GPU" exatamente quando algumas não estão - o único caso que
o operador precisa enxergar. O que vale é `RuntimeStatus.reported_gpu_layers`,
que o nó lê do `n_gpu_layers_effective` do próprio `/health` do servidor de
inferência, no mesmo poll de 3s que já lhe dá `reported_total_layers`. Nenhuma
chamada nova: o `/api/ui/cluster` já busca o `/api/ui/node` de cada peer.

Essa busca tem dois desfechos de erro, e a tela os separa. `unreachable` é o par
que atendeu no endereço anunciado e cuja leitura quebrou - beco sem saída, ponto
rosa, e a mensagem diz o erro. `rediscovering` é o par cujos endereços todos
deixaram de responder e cujo endereço atual o nó já foi pedir à rede: ponto âmbar
a pulsar e "Reencontrando este nó", porque endereços IPv6 temporários rodam
sozinhos e o estado se resolve na volta seguinte do gossip. Dizer "nada abaixo
pode ser lido" nesse caso mandava o operador procurar um defeito que se conserta
sozinho - mas a promessa tem prazo de 90 s, e depois disso o card volta a
`unreachable`, porque um par que anuncia e não atende existe. Nos dois casos as camadas do par continuam contando para a cobertura -
o servidor de inferência dele pode estar perfeito e só o endereço de controle é
que morreu - então o card diz isso, e o cartão "Cobertura" do topo perde o verde
de "completo" e mostra `dashboard.coverageUnconfirmed` enquanto
`model_inventory[].unreadable_nodes` do modelo ativo não estiver vazio.

Sem relato do motor a linha não aparece. `layerPlacement` (`src/lib/layerPlacement.ts`)
devolve `null`, e a tela cala em vez de chutar - dizer "tudo na GPU" sem saber
seria pior que o silêncio. Negativo é "todas" na convenção do llama.cpp, e lido
como zero diria que a máquina inteira roda na CPU.

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

- **Fatiar** (`SliceModels`): Pipeline e Tensor Parallelism são um par de abas
  (`role="tablist"`) no topo do painel de origem, não uma caixa de marcar - cada
  modo tem seus próprios controles embaixo. A aba Tensor Parallelism só existe
  quando `useNode().data.pom_desenv === true` (`NodeInfo.pom_desenv`, o mesmo
  `POM_DESENV=1` que já esconde as seções DESENV do `DeploymentInspector`); sem
  a flag a tela nunca oferece nem envia `mode: "tensor_parallel"` - o único
  modo possível é pipeline, e um `mode` que já estivesse em `tensor_parallel`
  quando a resposta de `/api/ui/node` chega depois é revertido. Pipeline
  oferece plano por tamanho (alvo em GB por fatia, vazio =
  "o resto") ou uma faixa específica por início/fim de camada; a faixa
  específica usa as opções arquiteturais do GGUF, envia
  `range: { start_layer, end_layer }` e acrescenta a parte ao manifesto por
  hash, sem substituir nem apagar fatias anteriores. No modo TP, o operador
  escolhe 2..64 ranks e a prévia mostra uma faixa completa por rank com a
  identificação `1of2`, `2of2`, `1of4` etc. Cada alteração posta
  `POST /api/ui/slice/plan` (debounce 260 ms); TP leva `mode: "tensor_parallel"`
  e `tp_size`, enquanto pipeline leva `slices` ou `range`. `POST /api/ui/slice`
  roda o job. O painel de atividade fica visível também para jobs terminados e
  mostra fase nomeada, bytes, taxa, ETA quando confiável, arquivo atual e
  saídas parciais em caso de falha. A prévia confere o espaço livre no destino
  real configurado, com número concreto e alerta antes de iniciar. A tela Fatias separa
  `parts` e `tp_parts`, agrupa TP por tamanho e ordena os ranks dentro de cada
  grupo. A transferência considera ambas as listas. Quando o catálogo confirma
  um GGUF multipart, esta tela mostra a soma real dos bytes, a etiqueta
  MULTIPART e a lista de partes físicas. Enquanto o cabeçalho ainda está sendo
  enriquecido, não mostra o tamanho da primeira parte. Se a fonte estiver
  incompleta, o motivo vindo de `split.count`, `split.no` ou da parte
  ausente aparece no painel e o botão de fatiar fica desabilitado; o operador
  precisa colocar o conjunto completo junto e deixar o POM gerar as fatias.
  A prévia e o início do job repetem o preflight do GGUF para a cabeça exigida
  pelo loader. Em `deepseek4`, uma faixa intermediária copia os pesos reais da
  cabeça que o loader exige, e a prévia mostra o payload adicional repetido em
  cada uma; se a tabela de tensores não permitir a cópia, a recusa lista o
  motivo concreto. A faixa final só é oferecida quando a tabela contém a
  cabeça completa.
- **Implantar** (`DeployModel`): abre o Deploy Studio em tela ampla, com abas de
  Fatias/Modelo inteiro/Tensor, modelo técnico e nome operacional separados,
  estratégias Simples/Completa, mapa central e inspetor compacto de propriedades.
  Todos os peers aparecem na faixa inferior e podem ser arrastados para o mapa;
  no tab Globais, o inspetor também permite escolher `cache_type_k` e
  `cache_type_v` para a instalação; cada mudança dos combos é aplicada por
  `POST /api/ui/cache-types`, sem botão de salvar, subtítulo ou badge `global`;
  o planner recalcula a sugestão considerando cortes existentes, memória, RTT e
  carga. A barra de cobertura mostra camadas faltantes e sobrepostas; o mapa
  exibe hops, capacidade e latência conhecida, além de rotas alternativas e
  distribuição projetada de carga. Cada card separa a faixa física carregada da
  sub-faixa efetivamente executada.
  O inspetor é um componente próprio (`components/DeploymentInspector.tsx`), e é
  **o mesmo** que o Histórico usa em somente leitura: uma lista de propriedades
  só, e não duas cópias que divergem no primeiro campo novo. Ele tem três
  categorias, e a fronteira entre elas é de quem é o parâmetro - *Configurações
  padrões* (o que o llama.cpp já tem: contexto, sessões, máx. tokens, batch,
  chunk de prefill, amostragem, cache K/V da instalação, desativar
  thinking/embeddings, especulativa e MTP), *Configurações POM* (motor de
  inferência, ajustar à memória, chunks em voo, juntar fatias vizinhas e
  precisão do estado oculto) e *Configurações POM (DESENV)* (motor de
  inferência, as três janelas do detector de ciclo, o interruptor dele e o
  recuo do MTP para a RAM, além do descarregamento ocioso por peer). A seção
  só aparece quando `NodeInfo.pom_desenv` está ligada; a ausência da flag faz a
  interface remover seus controles do DOM. O `[i]`
  vem **antes** do nome, e não depois: com nomes de larguras diferentes, os
  ícones à direita do texto ficavam serrilhados, e antes do nome eles formam uma
  coluna reta do topo ao pé do painel. O combo do motor não oferece
  "Automático" e assume `llama.cpp` ao montar - é escolha de apresentação; o
  backend continua aceitando `auto` somente quando o pedido o informa
  explicitamente. Sem valor, o padrão é `llama.cpp` (`dense`).
  Os cards são um **preview de como o sistema fica depois da implantação**, não
  do estado atual: cada GPU de cada máquina ganha uma barra de VRAM projetada
  (uso previsto contra o total, com o uso de agora como tracinho) que se
  recalcula a cada ajuste de contexto, lote ou requisições simultâneas. A
  projeção vem de `POST /api/ui/deploy/capacity-preview`, que roda o mesmo
  ajuste automático do llama.cpp no peer sem carregar peso (ver
  `implantacao.md` §3.1). O card fica amber quando cabe só porque o "Ajustar à
  memória" apararia o contexto ou as camadas de GPU, e rósea quando não cabe
  nem no piso; os dois casos pedem uma confirmação nomeando as máquinas ao
  `Implantar` - é aviso, não bloqueio, para o operador reduzir ou aumentar o
  contexto como quiser. Cada parâmetro do painel direito tem um `[i]` que abre
  no hover/foco/toque com o que ele significa e como afeta a inferência; quando
  o nó reporta uma capacidade duvidosa ou limitada, o mesmo lugar da coluna
  mostra o triângulo âmbar com a razão. As arestas vêm do grafo de execução do
  planejador: ligam apenas sub-faixas contíguas, mantêm ramos equivalentes de
  entrada e mostram o RTT direcional usado para escolher entre eles. O canvas
  não inventa pontes pela ordem dos cards, o que preserva uma entrada rápida
  quando outra faixa carregada se sobrepõe. O Tensor usa o modelo inteiro em todos os
  ranks e não apresenta a seleção antiga de fatias. As propriedades globais
  (contexto, batch, amostragem, sessões) ficam
  separadas das propriedades por
  máquina (`n_gpu_layers`, o booleano de KV na GPU e capacidade local, na mesma linha de propriedade
  das globais - `[i]`, nome, controle); ao selecionar uma máquina, o
  inspetor também mostra as camadas que ela executará. A precisão do estado
  oculto (f16/f8) só aparece quando o estado atravessa a fronteira de um peer -
  um pipeline de fatias com faixas adjacentes distintas; em modelo inteiro,
  Tensor ou fatias numa máquina só, o estado nunca é serializado e o seletor
  seria inerte. Nas globais, a seção
  de fatos do modelo inteiro e seu seletor de máquina não são exibidos: a
  máquina escolhida no mapa é a única que vale para a implantação. Nas globais,
  a seção de execução separa a especulação por lookup do MTP; ao marcar MTP, a
  probabilidade mínima começa em `0,75`, mas continua sendo um campo editável
  pelo operador e o valor alterado segue no deploy. A seção
  **Motor de inferência** aparece somente quando a metadata real do GGUF
  informa mais de um expert. A UI apresenta `dense` (llama.cpp) e `moe`, e
  envia a escolha nos modos modelo inteiro, pipeline fatiado e Tensor
  Parallelism. O backend também aceita `auto` somente quando ele chega como
  escolha explícita de uma API ou implantação persistida. Em modelo denso a
  seção fica oculta. Se a
  combinação escolhida não for suportada, o seletor continua visível com a
  razão explícita e o deploy fica bloqueado; a telemetria é a fonte de verdade
  sobre o motor executado. As demais opções condicionais recebem
  `DeployPlan.capabilities` do backend: `no_thinking` usa
  `tokenizer.chat_template` e o vocabulário GGUF, MTP usa metadata/tensores
  `nextn` do arquivo selecionado e usa o runtime do peer final apenas como
  limitação, e lookup usa as capacidades runtime de todos os alvos. Ausência
  comprovada no modelo oculta a opção; `unknown` deixa-a visível e acionável,
  com fonte, motivo e risco. Limitação de runtime também é aviso acionável. O
  switch `no_embeddings` permanece visível: ligado, peers comprovadamente
  inseguros recusam a implantação de forma nominal e atômica; topologia
  indeterminada pode seguir somente com aviso explícito.
  **Fatias** traz o interruptor "Juntar fatias vizinhas na mesma máquina"
  (`merge`), que é o que permite uma máquina ficar com duas faixas adjacentes:
  o nó as funde num arquivo só (`splitter::merge`) antes de subir, porque
  `split` renomeia os blocos de toda fatia para `blk.0..` e duas não carregam
  lado a lado. Sem ele o planejador recusa o par, e a recusa manda "juntar as
  fatias vizinhas na tela" - o interruptor existe para essa frase ter destino.
  Ele aparece sob a mesma condição que decide se `merge` vai na requisição
  (`!isWhole`): controle sem envio, ou envio sem controle, foi exatamente como
  ele se perdeu uma vez. Não existe uma segunda
  tabela legada abaixo do canvas: o mapa e a faixa inferior concentram a
  topologia, os RTTs direcionais e a inclusão de peers. Cada edição volta ao nó para validação; o deploy
  (`POST /api/ui/deploy`) **re-varre o cluster** em vez de confiar na prévia,
  e o progresso vem de `GET /api/ui/jobs`. O mapa usa os mesmos primitives do
  canvas do Painel: pontos no fundo, cards arrastáveis, zoom, Fit, Organizar e
  Redefinir, com posições persistidas por implantação. O inspetor tem 19rem,
  pode ser recolhido pela borda esquerda, começa diretamente pelas abas
  Globais e Máquina, sem título próprio e sem o padding externo `p-3`, e separa
  visualmente propriedades globais e da máquina selecionada. Avisos de
  capacidade ficam como ícones âmbar ao lado do controle: o texto completo,
  incluindo fonte e motivo, abre por hover, foco ou clique em um portal fixo que
  quebra palavras e identificadores sem espaços sem criar overflow horizontal.
  O conjunto de alertas permanece distribuído junto dos respectivos controles,
  em vez de formar uma fileira de cartões. O inspetor permite atribuir ao peer uma faixa que
  ele já possui trocando a faixa ativa daquele peer sem mover outra máquina. A mesma faixa em dois
  peers é uma rota paralela válida, não uma troca. Clicar de novo numa faixa já
  atribuída tira a seleção - o mesmo botão é o caminho de ida e de volta. A
  saída remove só aquela faixa naquela máquina: as réplicas da mesma faixa em
  outros peers ficam onde estão, e o peer continua no mapa mesmo sem faixa
  nenhuma, porque tirar uma camada não é o mesmo gesto que "Remover da
  implantação". Peers que já estão no mapa
  deixam de aparecer na barra inferior; arrastar ou dar duplo clique num peer
  disponível adiciona o card sem redesenhar a topologia até que uma faixa seja
  atribuída. Em modelo inteiro, o mapa aceita uma máquina. Ao trocar de fatias para esse
  modo, o mapa fica vazio para que o operador escolha a máquina; depois de uma
  escolha, tentar incluir uma segunda mostra um aviso e preserva a máquina
  escolhida. No Tensor, somente peers que estão no mapa entram como
  primary/worker, mesmo que outros tenham o modelo.
  GPU, RAM e carga são atualizados pela visão do cluster a cada 5 segundos; o
  mapa do Painel mostra o último RTT e transporte efetivos observados na
  execução de inferência para cada aresta e, no Gateway, todas as rotas
  completas do modelo com o RTT agregado de cada perna, inclusive o retorno;
  quando há trace recente, o RTT de fio da execução vence a sonda libp2p de
  fallback.
  As rotas aparecem do menor custo ao maior, com o id dos nós como desempate;
  uma implantação de modelo inteiro com um único head também é consultada,
  mesmo sem aresta no grafo. O
  survey direcional do planner continua reservado à escolha de rota e não é
  exibido como se fosse a execução. Uma GPU ausente aparece como `sem GPU`, sem barra
  indeterminada. Em Completa, todos os peers entram no canvas e na lista de
  candidatos nos modos de fatias e Tensor; modelo inteiro mantém a restrição de
  uma máquina. A cobertura fica reduzida no rodapé junto da barra de peers.
  A última implantação de cada modelo
  fica no `localStorage` (`deployMemory`) e volta preenchida após o primeiro
  plano, podada contra o que ainda existe.
  Alterações de topologia são validadas por `POST /api/ui/deploy/plan` após o
  debounce de 260 ms. Parâmetros de runtime, overrides de GPU/KV por máquina e
  o nome operacional não mudam as máquinas ou faixas, então permanecem locais
  durante a edição e não reiniciam o survey distribuído. A chave da consulta
  contém somente faixas e máquinas; o deploy leva os overrides atuais. Se algum
  deles mudar, a projeção de capacidade antiga é ocultada; `POST /api/ui/deploy` faz o preflight completo com os valores atuais
  antes de iniciar a implantação.
  A resposta inicial do catálogo pode conter somente nome, caminho, tamanho e
  dados do manifesto, enquanto o nó lê os cabeçalhos GGUF em background. A
  tela mostra o inventário imediatamente. O catálogo lê somente o KV header e
  `stat` das partes na varredura rápida: não percorre a tabela de tensores, não
  calcula hash de modelos inteiros e não mapeia os pesos. A tarefa de
  enriquecimento pode validar o `pom_id` de uma origem para herdar um projetor,
  depois que o inventário já foi publicado. O botão e o endpoint de execução verificam somente os
  metadados necessários para a seleção atual: para um modelo inteiro pendente,
  o planner envia seu caminho já listado a `POST /api/ui/model-metadata`, que
  enriquece esse item sem esperar os vizinhos. Outro modelo ainda em
  enriquecimento, peer silencioso ou peer antigo sem `metadata_complete` não
  bloqueia. Quando a própria seleção ainda não pode ser resolvida, o plano
  devolve um único `blocked` que nomeia a faixa ou o modelo pendente. O
  enriquecimento registra início e conclusão; um erro terminal fica em
  `metadata_error` no modelo e não impede os demais, enquanto
  `metadata_complete` vira `true` ao terminar a passagem. A resposta publicada
  é reaproveitada pelas requisições. A primeira chamada instala um placeholder
  e compartilha a única varredura em voo, por isso F5 e trocas repetidas na combo
  não empilham scans ou enriquecimentos. Se o refresh trocar o inventário, o
  backend reconcilia o resultado por `path + size + mtime` e preserva os modelos
  já lidos; somente arquivo novo ou alterado volta ao cabeçalho. Nova tentativa
  só ocorre em refresh por TTL ou invalidação, e descartes de tarefas ficam
  registrados no log.
  **O botão cinza sempre diz por quê.** A comparação entre o plano do nó e as
  linhas da tela (`sameRows`) ordena os dois lados por camada, porque a tela
  mantém as linhas nessa ordem e o nó monta a resposta do jeito dele; e os
  painéis de explicação renderizam sob `answered` (o nó respondeu sobre *estas*
  linhas), nunca sob `validated`. O planner aceita várias faixas na mesma
  máquina, mas mantém `blocked` para gaps globais, sobreposição e outros
  problemas reais; `whyNot` põe a razão em uma linha ao lado do botão, inclusive
  nos dois casos sem painel (nada selecionado, resposta a caminho). O mapa
  mostra hops consecutivos somente para frente, acrescenta um Gateway virtual
  no fim com a perna de retorno quando a cauda ainda não é o gateway e exibe
  todas as rotas candidatas, destacando no
  total a de menor RTT. A rolagem interna em viewport estreito não
  corta cards nem cria overflow na página.
- **Implantações** (`Deployments`): uma linha por `deploy_id`, "no ar" separado
  de "padrão", ordenada pela criação. Cada linha expande e lista as versões
  daquele `deploy_id` (o histórico de `RuntimeParams` já rodados sob o mesmo
  id), com a versão ativa marcada. "Reimplantar esta versão" manda
  `{ version }`; "Reimplantar com novos parâmetros" abre o formulário semeado
  do `RuntimeParams` da versão ativa e manda `{ params }` - o backend reusa a
  versão de parâmetros iguais ou anexa uma nova ao mesmo id. Reimplantar do
  Deploy Studio a mesma forma (mesmo modelo provado, mesmas máquinas e faixas)
  também cai no mesmo `deploy_id`; trocar de máquina ou de faixa cunha um id
  novo. Ver `implantacao.md` §7.1.
- **Transferir** (`Transfers`/`SendFile`/`Notifications`): downloads de modelos
  remotos com progresso e cancelamento, ofertas pendentes
  (aceitar/rejeitar direto no sino), transferências ativas (bytes, velocidade,
  ETA, cancelar) e concluídas. O envio ("enviar um arquivo desta máquina")
  oferece qualquer caminho legível via `POST /api/transfer/send`; o receptor
  só vê uma concessão opaca (`grant`), então "mandar qualquer coisa" nunca
  vira "ler qualquer coisa". O progresso vem dos nós, então recarregar a aba
  não perde nada. `POM_DEV_AUTO_ACCEPT_TRANSFERS=1` (variável do processo
  receptor, lida no arranque) aceita toda oferta sem clique - só dev.
  Peers da mesma máquina compartilham o hostname, então o destino e a etiqueta
  de disponibilidade levam o começo do `node_id` quando o nome se repete
  (`uniqueName`, `routes/Shards.tsx`). Ofertas, ativas e concluídas em
  `Transfers` são cada uma uma tabela de largura total (era uma lista de
  cartões), com a ação de aceitar/rejeitar/cancelar na última coluna.

### Informações da rede e materiais de entrada

`/config/rede` busca `GET /api/ui/network/info` e mostra o mesmo cartão para
todos os perfis. Node e infer veem nome, descrição, regras, organização,
canais, visibilidade, país, cidade e localização em modo somente leitura. O
administrador autorizado edita esses campos e salva em
`POST /api/ui/network/settings`; o backend grava a configuração local,
propaga a mutação assinada aos peers e republica o rótulo geo público.

Abaixo das métricas, o painel "Travessia de NAT" (`TraversalPanel`) mostra o
campo `traversal` de `GET /api/ui/cluster`: estado do AutoNAT, endereços
diretos e circuitos publicados, reservas de relé aceitas e a contagem de hole
punches com a última falha. Ele existe porque dois nós atrás de NAT que não se
enxergam falham num de três degraus, e antes dele descobrir qual exigia
`RUST_LOG=debug` na máquina certa. Nó de binário antigo não manda `traversal` e
o painel simplesmente não aparece.

O mesmo administrador tem, nessa tela, os controles de copiar e baixar o
documento `.pom-admin-trust.json`. Em rede local ou privada também aparece a
chave de conexão para copiar ou baixar. O wizard real oferece os mesmos
materiais na etapa de convite, destacando a chave de conexão no primeiro
cartão. O conteúdo bruto do trust e da chave pública começa recolhido, mas
copiar e baixar continuam disponíveis sem expandir os cartões. `SetupWizard.tsx`
continua sendo apenas o protótipo visual e não participa dessas chamadas.

O painel também expõe `Esquecer rede`, com confirmação destrutiva. Essa ação
chama `POST /api/ui/network/leave`, que revoga a confiança local, prepara o
`pom.json` para o setup e reinicia este nó; depois a interface volta para
`/setup`. Ela é deliberadamente diferente de `Sair da rede`, no painel
`Esta máquina`, que chama `POST /api/ui/leave` e apenas tira a máquina do
cluster sem apagar sua configuração local. Essa ação também fica disponível
para um perfil node sem autoridade administrativa e não depende de modelo
carregado ou estado Ready: o handler para o runtime, limpa o journal da
implantação e desanuncia a máquina antes de responder.

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
- DEFEITO ANOTADO: em janela estreita (~420 px) a coluna de conteúdo estoura na
  horizontal e os painéis saem cortados à direita - `node_id`, texto de ajuda e
  botões ficam fora da viewport. Não é de uma tela só: reproduz igual em
  `/config/rede` e em `/config/preferencias`, então é da moldura, não do painel.
  Verificado em captura headless a 420x900 nas duas telas.

## O nome de uma máquina é um só

`node_name` do `pom.json` é como o operador batiza a máquina, e é o nome que o
nó anuncia aos peers (`network.hostname`). Toda tela lê esse nome; o seletor
"Enviar para..." de `/fatias` lia `sysmon`, o hostname cru do sistema
operacional, e era o único lugar do aplicativo que chamava a máquina por outro
nome. Numa máquina com dois nós era pior: os dois relatavam o mesmo hostname do
SO, a lista via colisão e desempatava com o prefixo do `node_id`. `/api/ui/models`
e `/api/ui/cluster/models` respondem o nome anunciado, e um peer que não
respondeu ao levantamento ainda leva o nome que anunciou - sem ele a tela caía
no `node_id`, um prefixo hexadecimal. A apresentação está em
`lib/peerName.ts`.
