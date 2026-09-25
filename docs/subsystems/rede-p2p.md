# Subsistema: rede P2P e descoberta

O subsistema de rede resolve o problema de um nó pom achar os outros nós
do cluster e se anunciar para eles. São dois mundos no mesmo diretório
(`crates/node/src/network/`): o transporte nativo UDP (`network.rs`, `addrs.rs`,
`identity.rs`, `route.rs`), que é o coração da LAN, e o caminho libp2p/pkarr
(`discovery.rs`, `pkarr_node.rs`, `bitswap.rs`), desenhado para a internet
(Circuit Relay v2, DCuTR, AutoNAT, Kademlia, gossipsub, DHT pkarr). A regra da
casa é que a decisão de endereço pertence a quem vai discar, não a quem anunciou.
Com `advertise_addr` fixado, ele é o único endpoint anunciado e discado pelo
libp2p; sem pin, `addr_candidates` e `RouteTable` mantêm o fallback (ver
`crates/node/src/network/AGENTS.md`).

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

### Admissão antes da tabela `peers`

Descoberta de transporte não é pertencimento ao enxame. O processo pode
aprender um endereço por UDP, mDNS, Kademlia, pkarr ou um link gossipsub, mas
nenhuma dessas fontes registra um `PeerInfo` sem prova local de admissão. A
mesma regra vale para `Announce` UDP, `Announce` gossipsub, `PeerList` e a
entrada direta por `insert_peer`; políticas, túneis, rotas, UI e pipeline só
consomem a tabela depois desse ponto.

- Em rede privada, uma `connection_key` não vazia seleciona a prova por
  `/handshake`. O nó envia sua identidade e a chave ao gateway anunciado e
  considera a resposta `approved` ou `challenge_required` como prova de que o
  destino conhece a chave compartilhada. Chave errada, chave ausente, peer
  sem gateway ou timeout são recusados. A chave não aparece no `Announce`.
  Esgotados os endereços diretos, a mesma sonda vai pelo túnel libp2p; para um
  par atrás de NAT/CGNAT esse é o único caminho que chega, e sem ele a
  admissão nunca concluiria. O veredito fica em cache por 30 s (recusa: 5 s).
- **O `Announce` do gossipsub nunca sonda dentro do laço do swarm.** O pedido
  do túnel é atendido por esse mesmo laço, então uma sonda aguardada ali
  esperava por si mesma até o timeout de 2 s e falhava sempre. O sintoma: um
  par que só o relé alcança entrava enquanto alguma rota direta respondia e
  saía com `Removed timed-out peers` quando o cache vencia, porque os anúncios
  seguintes eram descartados. `discovery.rs` consulta
  `Network::admission_without_probe` (rede pública, chave vazia ou cache);
  sem veredito, `Network::admit_in_background` roda a sonda em outra tarefa,
  uma por peer, e o anúncio atual é descartado - o próximo, em até 5 s,
  encontra o cache. Os anúncios UDP continuam sondando em linha, no laço de
  recepção UDP, que não é o do túnel.
- Em rede pública, a âncora local precisa ser válida, e o rigor depende do
  `network.access_policy`. Em ambos os casos a chave pública legada precisa
  derivar o `node_id`, a chave de autoridade precisa derivar o `libp2p peer_id`,
  e o `Announce` do gossip já vem assinado por esse `peer_id`. A `connection_key`
  local do admin não participa dessa admissão nem é enviada aos peers.
  - `authorized` (padrão): além do vínculo de identidade, a admissão exige a
    tupla exata `{node_id, libp2p peer_id, authority_pubkey}` presente como
    principal atual ou concessão secundária não demovida. O `cluster_id` do
    `Announce` é só um seletor de cadeia e pode ser falsificado; sozinho não
    passa pela catraca. Sem trust local, nenhum peer público é membro.
  - `anyone_with_anchor`: o operador declarou que segurar a âncora
    correspondente já é a prova de pertencimento. A admissão exige uma cadeia
    local válida ancorada exatamente no `cluster_id` anunciado
    (`AdminTrust::anchors_cluster`) mais o vínculo de identidade acima - **sem**
    concessão de autoridade. É o que deixa um nó worker comum, co-locado ou não,
    que só tem a âncora e nunca recebeu concessão, entrar em `peers`. Um
    `Announce` sem `cluster_id`, com `cluster_id` diferente do da âncora, ou com
    identidade inconsistente continua recusado.
- `connection_verified` é uma marca produzida pelo gate local. O valor vindo
  de `PeerList` nunca é aceito como prova e a lista transitiva é revalidada
  antes de inserir um peer desconhecido ou atualizar um peer existente.

Um desconhecido público ainda pode aparecer nos eventos do transporte libp2p,
porque mDNS, Kademlia e gossipsub precisam continuar operacionais para achar
endereços. Ele é descartado em silêncio no limite da aplicação, sem logar
credencial ou responder com detalhes de falha, e não aparece em `/api/ui/cluster`,
nas rotas ou no planejamento de inferência. A sonda privada tem timeout curto e
cacheia o resultado por identidade e endereço por 30 segundos; recusas ficam
cacheadas por 5 segundos para não permitir uma tempestade de anúncios.

Sob `authorized`, o `cluster_id` sozinho não protege contra falsificação: a
proteção vem da comparação da tupla com a cadeia assinada carregada por
`AdminTrust`; anunciar o id correto sem a chave pública e a concessão
correspondentes não passa pela catraca. Sob `anyone_with_anchor` o operador
troca essa exigência pela posse da âncora do `cluster_id`, e o que resta
protegendo contra um estranho é o vínculo de identidade e a assinatura do
`Announce` no gossip. Em ambos os casos vale a decisão de
`admin-control-signature-scope`: quadros de inferência não ganham assinatura
administrativa, mas o controle de admissão usa a cadeia de confiança antes de
qualquer roteamento.

## 2. Protocolo UDP

### Mensagens (`P2PMessage`, `network.rs`)

Tagged enum JSON (`serde(tag = "type")`) sobre UDP:

- **`Announce`** - auto-apresentação: `node_id`, `pubkey`, `authority_pubkey`,
  `cluster_id` (se houver uma cadeia pública ancorada),
  `libp2p_peer_id`, `libp2p_addrs`, `addr`, `addr_candidates`,
  `model`, `start_layer`, `end_layer`, `total_layers`, `memory_mb`, `timestamp`,
  `ollama_port`, `inference_port`, `inference_quic_port`,
  `inference_frame_capabilities`, `gateway_port`, `gateway_quic_port`,
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
- **`LocalAutoDiscovery`** - handshake explícito e temporário do primeiro setup.
  `Probe` e `Offer` carregam apenas desafio, chaves efêmeras e o documento de
  confiança público. `Proof` vincula o pedido às duas pontas; só depois dele o
  admin envia `Secret`, cifrado com X25519 e ChaCha20-Poly1305, contendo a
  `connection_key`. Não há resposta nem material sensível para uma escuta
  passiva.

`libp2p_addrs` é a lista de multiaddresses libp2p que o nó publica, já com
`/p2p/<peer-id>`. É a única forma capaz de expressar um circuito de relay
(`.../p2p/<relé>/p2p-circuit/p2p/<self>`), que a convenção `host:port` com
porta `UDP+10000` não carrega. Vem de `Network::set_libp2p_addrs`, que o
`Discovery` atualiza a cada listener, endereço externo confirmado ou reserva de
circuito; o anúncio novo sai no gossip seguinte, em no máximo 5 s. Vazio num
peer de binário anterior, e aí quem disca volta a derivar do `addr`.

`PeerInfo` espelha os campos do `Announce` mais `last_seen`,
`connection_verified` e `api_keys_active`. `PeerInfo::is_alive()` compara
`last_seen` com `PEER_TIMEOUT_SECS` (30 s).

O `Announce` não é fonte de verdade para os metadados da rede. Depois de
validar que o emissor é o principal ancorado, o nó dispara uma leitura
assinada de `/api/cluster/network-settings` e persiste o `NetworkSettings`
completo para a tela de Informações da rede. Em rede pública a resposta não
leva a `connection_key`; o registro público continua limitado ao convite e à
descoberta.

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
   `discovery_sweep` e expira mortos. Quando há `advertise_addr`, o anúncio
   contém somente esse endpoint, não a enumeração de interfaces locais.
4. **Varredura de descoberta:** `discovery_sweep` re-pergunta os peers a cada
   6 tiques (30 s), com rodízio de `SWEEP_FANOUT` = 3 por varredura começando em
   `k * 3` sobre a lista ordenada por `node_id`. É o que aprende peers
   transitivos fora da LAN; sem ele a descoberta seria um retrato do arranque.
5. **Timeout (30 s):** `cleanup_dead_peers` remove peers com `last_seen` a mais
   de `PEER_TIMEOUT_SECS`.

O serviço `LocalDiscoveryRuntime` não substitui o gossip normal. Durante o
primeiro setup ele recebe `Probe` na porta UDP local e fica ativo somente até a
configuração terminar. Depois do setup a configuração `local_auto_discovery`
começa `false`; o administrador pode ligá-la manualmente em Preferências, e o
processo mantém o trust document e a chave somente na memória enquanto estiver
respondendo. O `Offer` expõe apenas o fingerprint e os metadados de tamanho do
documento. Depois do `Proof`, o payload com trust document e `connection_key` é
cifrado com ChaCha20-Poly1305 e enviado em fragmentos de no máximo 640 bytes,
para que cada envelope UDP fique dentro do orçamento de 1200 bytes. O nó só
remonta e valida o fingerprint depois de receber todos os fragmentos; o material
aceito é limitado a 512 KiB. Desligar limpa as sessões do handshake e o material
sensível.

### Tratamento de `Announce` no recebimento

- Um `addr` começando em `0.0.0.0` é reescrito com o endereço de origem do
  pacote (o "sentinel" de quem anuncia sem endereço). O endereço efetivo de
  origem é acrescentado a `addr_candidates` via `merge_observed` somente quando
  o anúncio não trouxe uma lista explícita. Com `advertise_addr`, a lista
  anunciada é a autoridade e não ganha LAN, bridge ou endereço observado como
  candidato adicional.
- `PeerRequest` responde com um `Announce` imediato (mesmo sem assignment) e com
  o `PeerList` de peers vivos, fatiado para caber em `MAX_DATAGRAM_PAYLOAD`
  (1200 B, o piso que o IPv6 garante; `PeerList` cresce ~530 B por peer).
- Um `PeerList` é também uma atualização de estado: se o `last_seen` do
  snapshot for mais novo, o registro existente é substituído para propagar
  mudanças de modelo e faixa entre peers transitivos. Snapshots mais antigos
  não revertem um anúncio recente; a verificação local é preservada no refresh,
  e os candidatos de endereço locais são herdados **menos os que este nó já
  provou mortos** (`RouteTable::endpoint_is_discredited`). Herdar a lista inteira
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

Sem pin, quem anuncia já lida com a rotação: `self_announce` enumera as
interfaces uma vez por anúncio e `filter_candidates` descarta `deprecated` e
põe estável antes de temporário. Essa enumeração é uma leitura local, sem sonda
de rede; acontece no gossip a cada 5 s e também em anúncios imediatos. Com pin,
essa lista não é publicada. Quem **guarda** é que não lidava:

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
  lugar ao próximo. As catracas autenticadas de TLS, logs e Chat também usam
  a lista inteira: uma resposta HTTP 401/403 pode vir de um processo fantasma
  naquele endereço e não prova que o principal recusou a chave. O anúncio
  futuro deve atualizar essa lista; o endereço guardado não é uma âncora
  permanente.
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
- A falha de rota é associada ao endpoint, host e porta. Uma falha na URL HTTP
  de inferência não apaga `addr_candidates` da porta P2P no mesmo host. Isso
  mantém o transporte P2P elegível quando apenas o serviço de inferência teve
  um solavanco, sem impedir que um `Announce` novo substitua endereços antigos.
- Aí entra `Network::request_peer_addresses`: manda `PeerRequest` aos bootstrap
  nodes e aos outros peers vivos (quem responde manda `Announce` + `PeerList`) e
  **poda os candidatos desacreditados da entrada local**, para que o `Announce`
  seguinte a repovoe em vez de somar-se ao cadáver. Limitado a uma vez por
  `REDISCOVERY_INTERVAL` (30 s) por peer, porque o Painel relê o cluster em
  ciclo curto. Sem broadcast na LAN: o par continua vivo e o `Announce` dele
  chega a cada 5 s de qualquer modo, e transmitir da suíte de rotas quebraria a
  regra de nunca falar UDP de verdade nos testes.
- O aviso `Reencontrando ... nenhum endereço conhecido responde` pode surgir
  por essa poda. A investigação encontrou uma ligação: a comparação antiga
  tratava falha HTTP de qualquer porta como falha do host inteiro, removendo
  também o endereço P2P, mesmo que o serviço de inferência ou o túnel em outra
  porta ainda respondesse. Agora a poda compara host e porta. Essa redescoberta
  não apaga uma sessão de KV por si só, mas deixar de remover uma porta P2P viva
  impede que uma falha HTTP transitória contamine os candidatos usados pelo
  pipeline.
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
- A cobertura do Painel sai do gossip, então um par ilegível continua
  contribuindo camadas - e é honesto que continue, porque o servidor de
  inferência dele pode estar perfeito e só o endereço de controle é que morreu.
  Na admissão de uma requisição, porém, o gateway também lê a faixa atual do
  `/health` do infer server. Essa leitura pode corrigir a fotografia P2P
  atrasada para o planejamento, sem tornar o `/health` fonte de verdade da
  cobertura visual. O que não pode é ficar invisível:
  `model_inventory[].unreadable_nodes` nomeia esses pares, e a tela tira o
  verde de "completo" enquanto a lista não estiver vazia.
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
  no Linux; Windows fica vazio e cai no endereço observado). Sem o `ip` (imagem
  Docker sem `iproute2`) a lista também fica vazia, com um aviso único no log:
  o nó não anuncia nem o IPv6 público, os peers da LAN guardam a origem privada
  do pacote e a repassam no `PeerList`, e um peer remoto passa a discar o IP da
  LAN e filtra com
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

Quando há endereço fixado, a precedência não significa "pin mais os demais":
`self_announce`, `self_addr_candidates` e a multiaddress de túnel publicam
somente o item 1. A lista de alternativas existe apenas no modo automático,
ou no caminho UDP de controle que precisa reencontrar um nó sem pin.

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
relatório pode marcar `direct-health` como fallback. O RTT é renovado a cada
120 s, independentemente da re-medida de endereçamento de 5 min. Se um peer
anuncia o túnel, mas o swarm ainda não está pronto para responder, a própria
rota HTTP de inferência continua como fallback `direct-health`; a capability
anunciada não faz a medição desaparecer. Nenhuma latência de outro peer é
transitiva ou reaproveitada como se fosse a aresta real.

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
DCuTR, AutoNAT e o behaviour de bitswap. Escuta na mesma porta `udp+10000` em
quatro endereços: `/ip4/0.0.0.0/tcp/<porta>`, `/ip6/::/tcp/<porta>`,
`/ip4/0.0.0.0/udp/<porta>/quic-v1` e `/ip6/::/udp/<porta>/quic-v1`.

- **Bootstrap libp2p:** os mesmos nós bootstrap do UDP, convertidos por
  `udp_to_libp2p_multiaddr` (IPv4/IPv6/hostname na porta TCP). A porta TCP é
  `udp+10000` usando a porta UDP efetivamente ligada; porta UDP zero é recusada.
  Um bootstrap IPv6 colado em `/ip4/` falhava o parse e era dialado sem palavra
  no log.
- **Transportes:** TCP e QUIC (`quic-v1`), os dois construídos no
  `SwarmBuilder`. QUIC não está ali por latência: o hole punch do DCuTR em TCP
  depende de simultaneous open, que a maioria dos NAT domésticos e
  praticamente todo CGNAT recusa. O caminho de UDP é o que o DCuTR foi
  desenhado para usar, e é o que torna a travessia viável sem IPv6.
- **Relays:** `DEFAULT_RELAY_NODES` está **vazio**. A lista trazia três
  bootstrappers públicos do IPFS, que não são relés: não concedem reserva de
  Circuit Relay v2 a um par qualquer, e os endereços fixados já estavam
  obsoletos. Relé útil aqui é um nó pom com IP público rodando `--serve-relay`,
  configurado em `--relay-peers` (resolvidos antes por pkarr, filtrando
  `is_relay`); só ele honra `max_circuit_bytes` do tamanho de um frame de
  prefill. `--no-default-relays` continua aceito e hoje não tem efeito prático.
  Com `--serve-relay` o servidor de relay aceita reservas (128 slots, 4 por
  peer, 1 h de reserva, circuitos de 120 s e `wire::MAX_FRAME_PAYLOAD_BYTES`);
  sem ele, `max_reservations: 0`. As flags valem com ou sem `--model`: um relé
  dedicado não serve modelo e roda pelo caminho `start_gateway_only`.
  `examples/relay-node/docker-compose.yml` é o exemplo de um relé dedicado,
  fora da rede que ele atende. Os nós atrás de NAT só precisam dele como
  bootstrap: a reserva é pedida a todo par conectado que oferece relé.
- **Reserva de circuito:** `reserve_circuit` pede `listen_on(<relé>/p2p-circuit)`
  para qualquer par cujo identify anuncie `relay::HOP_PROTOCOL_NAME`, não
  apenas para os relés configurados - o caso que mais importa é um nó pom com
  IP público dentro do próprio cluster. O endereço discável do relé sai da
  lista configurada quando ele está lá, e do anúncio vivo caso contrário;
  `listen_addrs` do identify continua fora, pela política "só Announce disca".
  `circuit_listens` guarda quem já foi pedido, porque `listen_on` repetido
  abre um listener novo a cada chamada e o identify do mesmo relé chega muitas
  vezes.
- **AutoNAT:** `NatStatus::Public(addr)` chama `Swarm::add_external_address`.
  Descobrir que se é público sem contar ao swarm não servia para nada: o
  endereço não ia no identify, os pares não o aprendiam, e o DCuTR ficava sem o
  endereço público que o par precisa discar no instante do punch.
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
O encerramento do endpoint QUIC de uma geração é um sinal one-shot local, sem
confirmação ou barreira entre peers. Ele aguarda no máximo 2 s pelo transporte
e aborta a task se o endpoint não fechar, para que o reload não fique preso por
uma notificação perdida ou por `endpoint.wait_idle()`.
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
- **mDNS:** descoberta na LAN que alimenta o endereço da Kademlia e diala o peer
  somente quando não há `advertise_addr` fixado. Com pin, o comportamento nem
  é construído, evitando churn e os eventos de descoberta fora do endpoint
  configurado.
- **Identify:** não transforma `listen_addrs` do remoto em candidatos da
  Kademlia. Essa lista é a interface de escuta do processo, não o contrato de
  endpoint do cluster, e pode conter LAN, bridge Docker e endereços Tailscale
  antigos. Para relays conectados, continua escutando no circuito
  (`P2pCircuit`) do relay.
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

O DHT Mainline do cliente pkarr é UDP IPv4 e roda em um ator de thread própria.
No Windows, quando não há resposta, o timeout de leitura de 50 ms é reportado
pela dependência como WSAETIMEDOUT (10060); isso é polling ocioso, não um retry
do QUIC ou do carregamento do modelo, e o ator não compartilha o loop da UI.
O binário filtra esse aviso esperado, mantendo erros reais. O endpoint
`/api/public/networks` não consegue enumerar registros arbitrários do DHT:
resolve apenas candidatos já aprendidos pela descoberta UDP/libp2p ou pelo cache
local. Assim, um endereço IPv6 global anunciado sem rota externa não basta para
um nó novo descobrir a rede.

### Diretório federado de redes públicas

O catálogo global separado do swarm vive em `public_directory.rs`. O principal
de uma rede pública assina um registro com endpoint, documento de confiança,
descrição, regras e canais em Markdown, localização e ingressos conhecidos. O
registro carrega `version`, `sequence`,
`issued_at`, `expires_at` e um nonce de prova de trabalho de 16 bits. A
identidade lógica é `(publisher_pubkey, sequence)`; o merge mantém a maior
sequência e resolve conflito idêntico de forma determinística pelo menor hash.

Cada nó pode configurar até 16 ingressos HTTP(S) em `POM_PUBLIC_DIRECTORY` ou
na administração. O wizard aceita uma lista vazia; o publicador envia seu
registro a todos os ingressos configurados, e um leitor
consulta todos em paralelo e grava o merge no `pom.db`. Uma réplica é transporte,
não raiz de confiança: assinatura Ed25519, cadeia de confiança, cluster e
fingerprint são validados pelo leitor e pelo ingresso. Registros expiram em
sete dias e o cache mantém no máximo 1024 itens.

Um nó privado pode consultar o catálogo por saída HTTP(S), sem IPv6 público,
porta de entrada ou relay. Para manter isolamento, ele não consulta candidatos
UDP/libp2p/pkarr do próprio swarm privado e não publica registro público. O
runtime v1 mantém uma só rede P2P por processo: entrar numa pública substitui a
rede privada antes de reiniciar. Assim um peer público não recebe bootstrap ou
endereços privados e não há interface pública paralela para encaminhar até a
rede privada. A alcançabilidade do endpoint listado continua sendo uma
propriedade separada, verificada pelo navegador e dependente da rota real.

### Travessia de NAT: o que já funciona e o que ainda falta

Dois nós atrás de NAT ou CGNAT ainda não se alcançam. A causa não é ajuste de
parâmetro: é que **nenhum caminho do código produz um endereço discável de um
par atrás de NAT**.

`PeerInfo::libp2p_dialable_addrs` (`network.rs`) devolve o *primeiro*
`host:port` válido de `addr`/`addr_candidates`, e `tunnel_peer_multiaddrs`
(`discovery.rs`) o converte por `udp_to_libp2p_multiaddr`, que só sabe emitir
`/ip4|ip6/<host>/tcp|udp/<porta>`. Daí saem três consequências encadeadas:

1. Atrás de NAT os candidatos anunciados são endereços RFC 1918 ou 100.64. A
   discagem vai para um endereço privado e falha, sempre.
2. A convenção `host:port` **não consegue expressar um circuito**. O endereço
   `/ip4/<relé>/tcp/<porta>/p2p/<relé>/p2p-circuit/p2p/<par>` não tem forma
   `host:port`, então um par com reserva aceita não tem como contá-la a
   ninguém.
3. O DCuTR coordena o hole punch **por cima de uma conexão relayed**. Sem
   endereço de circuito publicado não há conexão relayed, e sem ela o DCuTR
   nunca roda.

As fases 1 e 2 fecharam esse caminho. A 1 deu ao swarm a capacidade de
atravessar (QUIC, relé de verdade, reserva em qualquer par com hop, endereço
externo confirmado). A 2 publica e disca multiaddresses reais:
`publish_swarm_addrs` recolhe do swarm os endereços externos confirmados, os
listeners com IP público e os listeners de circuito, e os grava em
`Network::set_libp2p_addrs`; o `Announce` os leva em `libp2p_addrs`;
`PeerInfo::libp2p_dialable_addrs` os devolve **todos**, em vez do primeiro
`host:port`; e `tunnel_peer_multiaddrs` aceita as duas formas, acrescentando
`/p2p/<peer-id>` só quando falta - num circuito o relé já aparece antes do
`/p2p-circuit`, e duplicar o sufixo faria o swarm discar o relé como se fosse o
destino.

O registro pkarr publica o endereço público confirmado pelo AutoNAT quando ele
existe (`Network::public_libp2p_addr`), e cai no multiaddress derivado no
arranque quando não. Circuito **não** vai para o registro: ele é republicado a
cada 30 min e uma reserva não dura previsivelmente tanto, então o registro
passaria a maior parte do tempo apontando para um relé que não vale mais. O que
vale no DHT é a descoberta a frio de um nó público.

A fase 3 levou o plano de controle para o mesmo túnel. `TunnelRequest` ganhou
`target`: `Inference` continua sendo o padrão e o único que um peer de binário
anterior conhece; `Gateway` é atendido pelo gateway do destino. Com isso a
sonda `/handshake` da admissão e as catracas de política
(`/api/cluster/{tls,quic,logs,network-chat,network-settings}`) alcançam um par
que nenhum endereço direto alcança - sem isso ele não entrava em `peers` nem
com o hole punch perfeito, e roteamento, UI e pipeline continuavam cegos para
ele.

**A lista de rotas do gateway é fechada, e isso é uma invariante de segurança,
não uma conveniência.** Tudo que sai do túnel chega ao gateway por
`127.0.0.1`, e o gateway decide o que é local pelo endereço do socket
(`serve::PeerAddr`): `install_trust_anchor` libera a administração da rede
olhando **só** isso. Um túnel que encaminhasse caminho arbitrário daria a
qualquer peer do swarm um caminho para se tornar administrador. Por isso
`infer_tunnel::gateway_path_is_allowed` compara o caminho exato, sem prefixo
nem curinga, e a conferência é do lado que recebe. O emissor escolhe a
variante de `target`; quem resolve a porta é o receptor, com as duas portas
que ele próprio escolheu.

A **volta** da resposta direta usa o mesmo túnel. O gateway manda no `reply_to`
os endereços por onde ele mesmo é alcançável, e eles saem das interfaces locais
dele: com o gateway atrás de NAT, a cauda em outra rede disca
`http://192.168.x.x:8080/v1/relay-result` e nunca chega. Desde então o
`reply_to` carrega também o `libp2p_peer_id` e os `libp2p_addrs` do gateway, e
`direct::deliver_via_tunnel` é a última via depois de todas as URLs falharem.
Era o buraco que sobrava: a ida já atravessava o NAT e a volta morria calada
até o `DIRECT_REPLY_DEADLINE`, dez minutos por token.

O que continua fora do túnel: transferência de arquivos, sincronização de
admin e o watcher do `/health` entre peers. Nenhum deles participa da
admissão, então um par atrás de NAT já entra no cluster e roteia sem eles.

### Preferência aprendida pelo túnel

Furar o NAT não basta se cada frame continua pagando a escada direta inteira.
`PeerClients::prefer_tunnel` é uma marca **por peer**, aprendida: liga quando o
túnel entrega depois de a escada direta falhar inteira, e desliga no primeiro
sucesso direto. Ligada, a primeira tentativa de cada frame pula QUIC e HTTP e
vai direto ao túnel; uma falha do túnel devolve a escada completa na tentativa
seguinte, porque a rota direta pode voltar (hole punch novo, peer que mudou de
rede). Sem ela, atrás de NAT todo prefill e todo decode queimavam timeout de
QUIC mais timeout de HTTP antes do único transporte que entrega. A marca não é
configuração e não se propaga entre peers: um par atrás de NAT não empurra o
vizinho alcançável para o túnel.

### Diagnóstico

`Network::traversal_status` (`TraversalStatus`) guarda o estado do AutoNAT, os
multiaddresses publicados, os relés que aceitaram reserva e a contagem de hole
punches com a última falha. Sai em `/api/ui/cluster` no campo `traversal` e na
tela de Rede, painel "Travessia de NAT". Dois nós atrás de NAT que não se
enxergam falham num de três degraus - o AutoNAT nunca confirmou endereço,
nenhum relé aceitou reserva, ou o hole punch falhou - e antes disso a única
forma de saber qual era ler `RUST_LOG=debug` na máquina certa no momento certo.
O campo entra no fingerprint do cache do cluster; sem isso o retrato
congelaria, porque nem o estado do AutoNAT nem uma reserva nova mexem em
`peers`.

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
2. **O tráfego de inferência escolhe um transporte por preferência.** Quando
   QUIC está ligado, o gateway e os relays tentam HTTP/3 via
   `relay::send_frame_or_tunnel` e, se ele falhar, caem para o cliente TCP
   pinado: HTTP quando TLS está desligado e HTTPS quando está ativo. Só depois
   de todos os endereços diretos falharem o túnel `network/infer_tunnel.rs`
   pode ser usado, quando o hop anuncia `transport/libp2p-tunnel-v1` e um
   `PeerId` válido; ele é protegido por Noise e leva a credencial da inferência
   até o loopback do destino. Quando QUIC está desligado, a mesma ordem direta
   antes do túnel preserva HTTP/HTTPS como transporte efetivo preferencial.
   Antes de cada envio, o swarm reidrata uma única multiaddress TCP a partir do
   endpoint canônico levado pelo plano quando disponível, ou do primeiro
   endereço válido do anúncio vivo como fallback, quando ainda não há conexão.
   Com `advertise_addr`, esse endpoint é o único publicado; não existe chamada
   a `Swarm::add_external_address` que substitua ou complemente os listen
   addresses do Identify. Isso cobre a janela em que o
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
   `connection_key` é a credencial usada nas chamadas internas, inclusive no
   fallback HTTP público; HTTPS e QUIC também podem provar o canal pelo
   transporte seguro. API keys são aceitas para manter o mesmo contrato de
   autenticação disponível nas APIs externas. `/health` permanece público para
   readiness.

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

Os clientes HTTP de peer ativam `TCP_NODELAY` em todos os caminhos TCP, claros,
TLS e sondas. Isso evita que requests pequenos de inferência paguem a combinação
de Nagle com delayed ACK do sistema operacional. O `PeerClients` claro é
compartilhado e o cliente TLS é cacheado por `node_id`; as chamadas de
`for_peer` reutilizam o pool interno do reqwest em vez de criarem um cliente por
frame.

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
   conexão e `x-pom-internal-node: 1`.
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

### 5.1 QUIC opcional para a inferência

`Config.quic_enabled` é uma preferência network-wide, desligada por padrão e
editável somente pelo principal em `POST /api/ui/network/quic`. O anúncio leva
`quic` apenas como gatilho: o peer confirma o valor em
`GET /api/cluster/quic`, autenticado pela `connection_key`, exatamente como a
catraca de TLS. O gatilho só é aceito quando o `authority_pubkey` anunciado
corresponde ao principal ancorado localmente; um admin secundário não pode
forçar a consulta nem reverter a preferência. Falha na consulta preserva o
valor local. A troca é atômica e
vale somente para chamadas novas; conexões e inferências em andamento não são
interrompidas.

Quando ligada, o servidor de inferência e o gateway escutam HTTP/3 em endpoints
UDP dedicados, derivados do `p2p_port` **do próprio nó**:
`quic::inference_quic_port` = `p2p_port + 1000` e `quic::gateway_quic_port` =
`p2p_port + 2000`. A derivação nunca parte de `inference_port`/`gateway_port`:
num cluster co-locado (host networking) esses números são consecutivos entre
nós, então `porta + 1` cairia na porta de gateway do nó vizinho e quebrava a
descoberta. O `p2p_port` já é único por nó co-locado; `p2p_port` 0 (bind
efêmero) ou offset que estoura `u16` desligam o QUIC e mantêm o TCP. As portas
são publicadas no `Announce` como `inference_quic_port` e `gateway_quic_port`. O cliente constrói `host:porta` a partir do endereço
público anunciado pelo peer, incluindo a forma com IPv6 entre colchetes; nunca
extrai um destino UDP de `inference_url` nem tenta a porta TCP.

O servidor faz bind em `[::]` com `IPV6_V6ONLY=0` configurado explicitamente
via `socket2` antes de ser entregue ao Quinn. Isso preserva IPv4 e IPv6 no mesmo
endpoint em Windows, Linux e macOS; não há um bind alternativo exclusivo para
IPv4. O cliente do Quinn já configura a mesma opção no socket IPv6 wildcard.

O endpoint QUIC é criado pela identidade do nó, independentemente de
`tls_required` ou do `announced_tls` usado apenas para consultar a política no
principal. Quinn negocia TLS 1.3 próprio e o cliente fixa o certificado Ed25519
do `node_id`; portanto QUIC permanece cifrado mesmo quando `tls_required` está
desligado. A política TLS continua escolhendo apenas `http://` ou `https://`
para o fallback TCP. Se o peer não tiver endpoint QUIC, a porta não estiver
escutando, a rota falhar ou responder 404/405, o relay cai para HTTP/HTTPS e
marca `transport_fallback`; a descoberta compartilha um orçamento de 300 ms
entre os endereços QUIC anunciados, em vez de serializar um timeout por
endereço, e coloca um endpoint que falhou em quarentena por 5 s. Uma sessão
saudável é reutilizada nos próximos frames. Se uma chamada falhar numa sessão
cacheada, o cliente fecha e remove essa sessão e repete uma vez o pedido com um
handshake novo dentro do orçamento existente. Assim, o `reset by peer` esperado
quando o processo remoto reinicia não transforma uma conexão antiga em estado
permanente. A quarentena continua valendo para a falha final do endpoint e dura
5 s, evitando handshakes repetidos por token. O túnel só é a última reserva
depois de uma falha de transporte também no caminho TCP.
Cada tentativa completa de transporte, incluindo handshake, envio, headers e
corpo, tem teto de 30 s; isso impede que um endpoint alcançável mas sem resposta
prenda a inferência no timeout de processamento de 600 s. Esse deadline vale
também para o HTTP/HTTPS usado no fallback.
A telemetria por hop publica o transporte efetivo como `http`, `https` ou
`quic`. Para QUIC, `transport_rtt_us` é o RTT medido pela conexão UDP e os
quantis de rede ficam separados do tempo de processamento remoto.
`quic_request_body_send_us` soma o wall aguardado por `send_data` e `finish`
antes da leitura dos headers de resposta. O resumo conta as operações físicas
medidas, então um upload de batch compartilhado é registrado uma vez. Esse
campo mede contrapressão e aceitação local do corpo, não confirmação de entrega
remota.

## Logs de processo do cluster

O log de processo usa um protocolo request/response próprio do swarm,
`/pom/cluster-logs/1.0.0`, separado de `infer_tunnel.rs`. Cada nó envia uma
cópia do lote a todos os peers vivos anunciados como admin. Cada admin efetivo
persiste a entrada na ordem de chegada da própria réplica, sem ordenar pelo
relógio remoto, e guarda `origin_at` apenas como dado informativo. O receptor
confere sua própria autoridade efetiva em `AdminTrust`; um perfil `admin` sem
concessão não vira destino de leitura nem aceita a cópia.

A auditoria de requisições segue a mesma topologia em
`/pom/request-audit/1.0.0`: `AuditLog` replica os eventos `Begin` e `Complete`
para todos os admins conhecidos, e o receptor grava a cópia sem retransmiti-la.
Com isso, `/api/ui/cluster-logs`, `/api/ui/requests` e suas estatísticas não
dependem de o principal estar acessível depois que a réplica local foi
recebida. Uma conclusão que chega antes do início fica retida numa fila
limitada até o evento de início.

`cluster_logs_enabled` no `Announce` é somente gatilho. Quando diverge da
política local, o nó faz uma busca autenticada em `GET /api/cluster/logs` no
principal com `connection_key` e `x-pom-internal-node: 1`, e só grava o valor
retornado; falha na busca preserva o valor local. O interruptor administrativo
aplica a mudança sem reinício.

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
cada 100 ms, em modo best-effort, sem retentativa agressiva. Cada admin grava
as linhas em seu próprio `pom.db`, associadas ao `deploy_id`, e também persiste
o contador de descartes. Essa escolha replica a retenção completa atual: com
`N` admins há até `N` escritas e cópias de armazenamento por entrada. Não há
política de retenção secundária diferente para esconder esse custo; uma redução
futura precisa ser uma decisão explícita de produto, porque mudaria a semântica
de auditoria.

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
`x-pom-internal-node: 1`; a rota é comum/interna e não exige token humano. Só
adota uma resposta autenticada; falha preserva o valor local. O perfil
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
