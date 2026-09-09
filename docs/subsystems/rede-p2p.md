# Subsistema: rede P2P e descoberta

O subsistema de rede resolve o problema de um nó pom achar os outros nós
do cluster e se anunciar para eles. São dois mundos no mesmo diretório
(`crates/node/src/network/`): o transporte nativo UDP (`network.rs`, `addrs.rs`,
`identity.rs`, `route.rs`), que é o coração da LAN, e o caminho libp2p/pkarr
(`discovery.rs`, `pkarr_node.rs`, `bitswap.rs`), desenhado para a internet
(Circuit Relay v2, DCuTR, AutoNAT, Kademlia, gossipsub, DHT pkarr). A regra da
casa é que a decisão de endereço pertence a quem vai discar, não a quem anunciou
- daí `addr_candidates` somado ao `RouteTable` (ver `crates/node/src/network/AGENTS.md`).

`main.rs` monta `Network` e `Discovery`; `backend/gateway.rs` e
`routing/pipeline.rs` consomem `PeerInfo` e `RouteTable`.

## 1. Visão geral e topologia

- **Caminho UDP (LAN):** socket UDP dual-stack, broadcast `255.255.255.255`,
  bootstrap explícito e gossip periódico. Funciona sem infraestrutura nenhuma na
  rede local e é o único transporte que sempre fala (anuncia mesmo sem modelo).
- **Caminho libp2p (internet):** swarm libp2p sobre TCP (porta TCP = UDP + 10000)
  com gossipsub para trocar `Announce`, Kademlia + mDNS para descoberta, relay
  para atravessar NAT e DCuTR para hole punching. Endereços também são
  publicados/resolvidos no DHT pkarr.
- Os dois caminhos desaguam no mesmo lugar: a tabela `peers` de `Network`
  (`HashMap<node_id, PeerInfo>`). O libp2p entra por `Network::insert_peer`.

## 2. Protocolo UDP

### Mensagens (`P2PMessage`, `network.rs`)

Tagged enum JSON (`serde(tag = "type")`) sobre UDP:

- **`Announce`** - auto-apresentação: `node_id`, `pubkey`, `authority_pubkey`,
  `libp2p_peer_id`, `addr`, `addr_candidates`,
  `model`, `start_layer`, `end_layer`, `total_layers`, `memory_mb`, `timestamp`,
  `ollama_port`, `inference_port`, `inference_frame_capabilities`, `gateway_port`,
  `profile`, `hostname`, `capabilities` (catálogo completo de `ModelCapability`),
  `disk_total_mb`, `disk_free_mb`, e `tp` (a `TpIdentity` de tensor parallelism:
  `{role, group_id, size, rank, rank0_url, members}`, `None` = nó sem TP - ver
  `network.rs`). `rank0_url` é a URL do primary onde os ranks fazem o
  `POST /v1/tp/reduce` da coletiva. Cada `members` traz URL, `node_id`, `PeerId`
  libp2p e a capacidade do túnel. `idle` informa que este peer está vivo e com
  o modelo descarregado; não significa que o deployment saiu do ar. Campos novos têm `serde(default)` para um nó
  antigo continuar parseável em cluster de
  versões misturadas.
- **`PeerRequest`** - pede a lista de peers de um nó conhecido.
- **`PeerList`** - resposta com `Vec<PeerInfo>`, fatiada por MTU (ver abaixo).
- **`Heartbeat`** - liveness; atualiza `last_seen`. Definido e tratado no recebimento,
  mas nenhum caminho do código emite hoje (o `Announce` de 5 s cumpre esse papel).
- **`LocalDiscover`** - descoberta estilo mDNS na LAN. Igualmente só tem o lado
  de recebimento (responde com `PeerRequest`).

`PeerInfo` espelha os campos do `Announce` mais `last_seen`,
`connection_verified` e `api_keys_active`. `PeerInfo::is_alive()` compara
`last_seen` com `PEER_TIMEOUT_SECS` (30 s).

### Ciclo de vida

1. **Bind:** `Network::new` monta um socket dual-stack `[::]:<porta>` à mão com
   `socket2`, desligando `IPV6_V6ONLY` **antes** do `bind` (depois não tem efeito;
   sem IPv6 no SO cai para `0.0.0.0`). Porta padrão `DISCOVERY_PORT` = 42777.
   Se a configuração pedir porta `0`, o SO escolhe uma porta efêmera e o nó
   conserva essa porta real no anúncio; o TCP libp2p também deriva dela.
   Os testes usam `Network::testkit`, que liga somente em `127.0.0.1:0` e
   injeta um destino loopback para o anúncio local; `testkit_with_broadcast_addr`
   aceita um endpoint loopback explícito e recusa o broadcast global.
2. **Bootstrap:** `Network::start` chama `bootstrap_discovery` logo no início
   (envia `PeerRequest` a cada bootstrap configurado). `main.rs` então espera
   ~2 s juntando `Announce` antes de calcular a faixa de camadas.
3. **Gossip (5 s):** a cada `GOSSIP_INTERVAL_SECS` o nó re-anuncia
   incondicionalmente (bootstrap + peers vivos + broadcast local), roda
   `discovery_sweep` e expira mortos.
4. **Varredura de descoberta:** `discovery_sweep` re-pergunta os peers a cada
   6 tiques (30 s), com rodízio de `SWEEP_FANOUT` = 3 por varredura começando em
   `k * 3` sobre a lista ordenada por `node_id`. É o que aprende peers
   transitivos fora da LAN; sem ele a descoberta seria um retrato do arranque.
5. **Timeout (30 s):** `cleanup_dead_peers` remove peers com `last_seen` a mais
   de `PEER_TIMEOUT_SECS`.

### Tratamento de `Announce` no recebimento

- Um `addr` começando em `0.0.0.0` é reescrito com o endereço de origem do
  pacote (o "sentinel" de quem anuncia sem endereço). O endereço efetivo de
  origem é sempre **acrescentado** a `addr_candidates` via `merge_observed`,
  mesmo quando o anúncio trouxe outro `addr`: ele é o caminho comprovadamente
  alcançável neste instante, enquanto o endereço anunciado continua sendo
  apenas cache.
- `PeerRequest` responde com um `Announce` imediato (mesmo sem assignment) e com
  o `PeerList` de peers vivos, fatiado para caber em `MAX_DATAGRAM_PAYLOAD`
  (1200 B, o piso que o IPv6 garante; `PeerList` cresce ~530 B por peer).
- Um `PeerList` é também uma atualização de estado: se o `last_seen` do
  snapshot for mais novo, o registro existente é substituído para propagar
  mudanças de modelo e faixa entre peers transitivos. Snapshots mais antigos
  não revertem um anúncio recente; a verificação local é preservada no refresh,
  e os candidatos de endereço locais são herdados **menos os que este nó já
  provou mortos** (`RouteTable::host_is_discredited`). Herdar a lista inteira
  tornava candidato morto imortal - ver "Endereço que deixou de responder".
- Nó destacado (`is_detached`, `--no-discovery`) não aprende nem responde nada;
  o socket continua bound, e `rejoin` refaz bootstrap + announce.

### Endereço que deixou de responder

**Endereços IPv6 temporários rodam sozinhos.** As privacy extensions (RFC 4941)
geram um endereço novo de tempos em tempos e aposentam o anterior; o macOS ainda
mantém o antigo marcado `deprecated` por um tempo antes de removê-lo. O cluster
tem de aguentar isso sem configuração: o endereço que um par anunciou ontem pode
não existir em máquina nenhuma hoje. Fixar `--advertise-addr` **não** é a
resposta - é um remendo por máquina para um defeito que é de todas.

Quem anuncia já lida com a rotação: `self_announce` enumera as interfaces uma
vez por anúncio e `filter_candidates` descarta `deprecated` e põe estável antes
de temporário. Essa enumeração é uma leitura local, sem sonda de rede; acontece
no gossip a cada 5 s e também em anúncios imediatos. Quem **guarda** é que não
lidava:

- `PeerInfo::gateway_url()` era só `peer.addr`, um endereço e nenhum plano B,
  enquanto `inference_url()` já preferia `addr_candidates`. Todo o plano de
  controle (Painel, `deploy`, transferência, `devupdate`) discava esse endereço
  único; morto ele, o Painel mostrava "não respondeu a `/api/ui/node`" para
  sempre, com o par vivo noutro endereço já dentro de `addr_candidates`.
- A sonda do `RouteTable` mede `inference_candidates()` - outra porta, outro
  espaço de URL. Ela nunca ia apanhar um gateway morto, e o Painel nunca
  perguntava à memória de falha.
- O `PeerList` herdava a lista de candidatos local inteira por cima do snapshot
  novo, então o cadáver voltava a cada volta de gossip e o conjunto só crescia.
- `invalidate` apagava as falhas por `node_id`. A rotação de IPv6 - o evento que
  **mata** o endereço - era também o que apagava a prova de que ele morreu.

Hoje:

- `gateway_candidates()` devolve uma URL por endereço conhecido, com `addr`
  primeiro (fato conhecido: é de onde os pacotes chegaram) e os candidatos
  anunciados depois; `gateway_url()` é só o primeiro dela. Mesma disciplina de
  `pipeline::resolve_step_url`. O plano de controle de promoção usa a lista
  inteira nas duas direções: o primeiro destino que falhar no transporte cede
  lugar ao próximo, enquanto uma resposta HTTP permanece uma recusa do peer.
  O anúncio futuro deve atualizar essa lista; o endereço guardado não é uma
  âncora permanente.
- O caminho de inferência (`inference_candidates`) e a descoberta UDP
  (`dialable_addrs`) também preservam todos os endereços válidos. O transporte
  ordena os candidatos pela memória de falha, tenta o próximo quando o anterior
  falha e deixa a falha expirar; DNS dinâmico é resolvido de novo em cada
  tentativa, então uma mudança de hostname não exige reinício.
- A leitura do cluster (`/api/ui/cluster`) percorre esses candidatos e alimenta
  a **mesma** memória de falha do caminho de inferência: `mark_failure` em erro
  de transporte ou timeout, `mark_success` em resposta boa. Resposta inválida ou
  HTTP de erro **não** desacreditam o endereço - quem responde 401 ou 500 está
  vivo ali, e esconder um defeito de payload atrás de uma busca de rota seria
  trocar um defeito por outro.
- `RouteTable::usable` devolve os candidatos ainda não desacreditados. Vazio é
  informação: significa "todo endereço conhecido deste par falhou".
- Aí entra `Network::request_peer_addresses`: manda `PeerRequest` aos bootstrap
  nodes e aos outros peers vivos (quem responde manda `Announce` + `PeerList`) e
  **poda os candidatos desacreditados da entrada local**, para que o `Announce`
  seguinte a repovoe em vez de somar-se ao cadáver. Limitado a uma vez por
  `REDISCOVERY_INTERVAL` (30 s) por peer, porque o Painel relê o cluster em
  ciclo curto. Sem broadcast na LAN: o par continua vivo e o `Announce` dele
  chega a cada 5 s de qualquer modo, e transmitir da suíte de rotas quebraria a
  regra de nunca falar UDP de verdade nos testes.
- Enquanto isso o `/api/ui/cluster` devolve `rediscovering: true` no par, e a
  tela diz "reencontrando" em vez de "nada abaixo pode ser lido". A distinção é
  de produto: um é trabalho em curso que se resolve sozinho, o outro é beco sem
  saída. **A promessa tem prazo**: passado `REDISCOVERY_GIVE_UP` (90 s, três
  perguntas) sem leitura boa, o par volta a `unreachable` dizendo que nenhum dos
  endereços conhecidos respondeu e que a rede não devolveu um novo. Um par que
  anuncia por UDP e não atende no HTTP existe, e chamá-lo de "reencontrando"
  para sempre seria repor o aviso desonesto com outra palavra. Uma leitura bem
  sucedida (`Network::note_peer_read`) encerra a busca, para que a próxima
  rotação comece do zero.
- A cobertura sai do gossip, não da leitura, então um par ilegível continua
  contribuindo camadas - e é honesto que continue, porque o servidor de
  inferência dele pode estar perfeito e só o endereço de controle é que morreu.
  O que não pode é ficar invisível: `model_inventory[].unreadable_nodes` nomeia
  esses pares, e a tela tira o verde de "completo" enquanto a lista não estiver
  vazia.
- `cleanup_dead_peers` chama `RouteTable::forget` no peer que expirou, para que
  um nó que volta com o mesmo endereço não nasça desacreditado.

Provado por `network::tests::gossip_does_not_resurrect_an_address_already_proved_dead`,
`the_gateway_has_one_url_per_known_address_with_the_proven_one_first`,
`an_address_rotation_does_not_erase_the_proof_that_the_old_one_died` e pelo
módulo `webui::dead_address_tests`, que sobe um `/api/ui/node` de verdade e faz
o Painel recuperar o par pelo endereço vivo numa única leitura - incluindo
`rediscovering_gives_up_and_says_so_instead_of_promising_forever`, que trava o
prazo.

### `normalize` dual-stack

O socket é `AF_INET6`, e o macOS recusa destino `sockaddr_in` com EINVAL. Todo
envio passa por `Network::normalize`, que mapeia IPv4 para `::ffff:a.b.c.d`.
Sem isso a descoberta morre num sentido só (o Linux aceita o destino, por isso
a quebra parecia unilateral). Há teste que fala IPv4 de verdade
(`ipv4_destinations_are_mapped_on_a_dual_stack_socket`).

### Endereços anunciados (`addrs.rs` + `network.rs`)

- `self_addr_candidates` enumera as interfaces (`ifconfig` no macOS, `ip -o addr`
  no Linux; Windows fica vazio e cai no endereço observado) e filtra com
  `filter_candidates`: descarta loopback/link-local/deprecated e interfaces
  virtuais de container (`docker0`, `br-*`, `veth*`, `cni*`, `podman*` e
  equivalentes), mantém endereço estável antes de temporário e limita a 2 por
  família. IPs privados em interfaces reais continuam válidos para clusters na
  LAN. O endereço fixado pelo operador (`--advertise-addr`) vem primeiro e não
  conta para o teto; sem fixação, os endereços públicos atuais vêm antes do
  observado por `identify` e dos demais endereços locais.
- Porta `0` nunca é anunciada nem usada como destino: pins explícitos,
  endereços observados e candidatos de peers com essa porta são descartados.
  Um peer sem nenhum candidato discável não entra na tabela de destinos.
- `self_announce` monta `Announce.addr` e `addr_candidates` a partir da mesma
  fotografia das interfaces: o fixado, o primeiro endereço público local com
  a porta de descoberta, o observado por `identify`, ou o literal
  `0.0.0.0:<porta>`. A fotografia é renovada em cada anúncio, sem sondagem de
  rede cara; no gossip, a frequência efetiva é 5 s.
- O literal curinga continua podendo aparecer no `Announce` como sentinel de
  escuta quando o nó ainda não tem endereço anunciável. Antes de qualquer uso
  como destino, `network::is_valid_destination` exige porta diferente de zero e
  rejeita endereços não especificados, multicast e broadcast. Assim o sentinel
  não vira destino UDP, URL HTTP de inferência ou multiaddr do túnel; candidatos
  válidos seguintes continuam disponíveis.
- `advertised_addr` normaliza o que o operador digita à mão em `host:port`
  (IPv6 puro precisa de colchetes para ganhar porta).
- `self_announce` monta o `Announce`; anuncia zero camadas quando não há
  `LayerAssignment`, para um nó configurado pela UI continuar visível.

**Precedência do endereço anunciado** (`announce_addr()`/`self_addr_candidates()`,
a mesma para UDP e gossipsub - os dois leem do mesmo `Network`):

1. endereço fixado pelo operador - `--advertise-addr` ou `--public-addr`
   alimentam o mesmo campo, `--advertise-addr` vence se os dois vierem;
2. endereço público atual das interfaces locais, com endereço estável antes do
   IPv6 temporário;
3. endereço público observado por um peer via libp2p `identify`
   (`Network::set_observed_addr`, chamado de `discovery.rs` no evento
   `identify::Event::Received` - mesmo princípio de um STUN, sobre as conexões
   que o swarm já faz);
4. demais interfaces locais desta máquina;
5. o sentinela `0.0.0.0:<porta>`, só quando nada acima existe.

`addrs::is_private_or_local` decide se um endereço observado é novidade: RFC 1918,
**`100.64/10` (RFC 6598, o CGNAT - e o que a Tailscale usa)** e IPv6 unique-local
não contam, porque só provam que quem respondeu está na mesma rede privada.

Conferido ao vivo em 2026-08-16, sem reiniciar nada: fixar o endereço pela tela
de Rede (`POST /api/ui/network/advertise-addr`) chega aos outros nós em ~10 s;
limpar o campo volta para o endereço público atual da interface quando existe,
ou para o observado pelo `identify` quando o nó está atrás de NAT; e o endereço
da Tailscale (`100.122.132.97`), antes adotado como público, deixou de ser aceito.

### Rotas dinâmicas (`route.rs`)

Um nó pode ter vários endereços válidos e qual serve depende de quem pergunta.
`RouteTable` guarda a medição por candidato: sonda `GET /health` na **porta de
inferência** que o pipeline vai usar, com timeout 3 s e re-medida a cada 5 min.
Sondar o gateway seria insuficiente - ele pode responder enquanto a porta de
inferência engole SYN. Quando o peer anuncia endereço, candidatos ou portas
diferentes, a escolha anterior é invalidada na hora para não reutilizar uma URL
antiga durante a janela de 5 minutos. O cliente da sonda aceita o certificado
autoassinado e o hostname do peer; os clientes de inferência continuam com
pinning. A task
que mede reaplica ao cliente a política TLS atual antes da sonda. `choose_route` só troca de rota
atual se o ganho passar de 25% relativo **e** 2 ms absolutos (9,6 vs 7,2 ms
MacBook↔ksia não pode trocar a cada ciclo). `resolve` devolve `None` =
"ainda não medido ou vencido", e o chamador cai no `inference_url` do plano.

A **memória de falha** é a segunda metade da tabela, chaveada por
`(node_id, URL)`: `mark_failure` grava quando uma requisição real falhou naquele
endereço e tira a rota na hora, `mark_success` apaga, `is_failed` responde, e
`usable` filtra uma lista de candidatos. Como a chave é a URL inteira, a porta de
inferência e a porta do gateway do mesmo host convivem sem se confundir - e é de
propósito que as duas moram aqui: o plano de controle sofria do mesmo defeito que
o caminho de inferência sofria, e uma segunda tabela seria a mesma decisão para
tomar em dois lugares. Uma sonda lenta **não** escreve nessa memória: o timeout
dela é bem mais apertado que o de uma requisição real, e desacreditar todos os
candidatos deixaria `resolve_step_url` sem rota nenhuma. `invalidate` descarta a
escolha e **preserva** as falhas; só `forget`, quando o peer expira, as apaga.

Essa tabela de endereçamento não é a tabela de RTT do mapa. `RttTable` mede
`GET /health` pelo `TunnelRequest` libp2p/Noise, o mesmo transporte usado pelos
frames de inferência, e mantém a direção local `origem -> destino`. O peer
publica somente medições frescas em `GET /api/ui/network/rtt`, autenticado pela
chave de conexão. `snapshot_with_age` preserva a idade para diagnóstico e
`fresh_snapshot` é a fonte da seleção. O gateway agrega os relatórios e só usa
a medição de `A -> B` na aresta `A -> B`. Se o peer não oferece o túnel, o
relatório pode marcar `direct-health` como fallback. Nenhuma latência de outro
peer é transitiva ou reaproveitada como se fosse a aresta real.

O anúncio P2P continua sendo capacidade de dados: `capabilities` informa
modelo/faixa e `runtime_capacity` informa `n_ctx` e `max_new_tokens` apenas
depois da confirmação de `/health`. `profile` identifica o perfil do nó e
`authority_pubkey` identifica a chave de assinatura da cadeia; nenhum dos dois
concede voto ou poder de controle ao infer. Um infer pode ser descoberto e
escolhido para uma cabeça, mas não ganha voz nas decisões do plano de controle.
Para o Chat, o endpoint
`/api/ui/infer/heads` expõe somente as candidatas L0 do modelo pedido, não a
rede inteira. A cabeça é escolhida por cobertura e, entre faixas igualmente
úteis, por um score ponderado de RTT fresco e carga em voo, com empate
determinístico por `node_id`; sem candidata o estado é visível e a inferência
não começa.

## 3. Caminho libp2p (internet)

`Discovery` (`discovery.rs`) constrói o swarm (`DiscBehaviour`) com gossipsub
(tópico `pom/announce/1.0.0`, heartbeat 1 s, autenticação assinada e validação
estrita),
Kademlia em modo `Server`, mDNS, identify, relay (cliente **e** servidor),
DCuTR, AutoNAT e o behaviour de bitswap. Escuta em `/ip4/0.0.0.0/tcp/<udp+10000>`
e em `/ip6/::`.

- **Bootstrap libp2p:** os mesmos nós bootstrap do UDP, convertidos por
  `udp_to_libp2p_multiaddr` (IPv4/IPv6/hostname na porta TCP). A porta TCP é
  `udp+10000` usando a porta UDP efetivamente ligada; porta UDP zero é recusada.
  Um bootstrap IPv6 colado em `/ip4/` falhava o parse e era dialado sem palavra
  no log.
- **Relays:** por padrão diala os relays públicos do IPFS
  (`DEFAULT_RELAY_NODES`) e os de `--relay-peers` (resolvidos antes por pkarr,
  filtrando `is_relay`); `--no-default-relays` tira os públicos, inclusive no
  caminho gateway-only sem modelo. Com `--serve-relay` o servidor de relay
  aceita reservas (128 slots, 4 por peer,
  1 h de reserva, circuitos de 120 s / 128 kB); sem ele, `max_reservations: 0`.
- **Gossipsub:** `main.rs` publica o `Announce` a cada 5 s, com ou sem
  assignment, e o quadro é o mesmo do UDP (`Network::gossip_announce_bytes` ->
  `self_announce`); `handle_event` converte a mensagem em `PeerInfo` preferindo
  `addr_candidates.first()` ao `addr` (o literal `0.0.0.0` só sobrevive se nada
  foi anunciado), registra os candidatos convertidos para a porta TCP do
  libp2p com `/p2p/<peer-id>` no swarm e só então aquece a conexão do túnel;
  esse registro acontece junto com a chamada a `Network::insert_peer`. Sem
  ele, um peer aprendido somente pelo gossip tem `PeerId`, mas nenhum endereço
  para `swarm.dial(peer_id)`.

### Ociosidade e `reload_model`

O campo `idle` acompanha cada `Announce` e `PeerInfo`, mas não participa da
expiração de liveness: um peer ocioso continua vivo, alcançável e parte do
plano. Ao receber uma requisição para um plano que contém peers ociosos, o
gateway recarrega o peer local e dispara `POST /api/internal/reload_model` em
paralelo aos outros peers ociosos. Essa chamada usa `connection_key` e o
marcador interno, e só ordena a carga local de cada destinatário. Não há
conexão síncrona adicional no hop normal de inferência nem timer para decidir a
ociosidade; a decisão usa a atividade local registrada e o monitor existente.
  Requisições recebidas pelo túnel são encaminhadas ao servidor de inferência
  local em uma task separada. O loop do swarm continua drenando GossipSub,
  respostas TP e eventos de conexão enquanto um prefill, decode ou redução TP
  está em andamento.
  O id de mensagem é `discovery::announce_message_id` (hash do tópico +
  payload). Antes de inserir o peer, `handle_event` também exige que
  `Announce.libp2p_peer_id` corresponda ao `source` autenticado do transporte.
  Essa ligação prova quem transportou o anúncio, mas não é raiz da autoridade
  administrativa: concessões e comandos continuam exigindo a chave Ed25519 e a
  cadeia local.
- **mDNS:** descoberta na LAN que alimenta o endereço da Kademlia e diala o peer.
- **Identify:** grava `listen_addrs` do remoto na Kademlia e, para relays
  conectados, escuta no circuito (`P2pCircuit`) do relay.
- **DCuTR:** hole punch; sucesso loga conexão direta, falha avisa que o fallback
  do relay está ativo. **AutoNAT:** só loga a mudança de status do NAT.
- **Bitswap:** protocolo request/response (`/pom/bitswap/1.0.0`) no swarm,
  mas é código órfão (ver Defeitos).

### pkarr (`pkarr_node.rs`)

Cada nó publica uma TXT `_pom` assinada no DHT pkarr: JSON compacto com
campos de uma letra (`a` endereço, `i` porta de inferência, `m` modelo, `s`/`e`
faixa de camadas, `t` total, `r` é relay) para caber no limite DNS de ~1000 B.
Publicação inicial e republish a cada 30 min (TTL 300 s), apenas quando há
assignment. `resolve_peers` resolve chaves `pk:` em `ResolvedPeer`; qualquer
falha vira `None`, nunca erro. `--public-addr` alimenta apenas o multiaddr do
pkarr; `--pkarr-peers` são chaves a resolver e dialar no arranque;
`detect_local_ip` descobre o IP de saída com um socket UDP conectado a
`8.8.8.8:80` sem enviar nada.

## 4. Identidade

`Identity` (`identity.rs`) é um par Ed25519 de longa duração, vivendo em
`<project_root>/.pom-node.key` (ou `$POM_NODE_KEY`). O `node_id` é o
hex de `SHA256(pubkey)` (`NodeId::from_pubkey`), impresso com os primeiros 12
caracteres. A chave tem que sobreviver a reinícios: regenerar a cada boot
duplicava o peer na lista até o timeout e voidava `blocked_nodes`, que é
chaveado por node_id. Um arquivo corrompido gera identidade nova em vez de
parar. A chave do pkarr é separada: `.pom.key` (ver Defeitos).

## 5. Limitações conhecidas para a internet

Comportamento atual do código:

1. **Endereço no gossipsub pode ser `0.0.0.0:<porta real>`.** O `Announce` do
   gossipsub leva `addr = announce_addr()` (literal `0.0.0.0` sem
   `--advertise-addr`). Hoje ele também carrega `addr_candidates`, e o
   `handle_event` do libp2p usa o primeiro candidato, então o `0.0.0.0` só
   sobrevive quando o peer não anunciou candidato nenhum (ex.: Windows, onde a
   enumeração é vazia). O caminho UDP reescreve `addr` com o IP de origem.
   `insert_peer` (libp2p) não reescreve, mas a guarda de destino impede que esse
   sentinel seja discado ou convertido em URL/multiaddr. Um candidato válido
   observado ou anunciado continua sendo usado.
2. **O tráfego de inferência prefere libp2p por hop.** O gateway e os relays
   usam `network/infer_tunnel.rs` via `relay::send_frame_or_tunnel` quando o
   hop anuncia `transport/libp2p-tunnel-v1` e um `PeerId` válido. O túnel é
   protegido por Noise e leva a credencial da inferência até o loopback do
   destino. Se ele estiver indisponível ou falhar por transporte, o mesmo
   quadro segue pelo cliente direto pinado, usando HTTPS quando TLS está ativo.
   Antes de cada envio, o swarm reidrata as multiaddresses TCP a partir dos
   candidatos UDP levados pelo plano quando disponíveis, ou do anúncio vivo
   como fallback, quando ainda não há conexão. Isso cobre a janela em que o
   gateway já conhece o `PeerId`, mas o evento gossipsub ainda não registrou um
   endereço no livro de endereços do libp2p.
   Uma resposta HTTP explícita do peer não é repetida em outro transporte.
   No TP, `broadcast_to` e `POST /v1/tp/reduce` seguem a mesma regra e o
   `TunnelRequest` preserva os cabeçalhos do decode, da barreira, do rank e da
   autorização. O loopback só injeta `application/octet-stream` quando a
   chamada não informou outro tipo, preservando `application/json` nas rotas de
   ciclo de vida da sessão.
3. **A porta de inferência exige credencial.** `/v1/prefill`, `/v1/step`,
   `/v1/step-stream`, `/v1/session/release`, `/v1/session/rollback` e
   `/v1/tp/reduce` aceitam a
   `connection_key` compartilhada ou uma API key ativa no nó receptor. A
   `connection_key` é a credencial usada nas chamadas internas; API keys são
   aceitas para manter o mesmo contrato de autenticação disponível nas APIs
   externas. `/health` permanece público para readiness.

## 5. TLS entre peers

Ligado por um interruptor em Configurações / Preferências que **só o nó principal**
opera, e vale para o cluster inteiro. Desligado por padrão, e um `pom.json`
anterior a isto continua válido (`Config.tls_required`, `#[serde(default)]`).

**O certificado é derivado da identidade, não emitido por ninguém**
(`network/tls.rs`). `node_id` já é `hex(SHA256(pubkey))` da chave Ed25519 de
longa duração, e o cluster já distribui esse hash pelo gossip - então o
certificado só precisa **provar posse da chave que dá naquele node_id**, e é
isso que o handshake TLS faz. O `rcgen` monta um auto assinado com a própria
`.pom-node.key`, em memória; nada vai para o disco, não há CA, não há o que
renovar, revogar ou distribuir.

**A verificação é pinning** (`PinnedNodeId`): o cliente extrai a pubkey do SPKI
do certificado apresentado, calcula SHA256 e compara com o node_id que ele já
aprendeu para aquele peer. Validade, cadeia e nome são **ignorados de
propósito** - a confiança não vem deles. Checar validade só produziria falha por
relógio torto, e checar nome só produziria falha por discar um IP em vez do CN;
nenhuma das duas diria nada sobre com quem estamos falando. Sem o pinning o
auto assinado cifraria sem autenticar, e quem estivesse no meio apresentaria o
próprio auto assinado e seria aceito.

O teste `fingerprint_do_certificado_e_o_mesmo_de_from_pubkey` prende as duas
contas juntas: se um dia divergirem, o pinning recusa todo mundo, e é ele que
avisa.

**Uma porta, duas falas** (`backend/serve.rs`). O servidor espia o primeiro byte
sem consumi-lo (`TcpStream::peek`): `0x16` é handshake TLS, qualquer outra coisa
é HTTP em claro; as duas caem no mesmo router. Isso é o que permite a virada
**não** ser simultânea - enquanto uns peers já falam TLS e outros ainda não, o
servidor atende os dois. Com a política ligada, texto claro vindo de fora do
loopback é recusado logo após o peek.

**Loopback fala claro sempre**, e é requisito: `infer_tunnel::serve_loopback`, a
sonda de `runtime.rs` e o `gateway::testkit` batem em `127.0.0.1` e não têm par
de chaves a apresentar. Tráfego que não sai da máquina não tem o que proteger.

**O cliente passa a ser por destino** (`network/peer_client.rs`), porque o
verifier carrega qual node_id esperar e é fixado na construção. Sem TLS volta a
ser um só, que é o comportamento anterior. O esquema das URLs (`http`/`https`)
sai de um estado global do processo, `peer_client::tls_ativo`, porque é
propriedade do nó e não da conexão.

**Isso vale para o plano de controle também, não só para a inferência.** A
primeira versão converteu o caminho de inferência e o de transferência e deixou
de fora `/api/ui/cluster/models`, o levantamento de `deploy::survey`, o
encaminhamento de `plan`/`leave`/`apply`/`unload`/`stop-inference` e a sondagem
de jobs. Com a política ligada, **toda** consulta a peer falhava calada: a tela
de implantar dizia "nenhuma máquina do cluster tem fatias de <modelo>" e não
havia como implantar nada. Para o defeito não voltar, o `GatewayState` **não tem
mais** um cliente compartilhado sem pinning - existe só o `peer_http`, e quem
fala com destino que não é peer (loopback, Ollama) pede `peer_http.plain()`, que
diz o que está fazendo. `webui::catalogo_sob_tls_tests` prende as duas rotas que
o operador usa para implantar.

**O que fica de fora, e por quê:**

- **Gossip UDP.** TLS não se aplica a datagrama, e o conteúdo é endereço, faixa
  de camadas e capabilities - nenhum dado de usuário, nenhum peso, nenhuma
  credencial.
- **libp2p.** Já é cifrado ponta a ponta por Noise; sobrepor TLS não
  acrescentaria segurança. Um salto pelo túnel reporta `tls: false` na
  telemetria de propósito: ele é cifrado, mas não por TLS, e dizer `true` ali
  alegaria uma garantia que aquele salto não deu.
- **Nada, do lado do navegador** - ver "Dois certificados na mesma porta"
  abaixo. A primeira versão deixou a UI de fora, e o resultado foi ligar o TLS e
  trancar o operador para fora do painel.
- **Ollama.** `PeerInfo::ollama_url` continua em `http://`: é um servidor de
  terceiros na máquina do peer, sem o nosso certificado e sem a nossa
  identidade.

### Dois certificados na mesma porta

O certificado do pinning é **Ed25519**, porque a chave dele é a identidade do
nó - e é exatamente isso que navegador não sabe verificar. Medido com o
curl/LibreSSL do macOS: `sslv3 alert handshake failure`. Somado ao modo estrito,
ligar o TLS deixava a interface inalcançável de qualquer outra máquina: em claro
a conexão era derrubada, em TLS o handshake não fechava.

Então o nó apresenta **dois** certificados auto assinados na mesma porta,
escolhidos por conexão (`tls::NodeCertResolver`):

- **Peer** - o Ed25519 derivado da `.pom-node.key`, que sustenta o pinning.
- **Navegador** - um ECDSA P-256, que ele sabe verificar, com os endereços do nó
  no SAN (`serve::browser_sans`: locais, o fixado pelo operador, o hostname e o
  loopback). Os SANs existem para o aviso ser "emissor desconhecido" e não "host
  errado" - o segundo faz o operador desconfiar da máquina em vez do
  certificado.

**O discriminador é o ALPN**, e a escolha dele não é estética: SNI não serve
(peer disca IP, e IP não leva SNI) e a lista de algoritmos do cliente varia
entre navegadores. O ALPN está sob nosso controle - o cliente de peer oferece
`pom/1` (`tls::PEER_ALPN`), navegador nenhum oferece. Se um dia essa escolha
errar para um peer, ele recebe o ECDSA e o verifier, que só aceita Ed25519,
recusa: falha fechada.

Sendo auto assinado, o navegador avisa na primeira visita de cada máquina. Isso
é inerente a não ter uma CA; distribuir uma CA própria e instalá-la em cada
máquina é outro projeto.

### Texto claro: redireciona, menos onde não deve

Com a política ligada, a porta do gateway responde **301 para `https://` na
mesma porta** a quem chegar em claro, em vez de fechar a conexão. Caminho e
query são preservados, e o host sai do cabeçalho `Host` - reconstruí-lo do
socket daria o IP e trocaria o nome que o operador digitou por um endereço que
pode nem estar no certificado.

Duas exceções, ambas por um motivo concreto:

- **Rotas exclusivamente de peer** (`/api/transfer/*`, `/v1/relay-result`,
  `/v1/remote-stream-result`, `/v1/prefill`, `/v1/step`, `/v1/step-stream`)
  respondem **403**, não 301. O `reqwest` segue
  redirecionamento por padrão: um 301 aqui faria um peer antigo tentar TLS sem
  saber verificar nada e falhar com um erro que não diz o motivo.
- **A porta de inferência (`serve::PlainPolicy::Refuse`)** fecha a conexão, como
  antes. Não há navegador ali para acomodar.

### A catraca: o gossip nunca é fonte de verdade

O Announce ganhou um campo `tls`, e ele é **gatilho, nunca valor**. O UDP é em
claro e não autenticado: se a política viesse dele, qualquer um na rede
anunciaria `tls: false` e rebaixaria o cluster inteiro para texto claro -
exatamente o ataque que o TLS existe para impedir.

O caminho é `network/tls_policy.rs`:

1. Um Announce **do principal** discorda da política local.
2. Isso dispara `GET /api/cluster/tls` no principal, autenticado com a chave de
   conexão.
3. Só a resposta dessa busca é gravada em `pom.json`.

Um intruso pode disparar a busca à vontade; não pode responder por ela. O
booleano anunciado é usado para **uma coisa só**: saber por qual esquema discar
a sonda. Sem isso, um nó que ainda acha o cluster em claro discaria `http://`
num principal estrito, seria recusado, e a catraca nunca conseguiria *ligar*.
Forjar a dica não muda o que é gravado - só faz a sonda bater na porta errada e
falhar.

**Fail-closed:** busca que falha preserva o valor local. Principal fora do ar ou
rede partida nunca rebaixam o cluster.

**Sem reinício, e nos dois sentidos.** Ligar ou desligar esvazia o cache de
clientes, muda o que o anúncio diz e liga/desliga a recusa de texto claro. Os
demais nós adotam dentro de um ciclo de gossip (5 s), e o operador pode virar
quantas vezes quiser.

## Logs de processo do cluster

O log de processo usa um protocolo request/response próprio do swarm,
`/pom/cluster-logs/1.0.0`, separado de `infer_tunnel.rs`. Cada nó envia lotes
somente ao peer que conhece como principal; não há malha, retransmissão ou
encaminhamento de logs entre nós. O principal registra cada entrada na ordem de
chegada, sem ordenar pelo relógio remoto, e guarda `origin_at` apenas como dado
informativo.

`cluster_logs_enabled` no `Announce` é somente gatilho. Quando diverge da
política local, o nó faz uma busca autenticada em `GET /api/cluster/logs` no
principal e só grava o valor retornado; falha na busca preserva o valor local.
O interruptor administrativo aplica a mudança sem reinício.

A prova local de contrapressão roda com
`cargo test -p node --bin pom cluster_logs --no-default-features -- --nocapture`.
Ela enche uma fila de capacidade 1, exige que a segunda emissão retorne
imediatamente como descarte e confere o contador. O microbenchmark incluído na
mesma suíte, em Apple M4, fez 50.000 emissões e 782 lotes. Em cinco execuções,
as medianas foram `305 ns/linha` para `try_push` e `373 ns/linha` para receber e
formar lotes de até 64 entradas. A medida é do caminho de captura e lote, não de
inferência; o escritor SQLite e o transporte ficam fora da thread que emite.

O `tracing` entra numa fila própria limitada a 1024 entradas. A captura usa
snapshot lock-free do contexto e `try_send`, portanto nunca espera uma
atualização de identidade, a inferência ou a rede; fila cheia descarta a entrada
e acumula a contagem. Uma task fora do caminho quente envia até 64 entradas a
cada 100 ms, em modo best-effort, sem retentativa agressiva. O principal grava
as linhas em `pom.db`, associadas ao `deploy_id`, e também persiste o contador
de descartes.

## 6. Chat da rede

O Chat da rede é uma superfície separada do Chat com a IA e usa o mesmo swarm
libp2p, sem XMPP ou servidor externo. O protocolo request/response próprio é
`/pom/network-chat/1.0.0`, com Noise, relay/DCuTR e endereçamento exclusivamente
por `node_id`. Texto, arquivo e indicador de digitação compartilham a sala
`network`; arquivos têm limite de 8 MiB e texto de 8 KiB. O histórico de texto
e os metadados de arquivo ficam em `network_chat_messages` no mesmo `pom.db` do
histórico da IA. O snapshot da UI não repete bytes de arquivo a cada polling; o
download autenticado usa `GET /api/ui/network-chat/file/:message_id`.

O desenho é estrela. Um participante envia cada ação com a API key ativa do POM
no frame original. O nó principal confere a chave no banco, vincula a chave ao
`sender_node_id`, recusa `blocked_nodes`, aplica rate limit e só então
retransmite o frame já validado aos participantes. A retransmissão remove o
segredo e leva `relayed: true`; um nó que não é principal não aceita esse frame
como origem. O arquivo continua dentro do limite de frame do protocolo e não é
armazenado fora do banco local de cada nó.

`network_chat_enabled` no `Announce` é gatilho, nunca valor. O interruptor
`POST /api/ui/network/chat` grava a configuração autoritativa no principal
real, cuja `authority_pubkey` vem do `AdminTrust`, e atualiza a cópia atômica
anunciada. Quando a interface está num admin secundário, a mesma rota
encaminha a mutação ao principal com a chave de conexão. Um nó que observa
divergência consulta `GET /api/cluster/network-chat` com a chave de conexão e
só adota uma resposta autenticada; falha preserva o valor local. O perfil
anunciado não substitui a identidade do principal. Desligado, o canal não
aceita, persiste ou retransmite ações.

A comunicação é deliberadamente de baixa prioridade: o behaviour é separado do
túnel de inferência, o `select!` do swarm prioriza eventos do swarm e o túnel,
as filas usam `try_send`, e a persistência roda em writer dedicado com fila
limitada. Sob pressão, o Chat descarta e incrementa `dropped`; ele nunca espera
SQLite ou segura uma chamada de `/v1/tp/reduce`, prefill ou decode. O teste de
contrapressão e a medição de latência ficam em
`network_chat_tests.rs` e em `discovery.rs`.

## 7. Defeitos anotados

- **DEFEITO ANOTADO: bitswap é protocolo órfão** - `bitswap_behaviour` está no
  swarm e os eventos só são logados em `debug!`
  (`discovery.rs::handle_event`, `DiscBehaviourEvent::Bitswap`) - nenhuma
  requisição é atendida, nenhum bloco é servido. A transferência real vai por
  HTTP Range (`backend/transfer.rs`). O protocolo existe mas ninguém implementa
  o lado servidor.
- **DEFEITO ANOTADO: chave pkarr relativa ao cwd** (`main.rs`,
  `Path::new(".pom.key")`) - diferente de `identity::key_path()`, que
  resolve contra `project_root()`. Rodar o binário de outro diretório
  gera/ignora outra identidade pkarr, inconsistente com a regra documentada do
  project root. Registrado no `AGENTS.md` de `crates/node/`.
