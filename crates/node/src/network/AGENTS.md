# network/ - protocolo P2P

Transportes de descoberta e endereçamento do nó. Dois mundos no mesmo diretório:
o UDP nativo (`network.rs`, `addrs.rs`, `identity.rs`, `route.rs`) e o
libp2p/pkarr para a internet (`discovery.rs`, `pkarr_node.rs`).
`main.rs` monta `Network` e `Discovery`; `gateway.rs` e `routing/pipeline.rs`
consomem `PeerInfo` e `RouteTable`. A regra da casa: a decisão de endereço
pertence a quem vai discar, não a quem anunciou. Com `advertise_addr` fixado,
ele é o único endpoint anunciado e discado pelo libp2p; sem pin, a política de
fallback continua em `addr_candidates` + `RouteTable`.

## Arquivos

- `addrs.rs` - enumeração dos endereços locais por plataforma (`ifconfig` no
  macOS, `ip -o addr` no Linux) e a filtragem que decide o que anunciar.
  Captura também a interface dona do endereço para excluir bridges e pares
  virtuais de Docker, Podman, CNI e equivalentes sem perder IPs privados de
  interfaces reais da LAN. `preferred_local_advertise_ip` reaproveita essa
  filtragem para a sugestão do wizard, preferindo IPv6 global estável e depois
  IPv4, sem confundir endereço de privacidade com endereço persistente. Único
  arquivo com código por plataforma; a filtragem fica fora do `cfg` e tem teste
  que roda em qualquer sistema.
- `infer_tunnel.rs` - protocolo request/response que carrega a chamada de
  inferência pelo enxame libp2p como transporte preferencial quando o peer
  anuncia essa capability. Se o túnel falhar por transporte, o relay usa o
  cliente direto pinado como fallback. `TunnelRequest` leva método, caminho,
  corpo e cabeçalhos, inclusive a `connection_key` e o marcador interno da
  porta de inferência;
  `serve_loopback` repassa POSTs para o servidor de inferência e GETs para a
  sonda `/health`. POSTs sem `Content-Type` recebem o padrão binário, mas um
  `Content-Type` explícito, como `application/json` no ciclo de vida da sessão,
  nunca é duplicado nem sobrescrito. `measure_rtt` mede a ida e volta pelo
  mesmo túnel Noise.
- `cluster_logs.rs` - captura limitada do `tracing` e transporte request/response
  separado (`/pom/cluster-logs/1.0.0`); a fila descarta sob pressão e só o
  principal ingere e persiste lotes. `CaptureLayer` usa `INFO` como nível
  mínimo por padrão, configurável por `Config.cluster_logs_min_level`, sem
  alterar o filtro do console. No principal, `ClusterLogs::start` roda a poda
  em segundo plano: mantém sete dias e no máximo 500.000 linhas, em lotes de
  256 com pausa e tentativa não bloqueante.
- `cluster_logs_policy.rs` - catraca autenticada do interruptor de logs; o
  campo anunciado só dispara a leitura da política no principal.
- `network_chat.rs` - canal de conversa sobre o swarm libp2p, com request/
  response próprio, limites de texto/arquivo, fila limitada, rate limit e
  writer de persistência fora do loop do swarm. A ação original leva a API key;
  retransmissões internas levam apenas o frame já validado.
- `network_chat_policy.rs` - catraca autenticada do interruptor do Chat da
  rede; o booleano anunciado é gatilho e a leitura autenticada no principal é
  a fonte efetiva.
- `discovery.rs` - swarm libp2p: gossipsub, Kademlia, mDNS, identify, relay
  (cliente e servidor), DCuTR, AutoNAT, túnel de inferência e o
  protocolo de logs do cluster e Chat da rede. Põe peers no `Network` somente
  depois da catraca de admissão, e reidrata no swarm os endereços dos peers
  admitidos antes de enviar pelo túnel quando o anúncio chegou antes do
  registro de rota. A assinatura nativa do gossipsub prova a origem do
  transporte, mas não substitui a prova de pertencimento do enxame.
- `identity.rs` - chave Ed25519 de longa duração e o `node_id` (SHA256 da
  pubkey). Vive em `<project_root>/.pom-node.key` (ou `$POM_NODE_KEY`).
- `network.rs` - o coração do P2P UDP: `P2PMessage`, `PeerInfo`, socket
  dual-stack, gossip, lifecycle de peers, varredura de descoberta e admissão.
  A `connection_key` não vai no `Announce`: em rede privada, o handler faz
  uma sonda no `/handshake` antes de registrar o peer. Em rede pública,
  `cluster_id` só seleciona a cadeia e a tupla `{node_id, peer_id, pubkey}`
  precisa ser uma concessão ativa em `admin_trust.rs`; campo `profile` não
  concede acesso. `PeerList` é revalidado, e `connection_verified` só é
  escrito pelo gate local. O perfil anunciado por `with_identity` pode ser
  atualizado por `set_profile`; a capacidade de admin não vem do anúncio, mas
  da cadeia em `admin_trust.rs`. O campo `idle` também é anunciado por peer
  para distinguir modelo descarregado de deployment fora do ar; ele não altera
  a regra de liveness.
- `peer_client.rs` - `PeerClients`, um `reqwest::Client` por peer, porque o
  verifier do TLS é por destino. Sem TLS volta a ser um só. Guarda também o
  esquema global das URLs (`tls_ativo`/`peer_scheme`) e `peer_base_url`, que é
  quem colcheta IPv6, além da preferência e da quarentena temporária de rotas
  que falharam. `route_order` apenas reordena os candidatos: mesmo quando
  todos estão em quarentena, devolve a lista para que a tentativa real decida.
  `get_internal_from_candidates` percorre todos os gateways anunciados para
  uma rota de controle autenticada; inclusive 401/403 fazem a sonda tentar o
  próximo endereço, porque um endereço fantasma pode responder como outro nó.
  O cliente destinado a outro nó marca requests de controle com
  `x-pom-internal-node`, usa a `connection_key` somente nas rotas privadas de
  inferência e limita a abertura TCP a 2 s; em rede pública a inferência direta
  usa TLS e o mesmo marcador sem segredo compartilhado. O cliente `plain()` para
  Ollama e
  loopback não envia essa marca. `for_peer_with_state` informa ao trace se o
  cliente TLS foi criado ou reutilizado; transporte local, túnel e cliente
  sem TLS permanecem explicitamente desconhecidos.
- `quic.rs` - cliente e servidor HTTP/3 sobre Quinn. O pool mantém uma conexão
  QUIC por peer e endereço, usa o mesmo pinning Ed25519 da conexão TCP e deixa
  o endpoint UDP pronto na porta de inferência mesmo quando a preferência está
  desligada. A configuração do listener é independente do `tls_required` do
  TCP, porque QUIC leva seu próprio TLS 1.3.
- `pkarr_node.rs` - publicação/resolução no DHT pkarr: TXT `_pom` assinado,
  `publish`/`resolve_peers`.
- `tls.rs` - certificado auto assinado derivado da `.pom-node.key` e o
  `ServerCertVerifier` que confere `SHA256(pubkey) == node_id`. Nada vai para o
  disco; não há CA. Ver `docs/subsystems/rede-p2p.md`, seção 5.
- `tls_policy.rs` - a catraca: o campo `tls` do Announce é gatilho, e só a
  busca autenticada em `GET /api/cluster/tls` no principal grava a política.
  Busca que falha preserva o valor local.
- `quic_policy.rs` - catraca autenticada da preferência QUIC: o campo `quic`
  do Announce só dispara `GET /api/cluster/quic`; apenas a resposta do
  principal é adotada e falha preserva o valor local.
- `cluster_logs_policy.rs` aplica a mesma disciplina ao campo
  `cluster_logs_enabled`: anúncio é gatilho, leitura autenticada no principal
  é a fonte, e falha preserva o valor local.
- `network_chat_enabled` segue o mesmo contrato. O gatilho só é aceito como
  decisão do principal cuja `authority_pubkey` foi confirmada pelo
  `AdminTrust`; o perfil anunciado não identifica autoridade. O interruptor
  aberto num admin secundário encaminha a mutação ao principal e só a gravação
  dele muda a política autoritativa. A retransmissão do Chat da rede é estrela:
  o principal valida API key ativa e `blocked_nodes` antes de enviar a outros
  `node_id`.
- `route.rs` - `RouteTable`: qual endereço usar para cada peer, com sonda
  `/health`, histerese contra troca de rota por ruído e invalidação imediata
  quando o peer muda seus endereços anunciados. Guarda também a **memória de
  falha por `(node_id, URL)`**, que serve tanto ao pipeline quanto ao plano de
  controle - ver `docs/subsystems/rede-p2p.md`, "Endereço que deixou de
  responder". `RttTable` guarda a medição direcional deste nó para cada
  destino, sem inferir o sentido inverso, e expõe snapshots com idade: só os
  RTTs com menos de cinco minutos entram em seleção.

## Funções-chave

`network.rs`:
- `Network::new` - bind dual-stack (`[::]`) montado à mão com `socket2` e
  `IPV6_V6ONLY` desligado **antes** do bind (depois não tem efeito; no Windows o
  padrão é 1 e um nó lá não veria peer IPv4 nenhum). Sem IPv6 no SO, cai para
  `0.0.0.0`.
- `Network::testkit` - socket UDP isolado em `127.0.0.1:0`, com o destino do
  anúncio local preso à loopback; `testkit_with_broadcast_addr` permite injetar
  um endpoint loopback para observar o datagrama e recusa o broadcast global.
- `Network::start` - spawna o receive loop, o loop de gossip (5 s: announce
  incondicional + `discovery_sweep` + `cleanup_dead_peers`) e a task de medição
  de rotas e RTT (30 s, sonda lenta não segura o gossip). A política TLS inicial
  é aplicada antes de `start`; a task sonda `/health` na porta de inferência de
  cada candidato e mede o RTT por libp2p quando o peer anuncia o túnel. Cada
  `self_announce` enumera as interfaces uma vez por anúncio, portanto a
  troca de IPv6 temporário entra no gossip seguinte sem sondagem de rede. Com
  pin explícito, essa enumeração não é publicada: o pin substitui a lista local.
  O
  tratamento de cada datagrama é protegido por `catch_unwind`, com contexto no
  aviso e limite de frequência para não inundar o log se o pânico se repetir.
- `Network::handle_message` - Announce (reescreve `0.0.0.0` com o endereço de
  origem e chama `merge_observed` somente quando não há candidatos explícitos), PeerRequest
  (responde Announce imediato + `PeerList` fatiado), PeerList, Heartbeat,
  LocalDiscover. O `PeerList` também
  atualiza um peer já conhecido quando o snapshot é mais novo, preservando o
  estado local de verificação e os candidatos de endereço **que este nó ainda
  não provou mortos**; snapshots antigos são
  ignorados para não reverter a cobertura. Nó destacado (`is_detached`) não
  aprende nem responde. Um pin recebido não ganha o endereço de origem como
  candidato adicional.
- `Network::request_peer_addresses` - quando **todo** endereço conhecido de um
  peer está desacreditado, manda `PeerRequest` aos bootstrap nodes e aos outros
  peers vivos e poda os candidatos mortos da entrada local, para que o
  `Announce` seguinte a repovoe. Uma vez por `REDISCOVERY_INTERVAL` (30 s) por
  peer; `is_rediscovering` é o que o Painel lê para dizer "reencontrando" em vez
  de "não dá para ler", e ele expira em `REDISCOVERY_GIVE_UP` (90 s) - promessa
  sem prazo seria o mesmo aviso desonesto com outra palavra. `note_peer_read`
  encerra a busca quando o par volta a ser lido. Endereços IPv6 temporários
  rodam sozinhos, e o cluster tem de aguentar isso sem `--advertise-addr`.
- `Network::cleanup_dead_peers` - além de remover o peer expirado, chama
  `RouteTable::forget` nele, para que um nó que volta com o mesmo endereço não
  nasça desacreditado.
- `PeerInfo::dialable_addrs` - todos os endereços UDP válidos, com deduplicação,
  para que descoberta e reencontro tentem os candidatos antes de desistir.
  `PeerInfo::libp2p_dialable_addrs` reduz o conjunto a um endpoint canônico,
  impedindo que o pipeline ou o Chat transforme listen addresses antigos em
  fan-out de túneis.
- `PeerInfo::gateway_candidates` / `gateway_url` - uma URL de gateway por
  endereço conhecido, com `addr` primeiro (fato conhecido: é de onde os pacotes
  do peer chegaram) e os candidatos anunciados depois; `gateway_url` é só a
  primeira. Enquanto era só `peer.addr`, o plano de controle inteiro (Painel,
  `deploy`, transferência, `devupdate`) tinha um endereço e nenhum plano B.
  Mesma disciplina de `pipeline::resolve_step_url`: palpite não passa na frente
  de fato conhecido.
- `Network::discovery_sweep` - PeerRequest rotativo: 3 por varredura, varredura
  a cada 6 tiques (30 s), rodízio cobre o cluster inteiro. É o que aprende peers
  transitivos fora da LAN; sem isto a descoberta é um retrato do arranque.
- `Network::normalize` - mapeia destino IPv4 para `::ffff:a.b.c.d` no socket
  dual-stack. Sem isto o macOS devolve EINVAL em todo send e a descoberta morre
  num sentido só (Linux aceita, por isso parecia unilateral).
- `peer_list_chunks` - fatia `PeerList` para caber em `MAX_DATAGRAM_PAYLOAD`
  (1200 B, o piso que o IPv6 garante; a 5G do captain tem MTU 1450 e IPv6 não
  fragmenta).
- `advertised_addr` - normaliza endereço digitado à mão (IPv6 puro precisa de
  colchetes para ganhar porta) em `host:port`.
- `detach`/`rejoin` - tirar/voltar o nó da rede sem derrubar a UI/API
  (`--no-discovery`); rejoin refaz bootstrap + announce.
- `self_announce` - monta o Announce; anuncia zero camadas quando não há
  assignment, para que nó configurado pela UI ainda seja visível. Relê as
  interfaces uma vez a cada anúncio, a cada 5 s no gossip e também em anúncios
  imediatos; é uma leitura local de `ifconfig`/`ip`, sem sonda de rede. O mesmo
  snapshot alimenta `addr` e `addr_candidates`. Com fixação explícita, ambos
  contêm somente o `advertise_addr`; sem ela, o endereço segue a ordem em UDP e
  gossip: endereço público atual das interfaces com estável antes de temporário,
  endereço observado por `identify`, demais interfaces locais e o sentinela
  `0.0.0.0`. Bridges e pares virtuais de container não entram nos candidatos
  automáticos. Quando o
  servidor de inferência está carregado, o anúncio também inclui
  `inference/remote-token-stream-v1`, que permite ao gateway remoto pedir o
  laço de decode com callbacks em lotes limitados. O mesmo anúncio leva
  `transport/link-v1`, a ligação persistente entre andares (`/v1/link`), que é
  servida pelo mesmo router das demais rotas `/v1`. A capability
  `inference/frame-v1-batch` só entra quando o servidor de inferência carregou
  um modelo e o scheduler multi-item está ativo; setup, TP, carregamento e
  queda não anunciam capacidade de inferência. `runtime_capacity` só entra
  depois de `/health` confirmar o modelo carregado e leva `n_ctx` e
  `max_new_tokens` (perna); apply, erro, stop e thermal unload limpam o campo
  antes de anunciar. Não existe anúncio ou fallback
  para formatos de frame anteriores, porque o produto ainda não foi lançado.
  `cluster_logs_enabled` também é anunciado, mas só como gatilho da catraca:
  a política efetiva vem de uma busca autenticada no principal.
- `TpIdentity` (`{role, group_id, size, rank}`) - identidade de tensor
  parallelism (caminho C) anunciada em `Announce.tp`/`PeerInfo.tp`
  (`None` = nó sem TP). Cada `TpMember` leva o `PeerId`, os candidatos de
  endereço UDP e a capacidade do túnel libp2p preferencial entre redes.
  `set_tp`/`tp_identity` ligam/leem sem reiniciar;
  `PeerInfo::is_tp_worker` é o predicado que o planejador usa para excluir
  workers da cadeia.

`discovery.rs`:
- `Discovery::new` - constrói o swarm (porta TCP = UDP+10000), diala bootstrap
  convertido por `udp_to_libp2p_multiaddr` e os relays (padrão IPFS + `--relay-peers`).
- `handle_event` - Gossipsub Announce vira `PeerInfo` usando
  `addr_candidates.first()` (o literal `0.0.0.0` só sobrevive se nada foi
  anunciado) e registra somente o endpoint canônico como multiaddress
  `/p2p/<peer-id>` antes de aquecer a conexão do túnel; mDNS descobre na LAN e
  só é construído quando não há `advertise_addr` pinado; com pin, nem o
  comportamento mDNS roda. Identify não grava `listen_addrs` no livro de
  endereços e, para relays conectados, escuta no
  circuito (`P2pCircuit`); requisições recebidas pelo túnel são encaminhadas
  em tasks separadas para que um prefill, decode ou TP longo não pare o loop
  que drena GossipSub e as respostas dos outros protocolos; DCuTR/AutoNAT
  só logam. O request/response de logs e o do Chat da rede usam behaviours
  próprios, separados do túnel de inferência. O Chat valida no principal,
  retransmite sem segredo e recusa relays que não vieram de um administrador.
  O `select!` é biased para o swarm e o túnel de inferência; filas de Chat usam
  `try_send`, descartam sob pressão e nunca esperam SQLite.
- `udp_to_libp2p_multiaddr` - `host:port` UDP → multiaddr `/ip4`/`/ip6`/`/dns`
  na porta TCP. Conserta o bug de IPv6 que era colado num `/ip4/`, falhava o
  parse e era dialado sem nenhuma palavra no log.
- `observed_host_port` - inverso do anterior: tira o `host:port` deste nó do
  `observed_addr` do `identify`. É o "qual é o meu IP público" sem dependência
  de STUN. Recusa endereço privado/CGNAT (`addrs::is_private_or_local`) e
  qualquer coisa que não seja `/ip4|ip6/.../tcp/<porta>`.
- `announce_message_id` - id determinístico do tópico + payload. O gossipsub
  agora usa autenticação assinada e validação estrita; `handle_event` confere
  que o `libp2p_peer_id` declarado no `Announce` é o `source` autenticado antes
  de inserir o peer. Essa ligação de transporte não substitui a cadeia
  Ed25519 de autoridade administrativa.

`addrs.rs`:
- `filter_candidates` - descarta loopback/link-local/deprecated, estável antes
  de temporário, teto de 2 por família (o macOS cria vários IPv6 sozinho e o
  `PeerList` já está no limite da MTU); endereço fixado pelo operador vem
  primeiro e não conta para o teto. `Network` separa esses candidatos em
  públicos e privados: o público atual vem antes do observado por `identify`,
  e o observado continua fallback para NAT. A comparação com o que já entrou
  é feita sem colchetes dos dois lados - `fixado` vem colchetado de
  `effective_host` e o local vem cru, e comparar as duas formas duplicava o
  IPv6 no anúncio.
- `is_private_or_local` - decide se um endereço observado por um peer é notícia
  sobre o IP público deste nó. Além de RFC 1918 e `fc00::/7`, recusa
  **`100.64/10` (RFC 6598)**: é a faixa do CGNAT e é a que a Tailscale usa.
  Medido em 2026-08-16 no MacBook: a primeira resposta de `identify` veio por
  um peer alcançado pelo tailnet e reportou `100.122.132.97`, que o nó adotou
  como endereço *público* e anunciou ao cluster inteiro.
- `local_addrs` - `ifconfig` (macOS) / `ip -o addr` (Linux); Windows vazio
  (cai no endereço observado pelo peer).

`identity.rs`:
- `Identity::load_or_create` - carrega a chave ou gera; regenerar a cada boot
  duplicava o peer na lista até o timeout e voidava `blocked_nodes` (chaveado
  por node_id). Corrompida, gera nova em vez de parar.
- `NodeId::from_pubkey` - SHA256 da pubkey; `Display` imprime os primeiros 12.
- `key_path` / `pkarr_key_path` - `.pom-node.key` (identidade no cluster) e
  `.pom.key` (identidade na DHT pkarr), ambas resolvidas contra
  `config::project_root()`, com override por `$POM_NODE_KEY` e
  `$POM_PKARR_KEY`. Nenhuma das duas sai do cwd: lançar o binário de outro
  diretório criaria uma segunda identidade.

`route.rs`:
- `choose_route` - melhor candidato vivo; só troca de rota atual se o ganho
  passar de 25% relativo **e** 2 ms absolutos (medido 9,6 vs 7,2 ms
  MacBook↔ksia não pode trocar a cada ciclo).
- `RouteTable::measure_peer` - sonda `GET /health` na própria URL de inferência
  anunciada, não no gateway. Assim um gateway saudável não promove uma porta
  de inferência que engole SYN. A sonda aceita o certificado autoassinado e o
  hostname do peer; isso não altera o cliente pinado usado pela inferência.
  Timeout de 3 s e intervalo de 5 min.
- `RouteTable::mark_failure` / `mark_success` / `is_failed` / `usable` - a
  memória de falha, chaveada por `(node_id, URL)` e válida por 5 min. Grava
  quando uma **requisição real** falhou naquele endereço (sonda lenta não
  escreve: o timeout dela é apertado demais e desacreditar tudo deixaria
  `resolve_step_url` sem rota). Como a chave é a URL inteira, a porta de
  inferência e a do gateway do mesmo host convivem - e é a mesma memória que a
  leitura do Painel usa, em vez de um mecanismo paralelo. `usable` devolve os
  candidatos ainda vivos; vazio significa "todo endereço deste par falhou", e o
  chamador deve pedir o atual em vez de insistir.
- `RouteTable::invalidate` **preserva** a memória de falha; só `forget` a apaga.
  Apagá-la em `invalidate` significava que a rotação de IPv6 - o evento que mata
  o endereço - era também o que apagava a prova de que ele morreu.
- `RouteTable::host_is_discredited` - a mesma pergunta por host, para o gossip
  decidir se herda um candidato antigo.
- `RouteTable::resolve` - endereço escolhido para o node_id; `None` =
  "ainda não medido ou vencido", e o chamador cai no `inference_url` do plano.
- `RttTable` - guarda `target_node_id`, `rtt_ms` e `transport` localmente;
  `libp2p` é a medição normal e `direct-health` identifica o fallback de sonda.
  `fresh_snapshot` é a única fonte de RTT para seleção; `snapshot_with_age`
  conserva a idade para diagnósticos. O endpoint autenticado
  `/api/ui/network/rtt` publica só medições frescas com `source_node_id`.

`pkarr_node.rs`:
- `PkarrNode::publish` - assina TXT `_pom` (JSON compacto, campos de uma
  letra para caber no limite DNS de ~1000 B) e publica no DHT. O campo `g`
  carrega o rótulo geo canônico da rede como metadado, sem alterar a chave
  pkarr nem o contrato de `Announce`.
- `resolve_peers`/`resolve_one` - resolve chaves `pk:` em `ResolvedPeer`
  (multiaddr + faixa de camadas + `is_relay` + rótulo geo); falha vira `None`,
  nunca erro. O gateway mantém um índice local dos rótulos observados para
  filtrar consultas públicas por prefixo/sufixo antes de sondar peers.
- `detect_local_ip` - IP de saída por socket UDP conectado a 8.8.8.8:80 (nada é
  enviado).

## Ligações

- **Chamado por**: `main.rs` (constrói `Network` + `Discovery`, resolve
  relays/pkarr, publica no gossipsub a cada 5 s o mesmo anúncio do UDP, via
  `Network::gossip_announce_bytes` -> `self_announce`, com ou sem assignment;
  diala `--pkarr-peers`),
  `routing/pipeline.rs` (`resolve_step_url` usa `RouteTable`), `gateway.rs`
  (consome `PeerInfo`/`alive_peers`).
- **Chama**: `network.rs` ← `identity.rs` (via main), `addrs.rs`
  (`self_addr_candidates`), `route.rs` (`Network.routes`); `discovery.rs` →
  `network.rs::insert_peer`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
