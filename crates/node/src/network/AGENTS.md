# network/ - protocolo P2P

Transportes de descoberta e endereçamento do nó. Dois mundos no mesmo diretório:
o UDP nativo (`network.rs`, `addrs.rs`, `identity.rs`, `route.rs`) e o
libp2p/pkarr para a internet (`discovery.rs`, `pkarr_node.rs`, `bitswap.rs`).
`main.rs` monta `Network` e `Discovery`; `gateway.rs` e `routing/pipeline.rs`
consomem `PeerInfo` e `RouteTable`. A regra da casa: a decisão de endereço
pertence a quem vai discar, não a quem anunciou (daí `addr_candidates` + `RouteTable`).

## Arquivos

- `addrs.rs` - enumeração dos endereços locais por plataforma (`ifconfig` no
  macOS, `ip -o addr` no Linux) e a filtragem que decide o que anunciar.
  Único arquivo com código por plataforma; a filtragem fica fora do `cfg` e tem
  teste que roda em qualquer sistema.
- `infer_tunnel.rs` - protocolo request/response que carrega a chamada de
  inferência pelo enxame libp2p como transporte preferencial quando o peer
  anuncia essa capability. Se o túnel falhar por transporte, o relay usa o
  cliente direto pinado como fallback. `TunnelRequest` leva método, caminho,
  corpo e cabeçalhos, inclusive a credencial da porta de inferência;
  `serve_loopback` repassa POSTs para o servidor de inferência e GETs para a
  sonda `/health`. `measure_rtt` mede a ida e volta pelo mesmo túnel Noise.
- `bitswap.rs` - codec libp2p request/response para um protocolo bitswap
  (manifesto + chunks com SHA256/MD5). Definido e ligado ao swarm, mas ninguém
  atende requisição (ver Defeitos).
- `discovery.rs` - swarm libp2p: gossipsub, Kademlia, mDNS, identify, relay
  (cliente e servidor), DCuTR, AutoNAT, bitswap, túnel de inferência. Põe peers
  no `Network` via `insert_peer`.
- `identity.rs` - chave Ed25519 de longa duração e o `node_id` (SHA256 da
  pubkey). Vive em `<project_root>/.pom-node.key` (ou `$POM_NODE_KEY`).
- `network.rs` - o coração do P2P UDP: `P2PMessage`, `PeerInfo`, socket
  dual-stack, gossip, lifecycle de peers, varredura de descoberta.
- `peer_client.rs` - `PeerClients`, um `reqwest::Client` por peer, porque o
  verifier do TLS é por destino. Sem TLS volta a ser um só. Guarda também o
  esquema global das URLs (`tls_ativo`/`peer_scheme`) e `peer_base_url`, que é
  quem colcheta IPv6.
- `pkarr_node.rs` - publicação/resolução no DHT pkarr: TXT `_pom` assinado,
  `publish`/`resolve_peers`.
- `tls.rs` - certificado auto assinado derivado da `.pom-node.key` e o
  `ServerCertVerifier` que confere `SHA256(pubkey) == node_id`. Nada vai para o
  disco; não há CA. Ver `docs/subsystems/rede-p2p.md`, seção 5.
- `tls_policy.rs` - a catraca: o campo `tls` do Announce é gatilho, e só a
  busca autenticada em `GET /api/cluster/tls` no principal grava a política.
  Busca que falha preserva o valor local.
- `route.rs` - `RouteTable`: qual endereço usar para cada peer, com sonda
  `/health` e histerese contra troca de rota por ruído. `RttTable` guarda a
  medição direcional deste nó para cada destino, sem inferir o sentido inverso.

## Funções-chave

`network.rs`:
- `Network::new` - bind dual-stack (`[::]`) montado à mão com `socket2` e
  `IPV6_V6ONLY` desligado **antes** do bind (depois não tem efeito; no Windows o
  padrão é 1 e um nó lá não veria peer IPv4 nenhum). Sem IPv6 no SO, cai para
  `0.0.0.0`.
- `Network::start` - spawna o receive loop, o loop de gossip (5 s: announce
  incondicional + `discovery_sweep` + `cleanup_dead_peers`) e a task de medição
  de rotas e RTT (30 s, sonda lenta não segura o gossip). A política TLS inicial
  é aplicada antes de `start`; a task mede o RTT por libp2p quando o peer
  anuncia o túnel e usa o health HTTP apenas para peers antigos.
- `Network::handle_message` - Announce (reescreve `0.0.0.0` com o endereço de
  origem e chama `merge_observed`), PeerRequest (responde Announce imediato +
  `PeerList` fatiado), PeerList, Heartbeat, LocalDiscover. Nó destacado
  (`is_detached`) não aprende nem responde.
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
  assignment, para que nó configurado pela UI ainda seja visível. O endereço
  segue a mesma ordem em UDP e gossip: fixação explícita, endereço público
  atual das interfaces com estável antes de temporário, endereço observado por
  `identify`, demais interfaces locais e o sentinela `0.0.0.0`.
- `TpIdentity` (`{role, group_id, size, rank}`) - identidade de tensor
  parallelism (caminho C) anunciada em `Announce.tp`/`PeerInfo.tp`
  (`None` = nó sem TP). Cada `TpMember` leva o `PeerId` e a capacidade do
  túnel libp2p preferencial entre redes. `set_tp`/`tp_identity` ligam/leem sem reiniciar;
  `PeerInfo::is_tp_worker` é o predicado que o planejador usa para excluir
  workers da cadeia.

`discovery.rs`:
- `Discovery::new` - constrói o swarm (porta TCP = UDP+10000), diala bootstrap
  convertido por `udp_to_libp2p_multiaddr` e os relays (padrão IPFS + `--relay-peers`).
- `handle_event` - Gossipsub Announce vira `PeerInfo` usando
  `addr_candidates.first()` (o literal `0.0.0.0` só sobrevive se nada foi
  anunciado) e registra os candidatos como multiaddresses `/p2p/<peer-id>`
  antes de aquecer a conexão do túnel; mDNS descobre na LAN e alimenta
  Kademlia; Identify grava listen_addrs e, para relays conectados, escuta na
  circuito (`P2pCircuit`); DCuTR/AutoNAT/Bitswap só logam.
- `udp_to_libp2p_multiaddr` - `host:port` UDP → multiaddr `/ip4`/`/ip6`/`/dns`
  na porta TCP. Conserta o bug de IPv6 que era colado num `/ip4/`, falhava o
  parse e era dialado sem nenhuma palavra no log.
- `observed_host_port` - inverso do anterior: tira o `host:port` deste nó do
  `observed_addr` do `identify`. É o "qual é o meu IP público" sem dependência
  de STUN. Recusa endereço privado/CGNAT (`addrs::is_private_or_local`) e
  qualquer coisa que não seja `/ip4|ip6/.../tcp/<porta>`.
- `announce_message_id` - **como o gossipsub distingue dois anúncios, e sem ela
  ele não distingue nada.** A função padrão do libp2p é `source + sequence`, e
  as duas são `None` sob `MessageAuthenticity::Anonymous`, que é o modo deste
  swarm: todo anúncio saía com o id constante `15R0` (`31355230` no log). O
  efeito medido em 2026-08-16, no cluster de verdade e com a malha formada
  (4 peers grafados): **zero** mensagem de gossipsub aceita em 90 s, e todo
  publish depois do primeiro recusado com `Not publishing a message that has
  already been published`. Hoje o id é hash do tópico + payload; o `timestamp`
  que o `self_announce` já grava faz cada anúncio diferir. É configuração
  local, então um nó atualizado convive com um que ainda usa o padrão.

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
- `RouteTable::measure_peer` - sonda `GET /health` num endpoint real do peer,
  preferindo a porta de gateway porque ela continua disponível sem modelo ou
  quando a porta de inferência está restrita. A escolha gravada continua sendo
  a URL de inferência correspondente. A sonda aceita o certificado autoassinado
  e o hostname do peer; isso não altera o cliente pinado usado pela inferência.
  Timeout de 3 s e intervalo de 5 min.
- `RouteTable::mark_failure` - tira a rota de uso na hora quando uma requisição
  real falhou naquele endereço, sem esperar o ciclo.
- `RouteTable::resolve` - endereço escolhido para o node_id; `None` =
  "ainda não medido", e o chamador cai no `inference_url` do plano.
- `RttTable` - guarda `target_node_id`, `rtt_ms` e `transport` localmente;
  `libp2p` é a medição normal e `direct-health` identifica compatibilidade com
  peers antigos. O endpoint autenticado `/api/ui/network/rtt` publica esse
  snapshot com o `source_node_id` para o planejador montar arestas direcionais.

`pkarr_node.rs`:
- `PkarrNode::publish` - assina TXT `_pom` (JSON compacto, campos de uma
  letra para caber no limite DNS de ~1000 B) e publica no DHT.
- `resolve_peers`/`resolve_one` - resolve chaves `pk:` em `ResolvedPeer`
  (multiaddr + faixa de camadas + `is_relay`); falha vira `None`, nunca erro.
- `detect_local_ip` - IP de saída por socket UDP conectado a 8.8.8.8:80 (nada é
  enviado).

`bitswap.rs`:
- `BitswapCodec` - enquadramento `[4 bytes BE len][JSON]` sobre o
  request/response do libp2p.
- `bitswap_behaviour` - behaviour request/response para `BSWP_PROTOCOL`
  (`/pom/bitswap/1.0.0`).

## Ligações

- **Chamado por**: `main.rs` (constrói `Network` + `Discovery`, resolve
  relays/pkarr, publica no gossipsub a cada 5 s o mesmo anúncio do UDP, via
  `Network::gossip_announce_bytes` -> `self_announce`, com ou sem assignment;
  diala `--pkarr-peers`),
  `routing/pipeline.rs` (`resolve_step_url` usa `RouteTable`), `gateway.rs`
  (consome `PeerInfo`/`alive_peers`).
- **Chama**: `network.rs` ← `identity.rs` (via main), `addrs.rs`
  (`self_addr_candidates`), `route.rs` (`Network.routes`); `discovery.rs` →
  `network.rs::insert_peer` e `bitswap.rs`.

## Defeitos anotados

- **bitswap é protocolo órfão**: `bitswap_behaviour` está no swarm e os eventos
  só são logados em `debug!` (`discovery.rs::handle_event`) - nenhuma requisição
  é atendida, nenhum bloco é servido. A transferência real vai por HTTP Range
  (`backend/transfer.rs`). O protocolo existe mas ninguém implementa o lado
  servidor.

- **Um hop inalcançável custa o timeout de `connect` do sistema antes de o
  túnel ser tentado**: `relay::send_frame` fixa 600 s por requisição (sobrepõe
  o do cliente), então quem decide é o SO - medido em **75 s** no macOS contra
  um endereço que engole os SYN (`relay::tests::the_tunnel_carries_a_real_frame_to_a_live_peer`).
  O desvio pelo túnel funciona e é rápido depois disso; o problema é o tempo
  até chegar nele.
- **O `receive_loop` não tem isolamento de pânico**: ele aguarda
  `handle_message` em linha, então um pânico ali mata a task e o nó **para de
  receber UDP para sempre**, enquanto HTTP, gossip e TUI seguem saudáveis - a
  única pista é o socket com a fila de recepção cheia. Aconteceu de verdade em
  2026-08-16 (`&node_id[..12]` num id de dois caracteres, corrigido com
  `short()`), e derrubou os dois nós do captain que ficam na porta 42777 da LAN.
  O guarda contra o próximo pânico não existe.
- **A suíte de testes transmite na LAN de verdade**: `broadcast_local` manda
  para `255.255.255.255:42777` qualquer que seja a porta do nó, e há teste que
  chama `announce` num nó não destacado. Rodar `cargo test` ao lado de um
  cluster vivo injeta mensagens de teste nele - foi assim que o id `"eu"` chegou
  ao nó do captain.
