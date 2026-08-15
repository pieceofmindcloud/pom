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
- `bitswap.rs` - codec libp2p request/response para um protocolo bitswap
  (manifesto + chunks com SHA256/MD5). Definido e ligado ao swarm, mas ninguém
  atende requisição (ver Defeitos).
- `discovery.rs` - swarm libp2p: gossipsub, Kademlia, mDNS, identify, relay
  (cliente e servidor), DCuTR, AutoNAT, bitswap. Põe peers no `Network` via
  `insert_peer`.
- `identity.rs` - chave Ed25519 de longa duração e o `node_id` (SHA256 da
  pubkey). Vive em `<project_root>/.rdma-p2p-node.key` (ou `$RDMA_P2P_NODE_KEY`).
- `network.rs` - o coração do P2P UDP: `P2PMessage`, `PeerInfo`, socket
  dual-stack, gossip, lifecycle de peers, varredura de descoberta.
- `pkarr_node.rs` - publicação/resolução no DHT pkarr: TXT `_rdmap2p` assinado,
  `publish`/`resolve_peers`.
- `route.rs` - `RouteTable`: qual endereço usar para cada peer, com sonda
  `/health` e histerese contra troca de rota por ruído.

## Funções-chave

`network.rs`:
- `Network::new` - bind dual-stack (`[::]`) montado à mão com `socket2` e
  `IPV6_V6ONLY` desligado **antes** do bind (depois não tem efeito; no Windows o
  padrão é 1 e um nó lá não veria peer IPv4 nenhum). Sem IPv6 no SO, cai para
  `0.0.0.0`.
- `Network::start` - spawna o receive loop, o loop de gossip (5 s: announce
  incondicional + `discovery_sweep` + `cleanup_dead_peers`) e a task de medição
  de rotas (30 s, sonda lenta não segura o gossip).
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
  assignment, para que nó configurado pela UI ainda seja visível.

`discovery.rs`:
- `Discovery::new` - constrói o swarm (porta TCP = UDP+10000), diala bootstrap
  convertido por `udp_to_libp2p_multiaddr` e os relays (padrão IPFS + `--relay-peers`).
- `handle_event` - Gossipsub Announce vira `PeerInfo` usando
  `addr_candidates.first()` (o literal `0.0.0.0` só sobrevive se nada foi
  anunciado); mDNS descobre na LAN e alimenta Kademlia; Identify grava
  listen_addrs e, para relays conectados, escuta na circuito (`P2pCircuit`);
  DCuTR/AutoNAT/Bitswap só logam.
- `udp_to_libp2p_multiaddr` - `host:port` UDP → multiaddr `/ip4`/`/ip6`/`/dns`
  na porta TCP. Conserta o bug de IPv6 que era colado num `/ip4/`, falhava o
  parse e era dialado sem nenhuma palavra no log.

`addrs.rs`:
- `filter_candidates` - descarta loopback/link-local/deprecated, estável antes
  de temporário, teto de 2 por família (o macOS cria vários IPv6 sozinho e o
  `PeerList` já está no limite da MTU); endereço fixado pelo operador vem
  primeiro e não conta para o teto.
- `local_addrs` - `ifconfig` (macOS) / `ip -o addr` (Linux); Windows vazio
  (cai no endereço observado pelo peer).

`identity.rs`:
- `Identity::load_or_create` - carrega a chave ou gera; regenerar a cada boot
  duplicava o peer na lista até o timeout e voidava `blocked_nodes` (chaveado
  por node_id). Corrompida, gera nova em vez de parar.
- `NodeId::from_pubkey` - SHA256 da pubkey; `Display` imprime os primeiros 12.

`route.rs`:
- `choose_route` - melhor candidato vivo; só troca de rota atual se o ganho
  passar de 25% relativo **e** 2 ms absolutos (medido 9,6 vs 7,2 ms
  MacBook↔ksia não pode trocar a cada ciclo).
- `RouteTable::measure_peer` - sonda `GET /health` na porta de inferência (o
  caminho que a inferência usa de verdade, com firewall no meio), timeout 3 s,
  intervalo 5 min.
- `RouteTable::mark_failure` - tira a rota de uso na hora quando uma requisição
  real falhou naquele endereço, sem esperar o ciclo.
- `RouteTable::resolve` - endereço escolhido para o node_id; `None` =
  "ainda não medido", e o chamador cai no `inference_url` do plano.

`pkarr_node.rs`:
- `PkarrNode::publish` - assina TXT `_rdmap2p` (JSON compacto, campos de uma
  letra para caber no limite DNS de ~1000 B) e publica no DHT.
- `resolve_peers`/`resolve_one` - resolve chaves `pk:` em `ResolvedPeer`
  (multiaddr + faixa de camadas + `is_relay`); falha vira `None`, nunca erro.
- `detect_local_ip` - IP de saída por socket UDP conectado a 8.8.8.8:80 (nada é
  enviado).

`bitswap.rs`:
- `BitswapCodec` - enquadramento `[4 bytes BE len][JSON]` sobre o
  request/response do libp2p.
- `bitswap_behaviour` - behaviour request/response para `BSWP_PROTOCOL`
  (`/rdma-p2p/bitswap/1.0.0`).

## Ligações

- **Chamado por**: `main.rs` (constrói `Network` + `Discovery`, resolve
  relays/pkarr, publica Announce no gossipsub a cada 5 s, diala `--pkarr-peers`),
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
- **Anúncio do gossipsub (main.rs) diverge do `self_announce` (network.rs)**:
  a task de publish do main.rs anuncia só `frame/v1-f16` e `frame/v2-zstd-f16`,
  sem `relay/v1-chain` nem `speculate/v1-lookup`, com `role` e `hostname`
  vazios, e só publica **quando existe assignment**. Peer que chegue só por
  gossipsub perde capacidades, papel e nome, e um nó sem modelo é invisível
  pela internet (o caminho UDP anuncia sempre). O comentário de `self_announce`
  fala em sincronizar "os outros dois builders" e não conta com este terceiro.
