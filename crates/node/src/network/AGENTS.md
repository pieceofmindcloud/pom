# network/ - protocolo P2P

Transportes de descoberta e endereçamento do nó. Dois mundos no mesmo diretório:
o UDP nativo (`network.rs`, `addrs.rs`, `identity.rs`, `route.rs`) e o
libp2p/pkarr para a internet (`discovery.rs`, `pkarr_node.rs`).
`main.rs` monta `Network` e `Discovery`; `gateway.rs` e `routing/pipeline.rs`
consomem `PeerInfo` e `RouteTable`. A regra da casa: a decisão de endereço
pertence a quem vai discar, não a quem anunciou. Identity is a key, never an
IP: the operator's `advertise_addr` is only a hint that goes in front of the
candidates (`addr_candidates` + `RouteTable` keep the fallback), and signed
documents (trust, public directory) carry `pk:` keys, never addresses. The
rules for when to use UDP, pkarr, libp2p and a direct route, and the ladder
direct -> dedicated relay (with DCuTR) -> member relay, are in
`docs/subsystems/p2p-network.md`, section 0.

## Arquivos

- `addrs.rs` - enumeração dos endereços locais por plataforma (`ifconfig` no
  macOS, `ip -o addr` no Linux) e a filtragem que decide o que anunciar.
  Captura também a interface dona do endereço para excluir bridges e pares
  virtuais de Docker, Podman, CNI e equivalentes sem perder IPs privados de
  interfaces reais da LAN. `preferred_local_advertise_ip` reaproveita essa
  filtragem para o wizard dizer se há IPv6 público (`public_ipv6_available`),
  preferindo IPv6 global estável e depois IPv4, sem confundir endereço de
  privacidade com endereço persistente. The wizard no longer pins that IP.
  `outbound_local_ip` is the outgoing IP (a UDP socket "connected" to
  `8.8.8.8:80`), a last resort for the node to dial itself. Único
  arquivo com código por plataforma; a filtragem fica fora do `cfg` e tem teste
  que roda em qualquer sistema.
- `infer_tunnel.rs` - protocolo request/response que carrega a chamada de
  inferência pelo enxame libp2p como transporte preferencial quando o peer
  anuncia essa capability. Se o túnel falhar por transporte, o relay usa o
  cliente direto pinado como fallback. `TunnelRequest` leva método, caminho,
  corpo e cabeçalhos, inclusive a `connection_key` e o marcador interno da
  porta de inferência;
  `serve_loopback` repassa a chamada para o loopback, sempre em texto claro
  (`serve::plain_allowed` libera loopback mesmo em TLS estrito e o certificado
  do nó não cobre `127.0.0.1`). `TunnelTarget` escolhe **qual** servidor local
  atende: `Inference` (padrão, e o único que um peer de binário anterior
  conhece) ou `Gateway`, o plano de controle. O emissor nomeia a variante; o
  receptor é que a mapeia para uma das duas portas que ele próprio escolheu -
  um peer remoto nunca nomeia porta. Para o gateway vale
  `gateway_path_is_allowed`, lista **fechada** e de comparação exata: admissão,
  políticas e retorno direto (`/handshake`, `/health`, `/api/cluster/*`,
  `/v1/relay-result`, `/v1/remote-stream-result`) e o plano de controle da
  implantação e das leituras do cluster (`/api/ui/{node,apply,...}`), plus
  administrative sync with the principal
  (`/api/internal/{api-keys,admin-trust,profile}/*`, all `InternalNodeAuth`). Tudo que
  sai do túnel chega ao gateway por `127.0.0.1`, e o gateway decide o que é
  local pelo endereço do socket; por isso `serve_loopback` marca o pedido com
  `TUNNEL_ORIGIN_HEADER` e o segredo do processo (`tunnel_mark`), e
  `serve::request_origin` o trata como remoto. Sem a marca, qualquer rota da
  lista daria a administração do nó (modo localhost, `install_trust_anchor`) a
  qualquer peer do swarm. O método do pedido é repassado como veio (GET, POST,
  ...). `peer_client::send_to_gateway_over` é o emissor do plano de controle: cada
  endereço anunciado e, por último, o túnel. POSTs sem `Content-Type` recebem o padrão binário, mas um
  `Content-Type` explícito, como `application/json` no ciclo de vida da sessão,
  nunca é duplicado nem sobrescrito. `measure_rtt` mede a ida e volta pelo
  mesmo túnel Noise.
- `cluster_logs.rs` - captura limitada do `tracing` e transporte request/response
  separado (`/pom/cluster-logs/1.0.0`); a fila descarta sob pressão, e cada
  lote é persistido localmente por todo admin efetivo e enviado por fanout aos
  demais peers anunciados como admin. `CaptureLayer` usa `INFO` como nível
  mínimo por padrão, configurável por `Config.cluster_logs_min_level`, sem
  alterar o filtro do console. Cada admin efetivo roda a poda em segundo plano:
  mantém sete dias e no máximo 500.000 linhas, em lotes de 256 com pausa e
  tentativa não bloqueante.
- `reqlog.rs` - protocolo request/response da auditoria de requisições
  (`/pom/request-audit/1.0.0`); o POM replica `begin` e `complete` para todo
  admin conhecido tanto quando o plugin grava localmente quanto no fallback.
  O receptor mantém a ordem dos eventos, entrega ao host do plugin ativo ou
  persiste no `pom.db` se ele não aceitar, sem retransmitir cópias recebidas.
- `cluster_logs_policy.rs` - catraca autenticada do interruptor de logs; o
  campo anunciado só dispara a leitura da política no principal.
- `network_chat.rs` - também transporta `ChatAction::PluginNotice`, o aviso
  que um plugin manda a todos os nós (`notification.notify` com
  `scope: "network"`): mesmo caminho autenticado e mesmo limite de taxa das
  mensagens, mas guardado só em memória (50 avisos, 24 h, `notice_snapshot`),
  nunca em `network_chat_messages`.
- `network_chat.rs` - canal de conversa sobre o swarm libp2p, com request/
  response próprio, limites de texto/arquivo, fila limitada, rate limit e
  writer de persistência fora do loop do swarm. A ação original leva a API key;
  retransmissões internas levam apenas o frame já validado.
- `network_chat_policy.rs` - catraca autenticada do interruptor do Chat da
  rede; o booleano anunciado é gatilho e a leitura autenticada no principal é
  a fonte efetiva.
- `discovery.rs` - swarm libp2p: gossipsub, Kademlia, mDNS, identify, relay
  (cliente e servidor), DCuTR, AutoNAT, túnel de inferência e os protocolos de
  logs do cluster, auditoria de requisições e Chat da rede. A auditoria recebida
  entra numa fila limitada fora do loop do swarm e segue para o plugin ativo ou
  o fallback local. Põe peers no `Network` somente
  depois da catraca de admissão, que no gossipsub nunca sonda dentro do laço
  do swarm (`admission_without_probe`, e sem veredito `admit_in_background`):
  a sonda cai no túnel, que esse laço atende. Reidrata no swarm os endereços dos peers
  admitidos antes de enviar pelo túnel quando o anúncio chegou antes do
  registro de rota. A assinatura nativa do gossipsub prova a origem do
  transporte, mas não substitui a prova de pertencimento do enxame.
- `identity.rs` - the node's **only** Ed25519 key and the `node_id` (SHA256 of
  the pubkey). It lives in `<project_root>/.pom-node.key` (or `$POM_NODE_KEY`).
  The `PeerId` (`libp2p_keypair`/`peer_id`), the pkarr key and the authority
  key also come from it; `Announce`/`PeerInfo` have a single `pubkey`.
- `network.rs` - o coração do P2P UDP: `P2PMessage`, `PeerInfo`, socket
  dual-stack, gossip, lifecycle de peers, varredura de descoberta e admissão.
  A `connection_key` não vai no `Announce`: em rede privada, o handler faz
  uma sonda no `/handshake` antes de registrar o peer. Em rede pública o rigor
  segue o `network.access_policy`: com `authorized` (padrão) o `cluster_id` só
  seleciona a cadeia e a tupla `{node_id, peer_id, pubkey}` precisa ser uma
  concessão ativa em `admin_trust.rs`; com `anyone_with_anchor` basta uma
  cadeia local ancorada exatamente nesse `cluster_id`
  (`AdminTrust::anchors_cluster`) mais o vínculo de identidade, sem concessão -
  é o que admite um worker co-locado sem grant. Campo `profile` nunca concede
  acesso. `PeerList` é revalidado, e `connection_verified` só é escrito pelo
  gate local. O perfil anunciado por `with_identity` pode ser
  atualizado por `set_profile`; a capacidade de admin não vem do anúncio, mas
  da cadeia em `admin_trust.rs`. O campo `idle` também é anunciado por peer
  para distinguir modelo descarregado de deployment fora do ar; ele não altera
  a regra de liveness.
- `local_discovery.rs` - handshake UDP temporário da configuração inicial e
  preferência administrativa de descoberta local. O admin anuncia somente o
  fingerprint e os metadados de tamanho do documento, recebe um `Proof` ligado
  ao desafio efêmero do nó e só então envia o documento junto da
  `connection_key` cifrada. O payload autenticado é fragmentado em blocos que
  cabem no orçamento de 1200 bytes do UDP e remontado antes da validação do
  fingerprint. A preferência fica desligada fora do wizard; o material sensível
  vive apenas em memória e a transferência é limitada a 512 KiB. O `Probe`
  leva `DiscoveredMachine` (nome, sistema, CPUs, RAM, GPUs/VRAM; limpo e
  limitado por `sanitized`) para o wizard do admin mostrar quem procura a
  rede: o peer entra em `peers` no `Probe` com `joined: false` e vira
  `joined: true` ao receber a raiz.
- `peer_client.rs` - `prefer_tunnel`/`remember_tunnel_win`/`remember_direct_win`
  são a marca **por peer** de que o caminho direto não responde: ligada, o
  frame pula QUIC e HTTP na primeira tentativa. Aprendida, não configurada -
  um sucesso direto a apaga. `relay.rs` a consulta nas duas escadas de envio.
  `TunnelPeer`/`tunnel_gateway_call` levam uma chamada do
  plano de controle pelo túnel quando nenhum endereço direto responde; é o que
  faz a admissão de um par atrás de NAT concluir.
  `get_internal_from_candidates` tenta os endereços diretos e só então o
  túnel, e devolve `InternalResponse` (status + corpo já lido), porque o túnel
  não produz um `reqwest::Response`.
  `PeerClients`, um `reqwest::Client` por peer, porque o
  verifier do TLS é por destino. Sem TLS volta a ser um só. Guarda também o
  esquema global das URLs (`tls_ativo`/`peer_scheme`) e `peer_base_url`, que é
  quem colcheta IPv6, além da preferência e da quarentena temporária de rotas
  que falharam. `route_order` apenas reordena os candidatos: mesmo quando
  todos estão em quarentena, devolve a lista para que a tentativa real decida.
  `get_internal_from_candidates` percorre todos os gateways anunciados para
  uma rota de controle autenticada; inclusive 401/403 fazem a sonda tentar o
  próximo endereço, porque um endereço fantasma pode responder como outro nó.
  O cliente destinado a outro nó marca requests de controle com
  `x-pom-internal-node`, usa a `connection_key` compartilhada também no fallback
  HTTP de inferência em rede pública e limita a abertura TCP a 2 s; HTTPS e
  QUIC podem validar pelo transporte seguro e o mesmo marcador. O cliente `plain()` para
  Ollama e
  loopback não envia essa marca. `for_peer_with_state` informa ao trace se o
  cliente TLS foi criado ou reutilizado; transporte local, túnel e cliente
  sem TLS permanecem explicitamente desconhecidos. `effective_quic_enabled`
  combina a flag do pool com a preferência global confirmada pelo principal,
  para que clientes lazy criados antes da propagação não desviem inferência
  para o túnel. Todos os clientes HTTP de peer ativam `TCP_NODELAY`, inclusive
  as sondas e clientes TLS, para que requests pequenos de decode não combinem
  Nagle com delayed ACK do sistema operacional. O `reqwest::Client` compartilhado
  e o cache por `node_id` também preservam o pool de conexões; `for_peer` não
  cria um cliente novo a cada frame.
- `quic.rs` - o orçamento de estabelecimento de cada chamada sai de
  `route_plan::book().establish_budget(peer)` (teto `QUIC_FALLBACK_BUDGET`);
  `ClientPool::for_probes` fixa o orçamento da sonda, que é quem mede o RTT.
  `ensure_local_route` faz um `connect` de UDP antes do handshake e falha na
  hora quando a máquina não tem rota para a família do destino (container sem
  IPv6 discando um IPv6 anunciado) - o quinn só registrava o `sendmsg` recusado
  e esperava o orçamento inteiro.
- `quic.rs` - cliente e servidor HTTP/3 sobre Quinn. O pool mantém uma conexão
  QUIC por peer e endereço anunciado, usa o mesmo pinning Ed25519 da conexão
  TCP e deixa o endpoint UDP pronto na porta dedicada anunciada
  (`inference_quic_port`/`gateway_quic_port`) mesmo quando a preferência está
  desligada. O dial exige `host:porta` e nunca deriva o destino de uma URL HTTP.
  A configuração do listener é independente do `tls_required` do TCP, porque
  QUIC leva seu próprio TLS 1.3. Cada resposta também expõe o RTT suavizado da
  conexão Quinn em microssegundos, separado do processamento do handler remoto.
  `request_body_send_us` mede a espera por `send_data` e `finish` do corpo
  HTTP/3; não é o tempo de resposta nem uma confirmação de entrega pelo peer.
  Cada tentativa completa, do handshake ao corpo da resposta, tem um só
  prazo: o prazo do frame (`relay::frame_deadline`: (1 s + 50 ms por token + 2 s por
  imagem) x hops restantes, fixado em `send_frame_or_tunnel` e lido pelos
  transportes com `relay::frame_time_left`). Não há teto menor aninhado; peer que caiu é detectado pela
  vida da conexão (`QUIC_MAX_IDLE` 1,2 s, keep-alive TCP de ~4 s em
  `peer_client::with_liveness`). As memórias de falha (rota, quarentena QUIC,
  abertura do canal persistente, admissão) usam a janela única
  `peer_client::FAILURE_MEMORY` (5 s). Um peer em `tunnel_first` tem o caminho
  direto sondado em segundo plano a cada `DIRECT_REPROBE_INTERVAL` (5 s,
  `relay::spawn_direct_reprobe`); sucesso apaga a marca.
  A descoberta de um endpoint QUIC morto compartilha um orçamento agregado de
  300 ms entre endereços, e a falha põe aquele endereço em quarentena por 5 s
  para não repetir handshake a cada token; a sessão saudável é reutilizada.
  Se uma requisição falha numa sessão cacheada, como no `ApplicationClose`
  causado pelo reinício do peer, o pool fecha e remove aquela instância e tenta
  uma vez um handshake novo dentro do mesmo orçamento. A quarentena só registra
  a falha final do endpoint. A remoção compara a instância cacheada para não
  apagar uma conexão nova criada concorrentemente.
- `pkarr_node.rs` - key -> address resolver on the pkarr DHT: a signed `_pom`
  TXT with public multiaddresses and the keys of the relays holding a
  reservation; `publish`/`resolve`/`resolve_dial_addrs`. The live publisher is
  `gateway::PkarrPublishHandle`.
- `public_directory.rs` - protocolo federado do catálogo público. Registros
  completos são assinados pelo principal da rede, têm prova de trabalho de 16
  bits, sequência monotônica, TTL de sete dias e merge determinístico. O cache
  local aceita no máximo 1024 registros; réplicas são somente ingressos HTTP(S)
  configuráveis e não autoridade. O registro assinado inclui descrição,
  regras e canais em Markdown junto do convite, e `relay_hints` (`pk:` keys
  only). The record carries no address: the network is found by the
  publisher's key. Um nó privado consulta esses ingressos por saída, mas não usa seu
  swarm privado como descoberta pública.
- `tls.rs` - certificado auto assinado derivado da `.pom-node.key` e o
  `ServerCertVerifier` que confere `SHA256(pubkey) == node_id`. Nada vai para o
  disco; não há CA. Ver `docs/subsystems/p2p-network.md`, seção 5.
- `tls_policy.rs` - a catraca: o campo `tls` do Announce é gatilho, e só a
  busca autenticada em `GET /api/cluster/tls` no principal grava a política.
  Busca que falha preserva o valor local.
- `quic_policy.rs` - catraca autenticada da preferência QUIC: o campo `quic`
  do Announce só dispara `GET /api/cluster/quic`; o gatilho só é aceito quando
  `pubkey` identifica o principal ancorado, nunca por `profile: admin`;
  apenas a resposta do principal é adotada e falha preserva o valor local.
- `cluster_logs_policy.rs` aplica a mesma disciplina ao campo
  `cluster_logs_enabled`: anúncio é gatilho, leitura autenticada no principal
  é a fonte, e falha preserva o valor local. A lista de admins para réplica é
  independente dessa catraca e não torna um secundário capaz de mudar a
  política.
  As consultas de TLS, logs e Chat recebem a lista de gateways candidatos do
  principal; esse é o único caminho para manter o fallback de endereço e a
  política TLS da sonda alinhados.
- `network_chat_enabled` segue o mesmo contrato. O gatilho só é aceito como
  decisão do principal cuja `pubkey` foi confirmada pelo
  `AdminTrust`; o perfil anunciado não identifica autoridade. O interruptor
  aberto num admin secundário encaminha a mutação ao principal e só a gravação
  dele muda a política autoritativa. A retransmissão do Chat da rede é estrela:
  o principal valida API key ativa e `blocked_nodes` antes de enviar a outros
  `node_id`.
- `NetworkSettings` tem a mesma fronteira de autoridade, mas com leitura
  explícita: depois do primeiro `Announce` autenticado do principal,
  `Network::handle_message` acorda a catraca de `GET /api/cluster/network-settings`.
  O gateway assina a consulta, persiste o documento completo no nó read-only e
  nunca envia a `connection_key` quando a rede é pública. O subconjunto do
  diretório público continua sendo apenas convite e descoberta, não fonte de
  verdade dos metadados.
- `route.rs` - `RouteTable`: qual endereço usar para cada peer, com sonda
  `/health`, histerese contra troca de rota por ruído e invalidação imediata
  quando o peer muda seus endereços anunciados. Guarda também a **memória de
  falha por `(node_id, URL)`**, que serve tanto ao pipeline quanto ao plano de
  controle - ver `docs/subsystems/p2p-network.md`, "Endereço que deixou de
  responder". `RttTable` guarda a medição direcional deste nó para cada
  destino, sem inferir o sentido inverso, e expõe snapshots com idade: só os
  RTTs com menos de cinco minutos entram em seleção.
  `last_success` devolve o último endereço HTTP que respondeu (há menos de
  5 min e sem falha depois), a metade HTTP de `Network::peer_route`.
- `route_plan.rs` - o **plano de rotas por aresta**: por qual transporte (QUIC,
  HTTP/HTTPS ou túnel libp2p) este nó fala com cada peer, medido em segundo
  plano e fixado. `RouteBook` (global do processo, `route_plan::book()`) guarda
  por `(node_id, Plane)` a lista de rotas medidas em ordem de RTT (empate de
  2 ms decide QUIC, depois HTTP, depois túnel). Dois planos por peer:
  `Inference` (prefill/decode) e `Gateway` (volta da resposta direta e do
  remote-stream). `relayed_nodes` lista os peers alcançados só por circuito
  num relé, refeita a cada volta do planejador a partir de
  `Network::libp2p_links`. `spawn_planner`, iniciado por `Network::start`, mede toda
  aresta sem análise, vencida (5 min) ou cujo peer mudou endereços, portas,
  `deploy_id` ou faixa de camadas - por isso a implantação que fica `serving` já
  dispara a análise antes da primeira requisição. `measure_edge` sonda em
  paralelo com o transporte real: `GET /health` por HTTP com o cliente pinado,
  POST HTTP/3 por endpoint QUIC (qualquer status prova o caminho) e `/health`
  pelo túnel; duas amostras, vale a menor. `record_failure` repete a rota uma
  vez, depois passa para a próxima medida e, com a lista esgotada, apaga a
  aresta e acorda o planejador (reanálise). `establish_budget` dá o orçamento de
  conexão: 3x o RTT medido entre 100 ms e o teto, 150 ms sem medição.
  `snapshot`/`signature` alimentam o campo `route_plan` de `/api/ui/cluster`.
- `Network::gateway_bases` - os endereços do gateway de um peer na ordem em
  que vale tentar: o host que já respondeu (`RouteTable::last_success`,
  comparado pelo host porque pode ter sido a porta de inferência), depois os
  que não falharam, depois os desacreditados. Quem só pode usar um endereço (um
  download, o `from_gateway` de uma oferta) usa o primeiro.
  `inference_bases` faz o mesmo com os endereços de inferência (a sonda de
  admissão ao plano), e `order_gateway_bases` ordena uma lista montada por
  quem chama (as catracas de política, com o esquema anunciado).
- `Network::peer_route` / `libp2p_links` - a rota em uso com cada peer para a
  tela de participantes: HTTP direto que respondeu, conexão libp2p direta
  (marcada quando nasceu de hole punch) ou só circuito pelo relé. As conexões
  vêm de `note_libp2p_link`/`drop_libp2p_link`/`mark_hole_punched`, chamados
  pelo laço do swarm em `discovery.rs`; ver `p2p-network.md`, "Diagnóstico".

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
  (1200 B, o piso que o IPv6 garante; a 5G do operador tem MTU 1450 e IPv6 não
  fragmenta).
- `advertised_addr` - normaliza endereço digitado à mão (IPv6 puro precisa de
  colchetes para ganhar porta) em `host:port`.
- `detach`/`rejoin` - tirar/voltar o nó da rede sem derrubar a UI/API
  (`--no-discovery`); rejoin refaz bootstrap + announce.
- `self_announce` - monta o Announce; anuncia zero camadas quando não há
  assignment, para que nó configurado pela UI ainda seja visível. Relê as
  interfaces uma vez a cada anúncio, a cada 5 s no gossip e também em anúncios
  imediatos; é uma leitura local de `ifconfig`/`ip`, sem sonda de rede. O mesmo
  snapshot alimenta `addr` e `addr_candidates`. The `advertise_addr` hint, when
  present, goes first and **does not replace** the rest; then come, in UDP and
  gossip: the interfaces' current public address with stable before temporary,
  the address observed by `identify`, the other local interfaces and the
  `0.0.0.0` sentinel. Bridges e pares virtuais de container não entram nos candidatos
  automáticos. Quando o
  servidor de inferência está carregado, o anúncio também inclui
  `inference/remote-token-stream-v1`, que permite ao gateway remoto pedir o
  laço de decode com callbacks em lotes limitados. O mesmo anúncio leva
  `transport/link-v1`, a ligação persistente entre andares (`/v1/link`), que é
  servida pelo mesmo router das demais rotas `/v1`. A capability
  `inference/frame-v1-batch` só entra quando o servidor de inferência carregou
  um modelo e o scheduler multi-item está ativo; setup, TP, carregamento e
  queda não anunciam capacidade de inferência. Capabilities separadas anunciam
  f8, i4g64 e i4g128; f16 continua implícito e padrão. `runtime_capacity` só entra
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
- `Discovery::new` - constrói o swarm com TCP **e** QUIC na porta UDP+10000
  (quatro listen addresses: v4/v6 x tcp/quic-v1), diala bootstrap convertido
  por `udp_to_libp2p_multiaddr` e os relés de `--relay-peers`. QUIC está ali
  para o DCuTR: hole punch em TCP depende de simultaneous open, que CGNAT
  recusa. `DEFAULT_RELAY_NODES` está vazio - os bootstrappers do IPFS que
  estavam lá não concedem reserva de Circuit Relay v2 e os endereços já eram
  obsoletos, então nenhuma reserva era aceita e o DCuTR nunca rodava.
- `handle_event` - Gossipsub Announce vira `PeerInfo` usando
  `addr_candidates.first()` (o literal `0.0.0.0` só sobrevive se nada foi
  anunciado) e registra somente o endpoint canônico como multiaddress
  `/p2p/<peer-id>` antes de aquecer a conexão do túnel; mDNS discovers on the
  LAN and is always on. Identify does not write `listen_addrs` into the address
  book; a peer announcing `relay::HOP_PROTOCOL_NAME` goes through
  `reserve_circuit` if it is a dedicated relay or if
  `RelayPolicy::members_allowed`, otherwise it waits in `pending_members`;
  AutoNAT `Public` becomes `Swarm::add_external_address`; `ListenerClosed` ends
  the reservation of the relay owning the listener; requisições recebidas pelo túnel são encaminhadas
  em tasks separadas para que um prefill, decode ou TP longo não pare o loop
  que drena GossipSub e as respostas dos outros protocolos; DCuTR/AutoNAT
  só logam o resto. Os request/responses de logs, auditoria e Chat da rede usam
  behaviours próprios, separados do túnel de inferência. Logs de processo são
  aceitos por todo admin efetivo; eventos de auditoria também são persistidos
  por todo admin efetivo e nenhum evento recebido é retransmitido. O Chat valida no principal,
  retransmite sem segredo e recusa relays que não vieram de um administrador.
  O `select!` é biased para o swarm e o túnel de inferência; filas de Chat usam
  `try_send`, descartam sob pressão e nunca esperam SQLite.
- `tunnel_peer_multiaddrs` - endereços discáveis de um peer. Aceita
  multiaddress completo (o que `Announce.libp2p_addrs` leva, inclusive
  circuito) e o `host:port` legado, derivado por `udp_to_libp2p_multiaddr`. O
  `/p2p/<peer-id>` final entra só quando falta: num circuito o relé já aparece
  antes do `/p2p-circuit`, e duplicar o sufixo faria discar o relé no lugar do
  destino.
- `udp_to_libp2p_multiaddr` - `host:port` UDP → multiaddr `/ip4`/`/ip6`/`/dns`
  na porta TCP. Conserta o bug de IPv6 que era colado num `/ip4/`, falhava o
  parse e era dialado sem nenhuma palavra no log.
- `TraversalStatus`/`traversal_status` (em `network.rs`) - estado do AutoNAT,
  endereços publicados, reservas aceitas e contagem de hole punches com a
  última falha. `discovery.rs` alimenta (`set_nat_status`,
  `remember_reservation`, `remember_hole_punch`) e `/api/ui/cluster` publica no
  campo `traversal`, que também entra no fingerprint do cache.
- `publish_swarm_addrs` - recolhe do swarm endereços externos confirmados,
  listeners com IP público e listeners de circuito, acrescenta `/p2p/<self>` e
  grava em `Network::set_libp2p_addrs`. Privado/loopback/não especificado fica
  de fora; circuito entra sempre, porque o IP ali é o do relé. Chamado em
  `NewListenAddr`, `ExpiredListenAddr`, `ExternalAddrConfirmed`,
  `ExternalAddrExpired`, AutoNAT `Public` e reserva aceita.
- `RelayPolicy`/`apply_relay_policy` - the relay ladder: dedicated (key in
  `relay_peers`) first; member (another node with `--serve-relay`) only when no
  dedicated relay holds a live reservation, and after `DEDICATED_RELAY_GRACE`
  (15 s) when a dedicated one is configured. Every 5 s the loop reserves on
  pending members, undoes member reservations when a dedicated relay comes back
  and asks connected dedicated relays without a reservation again.
  `Network::set_dedicated_relays` feeds `PeerRoute.relay_kind`.
- `reserve_circuit` - asks `listen_on(<relay>/p2p-circuit)` and keeps the
  `ListenerId`s in `RelayPolicy::listeners`. Endereço do relé
  vem de `relay_reservation_addrs`: lista configurada, anúncio vivo e, por
  último, o endereço que este nó discou (`dialed_addrs`, anotado no
  `ConnectionEstablished`) - é o caso do relé dedicado fora da rede. Nunca dos
  `listen_addrs` do identify. Sem circuito publicado não há conexão relayed, e sem ela o DCuTR
  não tem por onde coordenar o punch.
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
- `Network::effective_host` (via `choose_effective_host`) - host pelo qual o
  nó se descreve: fixado, depois o público atual da interface, depois o
  observado. Sempre devolve IPv6 **entre colchetes**, porque quem o usa
  concatena `:porta` (a entrada do próprio nó em `all_peers`, que a tela de
  participantes mostra) ou o passa por `ip_host` (`self_reply_hosts`). O
  endereço da interface vem cru de `filter_candidates` e é colchetado ali.
- `is_private_or_local` - decide se um endereço observado por um peer é notícia
  sobre o IP público deste nó. Além de RFC 1918 e `fc00::/7`, recusa
  **`100.64/10` (RFC 6598)**: é a faixa do CGNAT e é a que a Tailscale usa.
  Medido em 2026-08-16 num notebook: a primeira resposta de `identify` veio por
  um peer alcançado pelo tailnet e reportou um `100.x.y.z`, que o nó adotou
  como endereço *público* e anunciou ao cluster inteiro.
- `local_addrs` - `/sbin/ifconfig` (macOS) / `ip -o addr` (Linux); macOS usa
  caminho absoluto porque processos iniciados pela GUI podem não ter `/sbin` no
  `PATH`. Windows vazio (cai no endereço observado pelo peer). Se o programa
  não puder ser executado, `run` avisa uma vez no log e devolve vazio: o nó
  então não anuncia endereço próprio. Por isso a imagem Docker instala
  `iproute2`.

`identity.rs`:
- `Identity::load_or_create` - carrega a chave ou gera; regenerar a cada boot
  duplicava o peer na lista até o timeout e voidava `blocked_nodes` (chaveado
  por node_id). Corrompida, gera nova em vez de parar.
- `NodeId::from_pubkey` - SHA256 da pubkey; `Display` imprime os primeiros 12.
- `key_path` - `.pom-node.key`, resolved against `config::project_root()`,
  overridable with `$POM_NODE_KEY`. It does not come from the cwd: launching the
  binary from another directory would create a second identity. There is no
  separate pkarr key.
- `Identity::libp2p_keypair` / `peer_id` - the same seed as a libp2p identity.

`route.rs`:
- `choose_route` - melhor candidato vivo; só troca de rota atual se o ganho
  passar de 25% relativo **e** 2 ms absolutos (medido 9,6 vs 7,2 ms
  notebook↔servidor CUDA não pode trocar a cada ciclo).
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
- `RouteTable::endpoint_is_discredited` - compara host e porta ao filtrar
  candidatos herdados. Falha HTTP no gateway ou na inferência não desacredita a
  porta P2P/UDP do mesmo host; isso evita remover um endereço de tunnel vivo.
- `RouteTable::resolve` - endereço escolhido para o node_id; `None` =
  "ainda não medido ou vencido", e o chamador cai no `inference_url` do plano.
- `RttTable` - guarda `target_node_id`, `rtt_ms` e `transport` localmente;
  `libp2p` é a medição normal e `direct-health` identifica o fallback de sonda.
  `fresh_snapshot` é a única fonte de RTT para seleção; `snapshot_with_age`
  conserva a idade para diagnósticos. A renovação acontece a cada 120 s,
  independentemente da re-medida de endereçamento de 5 min. Se o peer anuncia
  o túnel mas o swarm ainda não está pronto, a sonda HTTP da rota de inferência
  continua sendo um fallback `direct-health`, em vez de apagar a medição.
  O endpoint autenticado
  `/api/ui/network/rtt` publica só medições frescas com `source_node_id`.

`pkarr_node.rs`:
- `PkarrNode::from_signing_key` - client over the node key; there is no
  separate key file.
- `PkarrNode::publish(&PkarrRecord)` - signs the `_pom` TXT (`a`
  multiaddresses without `/p2p/<self>`, `x` relays in z-base-32, `h`, `i`, `r`,
  `g`), trimmed from the end until it fits in 900 B. The `g` field carries the
  geo label as metadata, without changing the key.
- `resolve`/`resolve_peers`/`resolve_public_peer` - a `pk:` or hex key into
  `ResolvedPeer { peer_id, record }`; failure becomes `None`, never an error.
  This does not enumerate the DHT: `/api/public/networks` only resolves
  candidates that already arrived through UDP/libp2p or sit in the local cache.
  The Mainline DHT used by pkarr keeps an actor on its own thread and uses UDP
  IPv4; on Windows its 50 ms read timeout becomes WSAETIMEDOUT (10060), an
  expected idle result the binary's log filter downgrades without hiding real
  errors.
- `dial_addrs`/`resolve_dial_addrs` - direct ones first, then
  `<relay>/p2p/<relay>/p2p-circuit/p2p/<peer>` per published relay.
- `peer_id_from_uri`/`uri_from_peer_id` - `pk:` key <-> `PeerId` (the same
  Ed25519 key).

## Ligações

- **Chamado por**: `main.rs` (constrói `Network` + `Discovery`, resolve
  relays/pkarr, publica no gossipsub a cada 5 s o mesmo anúncio do UDP, via
  `Network::gossip_announce_bytes` -> `self_announce`, com ou sem assignment;
  `spawn_pkarr_bootstrap` dials `--pkarr-peers`, relays and trust chain
  principals by key),
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
