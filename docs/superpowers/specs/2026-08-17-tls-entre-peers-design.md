# TLS auto assinado entre peers, decidido pelo nó principal

Data: 2026-08-17

O nó principal ganha um interruptor em Configurações / Rede que liga TLS em todo
o HTTP trocado entre peers do cluster. A flag é dele e só dele; os demais nós a
herdam. A barra de topo passa a dizer, em toda tela, se o cluster está cifrado
ou não, e a telemetria de inferência separa o custo do handshake do resto.

## Por que

Hoje todo HTTP entre peers vai em texto claro: `/v1/prefill` e `/v1/step`
carregam **estado oculto em f16** - a representação interna do prompt do
usuário -, a transferência carrega pesos de modelo, e as rotas `/admin/*` e
`/api/ui/*` carregam a chave de conexão, que é a credencial de operador. Numa
LAN compartilhada ou num salto pela internet, isso é legível e alterável por
quem estiver no caminho.

A escolha não pode ser por máquina. Um cluster meio cifrado não é um cluster
cifrado: basta um salto em claro para o estado oculto daquele salto vazar. Por
isso a decisão é do principal e vale para a rede inteira.

## Escopo

**Dentro:** todo HTTP entre peers - `/v1/prefill`, `/v1/step`, gateway,
transferência de arquivo, `devupdate`, e a sonda `/health` da `RouteTable`.

**Fora, com razão:**

- **Gossip UDP** (`network.rs`: Announce, PeerRequest, PeerList, Heartbeat).
  TLS não se aplica a datagrama, e o conteúdo é endereço, faixa de camadas e
  capabilities - nenhum dado de usuário, nenhum peso, nenhuma credencial.
  Cifrá-lo é outro projeto, com protocolo novo e quebra de compatibilidade.
- **libp2p** (`discovery.rs`, `infer_tunnel.rs`, `bitswap.rs`). Já é cifrado
  ponta a ponta por Noise. Sobrepor TLS não acrescenta segurança nenhuma.
- **A UI e a API do operador no navegador.** Um certificado auto assinado faz o
  navegador gritar em toda máquina do cluster; o ganho não paga o custo
  operacional. Fica para uma decisão separada.
- **Loopback.** `127.0.0.1` continua em claro sempre - ver "Laço de accept".

## Confiança: o node_id já é o fingerprint da chave

`NodeId::from_pubkey` (`network/identity.rs`) é `hex(SHA256(pubkey))` da chave
Ed25519 de longa duração em `.pom-node.key`. O cluster inteiro já nomeia cada
máquina por esse hash e já o distribui pelo gossip. Isso resolve de graça o
problema que um certificado auto assinado normalmente cria.

Um auto assinado sozinho **cifra mas não autentica**: a única forma de aceitá-lo
é desligar a verificação, e aí quem estiver no meio apresenta o próprio auto
assinado e é aceito. O desenho evita isso amarrando o certificado à identidade
que já existe:

- **Emissão.** No arranque, o nó gera **em memória** um certificado auto
  assinado cuja chave é a própria `.pom-node.key`. `rcgen` aceita o PKCS#8 do
  `ed25519-dalek`; o rustls assina TLS 1.3 com Ed25519. CN e SAN carregam o
  node_id. Nada é escrito no disco - o certificado é derivado, não guardado, e
  regenerá-lo a cada boot não muda a identidade.
- **Verificação.** Um `ServerCertVerifier` próprio extrai a pubkey do SPKI do
  certificado apresentado, calcula SHA256 e compara com o node_id que o gossip
  já ensinou para aquele peer. Casou, é o nó certo. Não casou, o handshake
  morre.
- **O que é ignorado de propósito:** validade, cadeia e nome no certificado. A
  confiança não vem deles, e checá-los só produziria falha operacional (relógio
  torto, nome que não bate com o IP discado). Isso fica documentado no código,
  porque um `ServerCertVerifier` que aceita tudo *parece* um buraco quando lido
  fora de contexto.

Sem CA, sem arquivo a distribuir, sem renovação, sem revogação - um nó bloqueado
sai por `blocked_nodes` como já sai. Para se passar por um nó é preciso a chave
privada dele, que é a definição de ser aquele nó.

**mTLS fica de fora.** O cliente já se autentica pela chave de conexão no
header, que é a credencial que o resto do sistema usa. Somar autenticação de
cliente no TLS duplicaria a política de autorização em dois lugares.

## Canal

### Laço de accept: uma porta, as duas falas

`gateway.rs:1217` e o listener do servidor de inferência trocam o
`axum::serve` por um laço de accept próprio que **espia o primeiro byte** com
`TcpStream::peek`, sem consumi-lo: `0x16` é `ContentType::Handshake` do TLS e a
conexão sobe pelo `tokio-rustls`; qualquer outra coisa é HTTP em claro. As duas
terminam no mesmo router, servidas pelo builder automático do `hyper-util`.

Uma porta só significa: nenhum anúncio muda, nenhuma regra de firewall nova,
nenhum campo novo no `PeerInfo` (que já está no limite da MTU do `PeerList`), e
**a virada não precisa ser simultânea** - enquanto uns peers ainda falam claro,
o servidor atende os dois.

Com a flag ativa, conexão em claro vinda de fora do loopback é recusada logo
após o peek, com log nomeando o endereço de origem.

**Loopback fica sempre em claro**, e isso é requisito, não descuido:
`infer_tunnel::serve_loopback` repassa para `127.0.0.1:<inference_port>`, a
sonda de `runtime.rs:568` bate em `127.0.0.1/health`, e o `gateway::testkit`
sobe servidores em `127.0.0.1:0`. Nenhum deles tem par de chaves a apresentar, e
tráfego que não sai da máquina não tem o que proteger.

### Cliente: um `Client` por peer

O verifier precisa saber qual node_id esperar, e o `reqwest::ClientConfig` é
fixado na construção. Logo o `http_client` único de `main.rs:728` e `main.rs:1267`
vira um cache `Arc<RwLock<HashMap<NodeId, Client>>>`, construído sob demanda e
esvaziado quando a flag vira. O cache é limitado pelo tamanho do cluster.

*Alternativa recusada:* pôr o node_id no SNI (`<node_id>.pom`) com
`resolve_to_addrs`, o que permitiria um `Client` só. As resoluções do reqwest
são estáticas na construção do cliente, então o ganho evapora num cluster onde
peers entram e saem.

**Risco de integração conhecido, a inventariar no plano:** alguns pontos montam
a URL só com host e porta, sem carregar o node_id do destino -
`inference/direct.rs:191` e `backend/devupdate.rs:115` são os que já
encontramos. Cada chamador precisa passar a carregar o node_id até a borda. O
plano de implementação começa por esse inventário; um chamador esquecido é um
salto que continua em claro, e é exatamente o que a coluna `tls` por salto da
telemetria vai denunciar.

### Dependências novas

`rustls`, `tokio-rustls`, `rcgen`, `hyper-util`, e `reqwest` migrando para
`rustls-tls-manual-roots` (hoje usa o TLS do sistema, que não aceita verifier
próprio).

## A flag e a catraca

`Config.tls_required: bool`, com `#[serde(default)]` false para todo `pom.json`
existente continuar válido.

**Autoridade.** Escrita só por `POST /api/ui/network/tls`, que responde 403 se
`role != Role::Principal`. A verificação é no servidor; esconder o botão na tela
é conveniência, não controle.

**O gossip nunca é fonte de verdade.** O Announce ganha um campo `tls`, mas ele é
**gatilho, nunca valor**. O gossip UDP é em claro e não autenticado: se a flag
viesse dele, qualquer um na rede anunciaria `tls: false` e rebaixaria o cluster
inteiro para texto claro - justamente o ataque que o TLS existe para impedir.
Então:

1. O peer vê um Announce do principal cuja flag discorda do seu cache.
2. Isso dispara `GET /api/cluster/tls` **no principal**, autenticado com a chave
   de conexão, e já sobre TLS se a flag local estiver ligada.
3. Só a resposta dessa busca é gravada em `pom.json`.

Um intruso pode disparar a busca à vontade; não pode responder por ela, porque
não tem a chave de conexão.

**Fail-closed.** Se a busca falhar - principal fora do ar, rede partida - o valor
local **permanece**. Um principal indisponível nunca rebaixa o cluster.

**Sem reinício.** Ligar ou desligar esvazia o cache de `Client`; o servidor já
aceitava as duas falas desde o arranque. A janela em que uns peers já sabem e
outros não dura um ciclo de gossip (5 s), e nela um peer estrito recusa quem
ainda fala claro.

## Interface

**Barra de topo** (`components/Topbar.tsx`). Um `Badge` ao lado do `NodeChip` e
antes do sino: `ShieldCheck` em tom `aqua` com "TLS" quando ativo, `ShieldOff`
em `amber` com "sem TLS" quando não. O dado sai do `useNode()`, que já é polled -
nenhuma chamada nova. Em tela estreita fica só o ícone, seguindo o
`hidden sm:inline-flex` dos badges vizinhos. Clicar leva a Configurações / Rede.

**Configurações / Rede** (`routes/NetworkSettings.tsx`). Painel "Criptografia
entre nós" com o interruptor. No principal é editável; nos demais aparece em
leitura, dizendo que o principal decide e nomeando qual nó é o principal. Textos
em pt e en em `i18n/locales.ts`.

## Telemetria de inferência

`HopTiming` (`inference/relay.rs:34`) ganha dois campos, que entram no `x_pom`
junto dos `*_ms` que já existem.

- **`tls_handshake_ms`** - medido **no servidor**, cercando o
  `TlsAcceptor::accept` do laço de accept, onde o tempo é exato. Volta no `meta`
  da resposta e é agregado pelo relay como os `node_timings` já são.
- **`tls: bool`** por salto - qual salto foi cifrado. Também é o que expõe
  visualmente uma virada pela metade ou um chamador que ficou para trás.

**O que esse número é, e a tela precisa dizer.** O handshake acontece **uma vez
por conexão**, e as conexões ficam no pool do `reqwest`. Então
`tls_handshake_ms` vem preenchido no salto que abriu conexão nova e **zero nos
demais** - o que é a verdade, não uma falha de medição. Apresentá-lo como custo
por token faria o operador concluir que TLS é grátis, ou que é caro, conforme a
requisição que ele olhasse. A `Chat.tsx` rotula o campo como custo de
estabelecimento de conexão, não custo por requisição.

O custo por requisição depois do handshake é a cifra do record layer, diluída
dentro de `response_body_ms` e não separável sem instrumentar o socket. A tela
nomeia essa limitação em vez de inventar uma coluna.

## Testes

**Unitários (Rust):** certificado gerado da identidade passa no verifier;
certificado de outra chave é recusado; o peek escolhe TLS ou claro pelo primeiro
byte; conexão em claro de fora do loopback é recusada sob flag ativa e aceita do
loopback; a catraca ignora Announce forjado e só grava o que veio da busca
autenticada; a busca que falha preserva o valor local.

**Integração (Rust):** dois nós pelo `gateway::testkit` - nunca UDP de verdade,
que a suíte transmite na LAN -, um com TLS e um sem, exercitando prefill e step
nas duas falas.

**Front:** badge da Topbar nos dois estados; painel de Rede em modo leitura fora
do principal; `Chat.telemetry.test.tsx` cobrindo os campos novos.

## Documentação, no mesmo commit

- `docs/subsystems/rede-p2p.md` - o canal, o pinning pelo node_id, a catraca.
- `docs/subsystems/api-http.md` - `POST /api/ui/network/tls` e
  `GET /api/cluster/tls`.
- `docs/subsystems/web-ui.md` - badge da Topbar e painel de Rede.
- `docs/subsystems/inferencia.md` - `tls_handshake_ms` e `tls` por salto, com a
  ressalva sobre pool de conexões.
- `crates/node/src/network/AGENTS.md` - o `tls.rs`.
- `AGENTS.md` raiz - invariante nova: **o gossip nunca é fonte de verdade da
  flag de TLS**; ela só é gravada a partir de busca autenticada no principal, e
  falha de busca preserva o valor local.
