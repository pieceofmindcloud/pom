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
  Captura também a interface dona do endereço para excluir bridges e pares
  virtuais de Docker, Podman, CNI e equivalentes sem perder IPs privados de
  interfaces reais da LAN. Único arquivo com código por plataforma; a
  filtragem fica fora do `cfg` e tem teste que roda em qualquer sistema.
- `infer_tunnel.rs` - protocolo request/response que carrega a chamada de
  inferência pelo enxame libp2p como transporte preferencial quando o peer
  anuncia essa capability. Se o túnel falhar por transporte, o relay usa o
  cliente direto pinado como fallback. `TunnelRequest` leva método, caminho,
  corpo e cabeçalhos, inclusive a credencial da porta de inferência;
  `serve_loopback` repassa POSTs para o servidor de inferência e GETs para a
  sonda `/health`. POSTs sem `Content-Type` recebem o padrão binário, mas um
  `Content-Type` explícito, como `application/json` no ciclo de vida da sessão,
  nunca é duplicado nem sobrescrito. `measure_rtt` mede a ida e volta pelo
  mesmo túnel Noise.
- `bitswap.rs` - codec libp2p request/response para um protocolo bitswap
  (manifesto + chunks com SHA256/MD5). Definido e ligado ao swarm, mas ninguém
  atende requisição (ver Defeitos).
- `discovery.rs` - swarm libp2p: gossipsub, Kademlia, mDNS, identify, relay
  (cliente e servidor), DCuTR, AutoNAT, bitswap, túnel de inferência. Põe peers
  no `Network` via `insert_peer` e reidrata no swarm os endereços dos peers vivos
  antes de enviar pelo túnel quando o anúncio chegou antes do registro de rota.
- `identity.rs` - chave Ed25519 de longa duração e o `node_id` (SHA256 da
  pubkey). Vive em `<project_root>/.pom-node.key` (ou `$POM_NODE_KEY`).
- `network.rs` - o coração do P2P UDP: `P2PMessage`, `PeerInfo`, socket
  dual-stack, gossip, lifecycle de peers, varredura de descoberta.
- `peer_client.rs` - `PeerClients`, um `reqwest::Client` por peer, porque o
  verifier do TLS é por destino. Sem TLS volta a ser um só. Guarda também o
  esquema global das URLs (`tls_ativo`/`peer_scheme`) e `peer_base_url`, que é
  quem colcheta IPv6, além da preferência e da quarentena temporária de rotas
  que falharam. O cliente destinado a outro nó marca requests de controle com
  `x-pom-internal-node` e limita a abertura TCP a 2 s; o cliente `plain()` para
  Ollama e loopback não envia essa marca.
- `pkarr_node.rs` - publicação/resolução no DHT pkarr: TXT `_pom` assinado,
  `publish`/`resolve_peers`.
- `tls.rs` - certificado auto assinado derivado da `.pom-node.key` e o
  `ServerCertVerifier` que confere `SHA256(pubkey) == node_id`. Nada vai para o
  disco; não há CA. Ver `docs/subsystems/rede-p2p.md`, seção 5.
- `tls_policy.rs` - a catraca: o campo `tls` do Announce é gatilho, e só a
  busca autenticada em `GET /api/cluster/tls` no principal grava a política.
  Busca que falha preserva o valor local.
- `route.rs` - `RouteTable`: qual endereço usar para cada peer, com sonda
  `/health`, histerese contra troca de rota por ruído e invalidação imediata
  quando o peer muda seus endereços anunciados. `RttTable` guarda a medição
  direcional deste nó para cada destino, sem inferir o sentido inverso.

## Funções-chave

`network.rs`:
- `Network::new` - bind dual-stack (`[::]`) montado à mão com `socket2` e
  `IPV6_V6ONLY` desligado **antes** do bind (depois não tem efeito; no Windows o
  padrão é 1 e um nó lá não veria peer IPv4 nenhum). Sem IPv6 no SO, cai para
  `0.0.0.0`.
- `Network::start` - spawna o receive loop, o loop de gossip (5 s: announce
  incondicional + `discovery_sweep` + `cleanup_dead_peers`) e a task de medição
  de rotas e RTT (30 s, sonda lenta não segura o gossip). A política TLS inicial
  é aplicada antes de `start`; a task sonda `/health` na porta de inferência de
  cada candidato e mede o RTT por libp2p quando o peer anuncia o túnel.
- `Network::handle_message` - Announce (reescreve `0.0.0.0` com o endereço de
  origem e chama `merge_observed`), PeerRequest (responde Announce imediato +
  `PeerList` fatiado), PeerList, Heartbeat, LocalDiscover. O `PeerList` também
  atualiza um peer já conhecido quando o snapshot é mais novo, preservando
  candidatos de endereço e estado local de verificação; snapshots antigos são
  ignorados para não reverter a cobertura. Nó destacado (`is_detached`) não
  aprende nem responde.
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
  `identify`, demais interfaces locais e o sentinela `0.0.0.0`. Bridges e pares
  virtuais de container não entram nos candidatos automáticos. Quando o
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
  anunciado) e registra os candidatos como multiaddresses `/p2p/<peer-id>`
  antes de aquecer a conexão do túnel; mDNS descobre na LAN e alimenta
  Kademlia; Identify grava listen_addrs e, para relays conectados, escuta no
  circuito (`P2pCircuit`); requisições recebidas pelo túnel são encaminhadas
  em tasks separadas para que um prefill, decode ou TP longo não pare o loop
  que drena GossipSub e as respostas dos outros protocolos; DCuTR/AutoNAT/
  Bitswap só logam.
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
- `RouteTable::measure_peer` - sonda `GET /health` na própria URL de inferência
  anunciada, não no gateway. Assim um gateway saudável não promove uma porta
  de inferência que engole SYN. A sonda aceita o certificado autoassinado e o
  hostname do peer; isso não altera o cliente pinado usado pela inferência.
  Timeout de 3 s e intervalo de 5 min.
- `RouteTable::mark_failure` - tira a rota de uso na hora quando uma requisição
  real falhou naquele endereço e memoriza a falha por 5 min. A próxima
  resolução procura outro candidato, em vez de voltar ao endereço morto antes
  de uma nova sonda bem-sucedida.
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

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
