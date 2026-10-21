# network/ - the P2P protocol

The node's discovery transports and addressing. Two worlds in the same
directory: the native UDP (`network.rs`, `addrs.rs`, `identity.rs`, `route.rs`)
and the iroh/pkarr for the internet (`p2p.rs`, `discovery.rs`, `pkarr_node.rs`).
`main.rs` assembles `Network` and `Discovery`; `backend/gateway/` and
`routing/pipeline.rs` consume `PeerInfo` and `RouteTable`. The house rule: the
address decision belongs to whoever is going to dial, not to whoever announced.
Identity is a key, never an IP: the operator's `advertise_addr` is only a hint
that goes in front of the candidates (`addr_candidates` + `RouteTable` keep the
fallback), and signed documents (trust, public directory) carry `pk:` keys and
relay URLs with host names, never IPs. The rules for when to use UDP, pkarr, the
iroh endpoint and a direct route, and the ladder direct -> relay (n0's,
dedicated or member, with iroh's hole punching), are in
`docs/subsystems/p2p-network.md`, section 0.

## Files

- `addrs.rs` - enumeration of the local addresses per platform (`ifconfig` on
  macOS, `ip -o addr` on Linux) and the filtering that decides what to announce.
  It also captures the interface owning the address, to exclude bridges and
  virtual pairs of Docker, Podman, CNI and equivalents without losing private IPs
  of real LAN interfaces. `preferred_local_advertise_ip` reuses that filtering so
  the wizard can say whether a public IPv6 exists (`public_ipv6_available`),
  preferring a stable global IPv6 and then IPv4, without confusing a privacy
  address with a persistent one. The wizard no longer pins that IP.
  `outbound_local_ip` is the outgoing IP (a UDP socket "connected" to
  `8.8.8.8:80`), a last resort for the node to dial itself. The only
  file with per-platform code; the filtering stays outside the `cfg` and has a
  test that runs on any system.
- `p2p.rs` - the iroh transport pieces: the endpoint id is the node key
  (`secret_key`, `parse_endpoint_id` takes hex, z-base-32 or `pk:`); the textual
  address form (`ip:port` or a relay URL: `endpoint_addr`, `addr_texts`,
  `relay_url`, `relay_hint` for signed documents, host names only); the `Codec`
  trait every request/response protocol implements (one ALPN each, one QUIC
  bidirectional stream per call, `call_on`/`serve_connection`/`Serve`); `P2p`,
  the outbound side (one connection per peer and ALPN, the `MemoryLookup`
  address book, `DialBackoff`: 250 ms doubling to 10 s after a failed dial);
  `LinkPath`/`selected_path`/`link_path`, the path a connection uses now, for
  the screens and the file rule; `is_default_relay` (n0's relays); and
  `TUNNEL_CAPABILITY` and `P2P_PORT_OFFSET` (10000).
- `infer_tunnel.rs` - the request/response protocol that carries a call through the p2p
  endpoint as the preferred transport, with the pinned direct client as fallback when it fails on
  transport. `TunnelRequest` carries method, path, body and headers, including the
  `connection_key` and the inference port's internal marker; the receiver adds the
  PeerId Noise authenticated (`TUNNEL_PEER_HEADER`), which `serve::request_origin`
  believes only beside the process mark; the frame is binary
  (`[u32][JSON head][u32][raw body]` for a request, `[u16 status][u32][raw body]` for a
  response). `TunnelTarget` (`Inference` | `Gateway`) names which local server answers and the
  receiver resolves the port. `is_transfer_data_path`, `closed_range` and
  `transfer_chunk_is_bounded` hold the file rule on the receiving side (a piece is a bounded
  range, refused with `TRANSFER_ROUTE_REFUSED_STATUS` 421 on a connection that crosses a public
  relay - the receiver checks the connection's selected path); `gateway_path_is_allowed` is the closed exact-comparison list, plus the plugin calls in
  their fixed shape (`plugin_path_is_allowed`: services, `<code>/peer/<op>`, `<code>/engine/generate`); `serve_loopback` hands
  the call to the gateway over `127.0.0.1` and marks it with `TUNNEL_ORIGIN_HEADER` and
  `tunnel_mark` so `serve::request_origin` never reads it as local. `peer_client::send_to_gateway_over`
  is the control plane's sender: each announced address and, last, the tunnel. `measure_rtt`
  measures the round trip through the same tunnel. `live_probe_sender` (tests) binds a
  probe endpoint for the `#[ignore]` live probes, driven by `POM_TUNNEL_PROBE=<endpoint-id>@<addrs>`. The protocol, the security invariants
  and the frame format are `docs/subsystems/p2p-network.md`, "A request that comes out of the
  tunnel is never local" and "The tunnel frame is binary".
- `cluster_logs.rs` - bounded `tracing` capture and a separate request/response
  transport (`/pom/cluster-logs/1.0.0`); the queue discards under pressure, and
  each batch goes by fanout to every admin `Network::admin_peers` returns: a
  live peer announced as admin that the trust chain also proves one
  (principal or a secondary in force, never a recorded inference node). `CaptureLayer` uses
  `INFO` as the minimum level by default, configurable by
  `Config.cluster_logs_min_level`, without changing the console's filter. Storing
  the lines is the logs plugin's (`pom-plugin-logs`): an effective admin hands
  each accepted batch to it (`cluster_logs` ingest event, one task, bounded
  queue) and the console reads through `cluster_logs.list`; storage, order of
  arrival and retention (seven days, 500,000 lines) live in the plugin. An
  admin without the plugin active drops the batches, so only the admins need
  it. `ClusterLogs::list` returns `None` without the plugin.
- `reqlog.rs` - the request audit's request/response protocol
  (`/pom/request-audit/1.0.0`); the POM replicates `begin` and `complete` to every
  known admin both when the plugin records locally and in the fallback. The
  receiver keeps the events' order, hands them to the active plugin's host or
  persists them in the `pom.db` if it does not accept, without retransmitting
  received copies.
- `cluster_logs_policy.rs` - the authenticated ratchet of the logs switch; the
  announced field only triggers reading the policy on the principal, which
  every admin can change (a secondary forwards `POST /api/ui/network/logs`).
- `network_chat.rs` - also transports `ChatAction::PluginNotice`, the notice a
  plugin sends to every node (`notification.notify` with
  `scope: "network"`): the same authenticated path and the same rate limit as the
  messages, but kept only in memory (50 notices, 24 h, `notice_snapshot`), never
  in `network_chat_messages`.
- `network_lobby.rs` - the lobby: the `/pom/network-lobby/1.0.0` protocol a machine that is
  **not** a member uses to talk to a public network's principal before joining. Frame types and
  bounds (`LobbyLimits`), the self-declared hardware card (`LobbyCard`, clamped by `sanitized`),
  `transport_key` and `LobbyIdentity` (a key derived from the node key, proving the node
  with a signature over that transport endpoint id - never the node's own endpoint id, since
  two endpoints under one key knock each other off the relays),
  `LobbyGuests` (presence plus the per-candidate rate) and `LobbyClient`, the candidate's
  dial-only endpoint (ephemeral port, n0's relays) that also runs in setup mode. The principal's handler is
  `discovery.rs::answer_lobby_request`; inside the network the lobby is the `lobby` room of
  `network_chat.rs` (`ChatAction::LobbyPresence`, `NetworkChat::{lobby_guests, lobby_enabled,
  auto_approves}`). `Info` and `PublicStats` publish models, node count and VRAM under their own
  switch (`NetworkChat::{public_stats, public_stats_enabled}`), filled by
  `webui::spawn_public_stats_publisher`, never by the lobby handler. `Describe` and `Directory` are
  answered by any node from `DirectoryBoard` (own public network; known public networks when the
  node is a relay or in a public network), filled by `gateway::spawn_directory_board_publisher`;
  `Publish` lets a network's principal hand its description to a relay over a direct dial
  (`DirectoryBoard::accept_publication`) - the listing path that needs no relay.
  `default_directory_relays` is the relay list a fresh installation asks (`DEFAULT_DIRECTORY_RELAYS`,
  `POM_DIRECTORY_RELAYS`). Admission lives here too: `LobbyRequest::Admission`,
  `AdmissionRecord` / `AdmissionView` / `AdmissionGrant`, `apply_admission_request` (pending is
  refreshed, approved is kept, rejected waits an hour) and `NetworkChat::{request_admission,
  admission_of, admissions, decide_admission}`. `LobbyRequest::ClaimKey` hands the current key to
  a node on the member list (`NetworkChat::{record_member, member_departed}` keep it). The lobby
  has no switch: it answers whenever discovery is not off. The joining rules and the lobby
  protocol are `docs/subsystems/p2p-network.md`, 6.1.
- `network_chat.rs` - a conversation channel over the p2p endpoint, with its own
  request/response, text/file limits, a bounded queue, rate limit and a
  persistence writer outside the transport. The originating action carries the
  API key; internal retransmissions carry only the already validated frame.
- `network_chat_policy.rs` - the authenticated ratchet of the Network Chat switch;
  the announced boolean is a trigger and the authenticated read on the principal is
  the effective source.
- `discovery.rs` - the iroh endpoint and everything on it: the endpoint under the
  node key on UDP `p2p_port + 10000` (n0's relays unless `--no-default-relays`, the
  dedicated `relay_peers` URLs, member relays added live), mDNS, iroh-gossip (one
  topic for every network, announces sealed with the node key's signature, since
  iroh-gossip only authenticates the delivering neighbor), the embedded relay server
  on TCP on the same port (`GateAccess` asks `RelayGate`), the `Router` serving the
  tunnel, lobby, chat, logs and audit ALPNs, and `link_tracker`, which follows every
  connection's selected path into `Network::note_link`. It puts peers into the
  `Network` only after the admission ratchet, which on gossip never probes inline
  (`admission_without_probe`, and without a verdict `admit_in_background`). The
  announce signature binds the announce to its endpoint, but does not replace the
  membership proof. Every 5 s `apply_policy` refreshes the relay gate's members and
  URL, the member relays in use, the gossip membership, the published endpoint
  addresses and the reachability from the NAT report.
- `identity.rs` - the node's **only** Ed25519 key and the `node_id` (SHA256 of
  the pubkey). It lives in `<project_root>/.pom-node.key` (or `$POM_NODE_KEY`).
  The endpoint id (`secret_key`, the hex of the pubkey), the trust chain's
  `peer_id` (`trust_peer_id`: the libp2p `PeerId` text form signed chains carry,
  derived from the key; never the endpoint id), the
  pkarr key and the authority key also come from it; `Announce`/`PeerInfo` have a
  single `pubkey`.
- `network.rs` - the heart of the UDP P2P: `P2PMessage`, `PeerInfo`, dual-stack
  socket, gossip, peer lifecycle, discovery sweep and admission. The
  `connection_key` does not go in the `Announce`: on a private network the handler
  runs a probe on `/handshake` before registering the peer. Every announce is
  signed by the node key (`sign_announce`, key from `set_announce_key`), and the
  UDP receive loop drops one that is unsigned, signed by another key or stale
  (`verify_udp_announce`); a `PeerList` never moves the addresses of a peer whose
  own announce arrived in the last 30 s (`firsthand`, `insert_announced_peer`).
  `is_member_key` answers whether a node key is an admin of the chain or holds a
  valid, unrevoked membership token: what every node-to-node call authorized by the
  connection key is checked against (`auth::caller_is_member`). On a public network the
  rigor follows the `network.access_policy`: with `authorized` (the default) the
  `cluster_id` only selects the chain and the tuple `{node_id, peer_id, pubkey}`
  must be an active concession in `admin_trust.rs`; with `anyone_with_anchor` a
  local chain anchored exactly on that `cluster_id` is enough
  (`AdminTrust::anchors_cluster`) plus the identity bond, without a concession -
  that is what admits a co-located worker without a grant. The `profile` field
  never grants access. The `PeerList` is revalidated, and `connection_verified` is
  only written by the local gate. The profile announced by `with_identity` can be
  updated by `set_profile`, and the announced name the same way: it is
  read with `hostname()` and changed live with `set_hostname` (the My machine
  screen), never through the field; `trust_version` is the trust chain version
  each node announces (`set_trust_version`), a hint of who has a newer chain
  and never proof; `membership` is this node's membership token
  (`set_membership`, renewed by `webui::spawn_token_keeper`), announced and
  carried in `PeerInfo`, and only believed once `membership::peer_role`
  checks it against the chain; `is_principal` / `peer_is_principal` tell the
  principal from any admin (a secondary has the profile, not the principal's
  duties: chat relay, lobby, member list), by the key the local trust chain
  names; the admin capability does not come from the announcement but
  from the chain in `admin_trust.rs`. The `idle` field is also announced per peer
  to tell an unloaded model from a deployment that is down; it does not change the
  liveness rule.
- `local_discovery.rs` - the temporary UDP handshake of the initial setup and the
  administrative preference of local discovery. The admin announces only the
  fingerprint and the document's size metadata, receives a `Proof` bound to the
  node's ephemeral challenge and only then sends the document together with the
  encrypted `connection_key`. The authenticated payload is fragmented into blocks
  that fit the UDP budget of 1200 bytes and reassembled before the fingerprint
  validation. The preference stays off outside the wizard; the sensitive material
  lives only in memory and the transfer is limited to 512 KiB. The `Probe`
  carries a `DiscoveredMachine` (name, system, CPUs, RAM, GPUs/VRAM; cleaned and
  bounded by `sanitized`) so the admin's wizard can show who is looking for the
  network: the peer enters `peers` on the `Probe` with `joined: false` and becomes
  `joined: true` when it receives the root.
- `peer_client.rs` - `prefer_tunnel`/`remember_tunnel_win`/`remember_direct_win` are the
  **per peer** mark that the direct path is not answering: when on, the frame skips QUIC and
  HTTP on the first attempt. Learned, not configured - a direct success erases it - and
  `relay.rs` consults it on both sending ladders. `TunnelPeer`/`tunnel_gateway_call` carry a
  control plane call through the tunnel when no direct address answers.
  `get_internal_from_candidates` walks every announced gateway for an authenticated control
  route, trying the direct addresses and only then the tunnel, and returns an
  `InternalResponse` (status + body already read) because the tunnel does not produce a
  `reqwest::Response`; even 401/403 make the probe try the next address, because a ghost address
  can answer as another node. `PeerClients`, one `reqwest::Client` per peer, because the TLS
  verifier is per destination (without TLS it goes back to being a single one), and keeps the
  global URL scheme (`tls_enabled`/`peer_scheme`), `peer_base_url` - the one that brackets IPv6 -
  and the temporary quarantine of routes that failed; `route_order` only reorders the
  candidates, so even a fully quarantined peer still gets a real attempt. `for_peer` does not
  create a new client on every frame; `for_peer_with_state` tells the trace whether the TLS
  client was created or reused, with local transport, tunnel and a client without TLS explicitly
  unknown. `effective_quic_enabled` combines the pool's flag with the global preference confirmed
  by the principal, so lazy clients created before the propagation do not divert inference to the
  tunnel. Control requests carry `x-pom-internal-node`; the `plain()` client for Ollama and
  loopback does not. Every peer HTTP client enables `TCP_NODELAY`, including the probes and TLS
  clients, so small decode requests do not combine Nagle with the operating system's delayed ACK.
  `FAILURE_MEMORY` (5 s) is the single window the failure memories share - route, QUIC quarantine,
  opening the persistent channel, admission - and `with_liveness` sets the TCP keep-alive (~4 s).
- `quic.rs` - the HTTP/3 client and server over Quinn. The establishment budget of each call
  comes from `route_plan::book().establish_budget(peer)` (ceiling `QUIC_FALLBACK_BUDGET`), and
  `ClientPool::for_probes` pins the probe's budget - which is what measures the RTT.
  `ensure_local_route` does a UDP `connect` before the handshake and fails at once when the
  machine has no route to the destination's family (a container without IPv6 dialing an announced
  IPv6): quinn only recorded the refused `sendmsg` and waited for the whole budget. The pool keeps
  one QUIC connection per peer and announced address, uses the same Ed25519 pinning as the TCP
  connection, and leaves the UDP endpoint ready on the dedicated announced port
  (`inference_quic_port`/`gateway_quic_port`) even when the preference is off. The dial requires
  `host:port` and never derives the destination from an HTTP URL. The listener asks for the
  caller's node certificate and hands its identity to the request (`serve::TransportPeer`), like
  the TCP listener. Each answer exposes
  the Quinn connection's smoothed RTT in microseconds, separate from the remote handler's
  processing; `request_body_send_us` measures the wait for the body's `send_data` and `finish`,
  which is neither the response time nor a delivery confirmation. Every complete attempt, handshake
  to response body, runs on the frame's single deadline (`relay::frame_deadline`, fixed in
  `send_frame_or_tunnel` and read with `relay::frame_time_left`) - there is no nested smaller
  ceiling. A dropped peer is detected by the connection's life (`QUIC_MAX_IDLE` 1.2 s). Discovering
  a dead endpoint shares an aggregate budget of 300 ms across addresses and quarantines that address
  for 5 s so a handshake is not repeated on every token; if a request fails on a cached session (the
  `ApplicationClose` of a restarting peer) the pool closes and removes that instance - comparing the
  cached one, so it does not erase a connection created concurrently - and handshakes once more
  inside the same budget. A peer in `tunnel_first` has its direct path probed in the background every
  `DIRECT_REPROBE_INTERVAL` (`relay::spawn_direct_reprobe`); a success erases the mark.
- `pkarr_node.rs` - key -> address resolver on the pkarr DHT: a signed `_pom`
  TXT with the endpoint's public direct addresses and its relay URLs;
  `publish`/`resolve`/`resolve_dial_addr` (one `EndpointAddr` with both). The live publisher is
  `gateway::PkarrPublishHandle`.
- `public_directory.rs` - the federated protocol of the public catalog. Complete
  records are signed by the network's principal, have a 16 bit proof of work, a
  monotonic sequence, a TTL of seven days and a deterministic merge. The local
  cache accepts at most 1024 records; replicas are only configurable HTTP(S)
  tickets and not authority. The signed record includes the description, rules and
  channels in Markdown together with the invitation, and `relay_hints` (relay
  URLs with a host name, never an IP - `p2p::relay_hint`). The record carries no address: the network is found by the
  publisher's key. It also carries `accepted_enrollments`, the enrollment roles
  the publisher advertises as open (empty = not accepting new members); the
  field is signed with the rest of the record. A private node queries those tickets by egress, but does not use its
  private network as public discovery.
- `tls.rs` - the self-signed certificate derived from the `.pom-node.key`, the
  `ServerCertVerifier` that checks `SHA256(pubkey) == node_id`, and mutual TLS:
  `server_configs` builds the peer configuration (node certificate, asks for the
  caller's, `AnyNodeClient`) and the browser one (ECDSA, asks for nothing), picked
  per connection by `wants_peer` on the ALPN; `client_config` presents this node's
  certificate (`set_client_identity`). The peer certificate never goes to disk.
  TLS between nodes is always on. See `docs/subsystems/p2p-network.md`, section 5.
- `web_ca.rs` - the browser side of that TLS: the persisted web key
  (`.pom-web.key`) and the machine's local CA (`.pom-ca.key`, public half in
  `.pom-ca.crt`), name-constrained to `localhost`, the host name and
  `<SLUG>_TLS_NAMES`; `browser_certificate` issues the ECDSA certificate the
  browser gets, signed by that CA, with only the DNS names the CA permits. It is
  kept in `.pom-web.crt` and served again at start while it was issued by the
  current CA for the web key, names every current address and has more than 60
  days left: a browser that does not trust the CA holds its exception for one
  exact certificate, so reissuing on every start asked again on every restart. `is_local_web_certificate` (by public key, not by
  certificate) and `local_host_of` are what the native windows check before
  accepting the interface's certificate when the system does not trust the CA.
  Files are created with a link so two processes agree on one key. The OS
  trust store side is `src/trust_store.rs`.
- `quic_policy.rs` - the authenticated ratchet of the QUIC preference: the
  Announce's `quic` field only triggers `GET /api/cluster/quic`; the trigger is only
  accepted when the `pubkey` identifies the anchored principal, never by
  `profile: admin`; only the principal's answer is adopted and a failure preserves
  the local value.
- `cluster_logs_policy.rs` applies the same discipline to the
  `cluster_logs_enabled` field: the announcement is a trigger, the authenticated
  read on the principal is the source, and a failure preserves the local value. The
  list of admins for replication is independent of that ratchet and does not make a
  secondary able to change the policy.
  The TLS, logs and Chat reads receive the list of candidate gateways from the
  principal; that is the only path that keeps the address fallback and the probe's
  TLS policy aligned.
- `network_chat_enabled` follows the same contract. The trigger is only accepted as
  a decision of the principal whose `pubkey` was confirmed by the
  `AdminTrust`; the announced profile does not identify authority. A switch opened
  on a secondary admin routes the mutation to the principal and only its write
  changes the authoritative policy. The Network Chat relay is a star: the
  principal admits an action by the sender's transport and membership and checks
  `blocked_nodes` before sending to other `node_id`s; the browser's API key is
  checked by the sender's own gateway and never travels.
- `NetworkSettings` has the same authority boundary, but with an explicit
  read: after the principal's first authenticated `Announce`,
  `Network::handle_message` wakes the `GET /api/cluster/network-settings`
  ratchet. The gateway signs the query, tries the principal's addresses and,
  last, the tunnel (`GatewayState::control_get_via_tunnel`), persists the full
  document on the read-only node and never sends the `connection_key` when the
  network is public. Until it syncs, the ratchet is woken on every Announce,
  and only the 1st, 2nd, 4th, 8th... consecutive failure becomes a `warn`. The
  public directory subset is still only invitation and discovery, not the
  source of truth for the metadata.
- `route.rs` - `RouteTable`: which address to use for each peer, with a `/health`
  probe, hysteresis against a route swap by noise, and immediate invalidation when
  the peer changes its announced addresses. It also keeps the **failure memory per
  `(node_id, URL)`**, which serves both the pipeline and the control plane - see
  `docs/subsystems/p2p-network.md`, "Address that stopped answering". `RttTable`
  keeps this node's directional measurement for each destination, without inferring
  the reverse direction, and exposes snapshots with age: only RTTs younger than five
  minutes enter selection.
  `last_success` returns the last HTTP address that answered (less than 5 min ago
  and without a failure after), the HTTP half of `Network::peer_route`.
- `route_plan.rs` - the **per-edge route plan**: which transport (QUIC, HTTP/HTTPS or the p2p
  tunnel) this node speaks with each peer, measured in the background and pinned. `RouteBook`
  (global to the process, `route_plan::book()`) keeps per `(node_id, Plane)` the measured routes
  in RTT order (a 2 ms tie decides QUIC, then HTTP, then the tunnel); the two planes are
  `Inference` (prefill/decode) and `Gateway` (the return of the direct reply and of the
  remote-stream). `relayed_nodes` lists the peers reached only through a relay (every live
  connection's selected path on a relay), rebuilt on each turn of the planner from
  `Network::p2p_links`. `spawn_planner`, started by
  `Network::start`, measures every edge when beaten (3 min) or when the peer changed addresses,
  ports, `deploy_id` or layer range - which is why a deployment that becomes `serving` triggers the
  analysis before the first request. `measure_edge` probes in parallel with the real transport
  (`GET /health` over HTTP with the pinned client, an HTTP/3 POST over the QUIC endpoint - any
  status proves the path - and `/health` through the tunnel), two samples, the smaller counts; the
  tunnel probe does not count the tunnel's own "nothing is listening" 502
  (`infer_tunnel::NOTHING_LISTENING_STATUS`) as a route. `record_failure` repeats the route once,
  then moves to the next measured one and, with the list exhausted, erases the edge and wakes the
  planner. `begin_use` returns the `RouteUse` guard every frame and every return delivery holds:
  with a frame in flight or until `PIN_GRACE` (60 s) after the last one, the planner neither probes
  the edge nor swaps its route, and `active` ignores `PLAN_TTL`; only the frame's own failure swaps
  it. An empty analysis keeps the existing route but leaves the edge due, repeating after
  `EMPTY_RETRY` (5 s). `establish_budget` gives the connection budget: 3x the measured RTT between
  100 ms and the ceiling, 150 ms without a measurement. `snapshot`/`signature` feed the
  `route_plan` field of `/api/ui/cluster`.
- `Network::gateway_bases` - a peer's gateway addresses in the order worth trying:
  the host that already answered (`RouteTable::last_success`, compared by host
  because it may have been the inference port), then the ones that did not fail,
  then the discredited ones. Whoever can only use one address (a download, an
  offer's `from_gateway`) uses the first.
  `inference_bases` does the same with the inference addresses (the probe of
  admission to the plan), and `order_gateway_bases` orders a list assembled by the
  caller (the policy ratchets, with the announced scheme).
- `Network::peer_route` / `p2p_links` - the route in use with each peer for the
  participants screen: direct HTTP that answered, a direct endpoint path
  (marked when it was born of a hole punch) or only a path through a relay, with
  `relay_kind` `dedicated`, `public` (n0's) or `member`. The links come from
  `note_link`/`drop_link`, called by `discovery::link_tracker`; see
  `p2p-network.md`, "Diagnosis".

## Key functions

`network.rs`:
- `Network::new` - a dual-stack bind (`[::]`) hand-assembled with `socket2` and
  `IPV6_V6ONLY` turned off **before** the bind (after it has no effect; on Windows
  the default is 1 and a node there would see no IPv4 peer at all). Without IPv6 in
  the OS, it falls back to `0.0.0.0`.
- `Network::testkit` - an isolated UDP socket on `127.0.0.1:0`, with the local
  announce's destination pinned to loopback; `testkit_with_broadcast_addr` allows
  injecting a loopback endpoint to observe the datagram and refuses the global
  broadcast.
- `Network::start` - spawns the receive loop, the gossip loop (5 s: unconditional
  announce + `discovery_sweep` + `cleanup_dead_peers`) and the route and RTT
  measurement task (30 s, a slow probe does not hold up the gossip). The initial TLS
  policy is applied before `start`; the task probes `/health` on each candidate's
  inference port and measures the RTT over the p2p tunnel when the peer announces it.
  Each `self_announce` enumerates the interfaces once per announcement, so a
  temporary IPv6 change enters the next gossip without a network probe. With an
  explicit pin, that enumeration is not published: the pin replaces the local list.
  The handling of each datagram is protected by `catch_unwind`, with context in the
  warning and a rate limit so the log is not flooded if the panic repeats.
- `Network::handle_message` - Announce (rewrites `0.0.0.0` with the source address
  and calls `merge_observed` only when there are no explicit candidates), PeerRequest
  (answers an immediate Announce + a chunked `PeerList`), PeerList, Heartbeat,
  LocalDiscover. The `PeerList` also
  updates an already known peer when the snapshot is newer, preserving the local
  verification state and the address candidates **this node has not yet proven
  dead**; older snapshots are
  ignored so the coverage is not reverted. A detached node (`is_detached`) neither
  learns nor answers. A received pin does not gain the source address as an extra
  candidate.
- `Network::request_peer_addresses` - when **every** known address of a peer is
  discredited, it sends `PeerRequest` to the bootstrap nodes and to the other live
  peers and prunes the dead candidates from the local entry, so the next
  `Announce` can re-prove them. Once per `REDISCOVERY_INTERVAL` (30 s) per peer;
  `is_rediscovering` is what the Dashboard reads to say "finding it again" instead
  of "unreadable", and it expires in `REDISCOVERY_GIVE_UP` (90 s) - a promise
  without a deadline would be the same dishonest warning with another word.
  `note_peer_read` ends the search when the pair becomes readable again. Temporary
  IPv6 addresses rotate on their own, and the cluster has to live with that without
  `--advertise-addr`.
- `Network::cleanup_dead_peers` - besides removing the expired peer, it calls
  `RouteTable::forget` on it, so a node that comes back with the same address does
  not be born discredited.
- `PeerInfo::dialable_addrs` - every valid UDP address, deduplicated, so discovery
  and rediscovery try the candidates before giving up.
  `PeerInfo::endpoint_dialable_addrs` is what the endpoint gets as dial hints:
  the announced `endpoint_addrs` (relay URLs and public direct addresses) and then
  the UDP candidates on the endpoint port (`UDP + 10000`); iroh tries them all
  and keeps what answers.
- `PeerInfo::gateway_candidates` / `gateway_url` - one gateway URL per known
  address, with `addr` first (a known fact: it is where the peer's packets arrived)
  and the announced candidates after; `gateway_url` is just the first. While it was
  only `peer.addr`, the whole control plane (Dashboard, `deploy`, transfer,
  `devupdate`) had one address and no plan B.
  Same discipline as `pipeline::resolve_step_url`: a guess does not go in front of a
  known fact.
- `Network::discovery_sweep` - a rotating PeerRequest: 3 per sweep, a sweep every 6
  ticks (30 s), the rotation covers the whole cluster. It is what learns transitive
  peers outside the LAN; without it the discovery is a portrait of the startup.
- `Network::normalize` - maps an IPv4 destination to `::ffff:a.b.c.d` on the
  dual-stack socket. Without it macOS returns EINVAL on every send and the discovery
  dies in one direction only (Linux accepts it, which is why it looked one-sided).
- `peer_list_chunks` - chunks the `PeerList` to fit `MAX_DATAGRAM_PAYLOAD`
  (1200 B, the floor IPv6 guarantees; the operator's 5G has MTU 1450 and IPv6 does
  not fragment).
- `advertised_addr` - normalizes a hand-typed address (a bare IPv6 needs brackets to
  get a port) into `host:port`.
- `detach`/`rejoin` - taking the node off / bringing it back to the network without
  bringing the UI/API down (`--no-discovery`); rejoin redoes bootstrap + announce.
- `self_announce` - assembles the Announce; it announces zero layers when there is no
  assignment, so a node configured through the UI is still visible. It re-reads the
  interfaces once per announcement, every 5 s in the gossip and also in immediate
  announcements; it is a local read of `ifconfig`/`ip`, without a network probe. The
  same snapshot feeds `addr` and `addr_candidates`. The `advertise_addr` hint, when
  present, goes first and **does not replace** the rest; then come, in UDP and
  gossip: the interfaces' current public address with stable before temporary,
  the address the relays observed (the NAT report), the other local interfaces and the
  `0.0.0.0` sentinel. Bridges and container virtual pairs do not enter the automatic
  candidates. When the
  inference server is loaded, the announcement also includes
  `inference/remote-token-stream-v1`, which lets a remote gateway ask for the decode
  loop with callbacks in bounded batches. The same announcement carries
  `transport/link-v1`, the persistent link between floors (`/v1/link`), which is
  served by the same router as the other `/v1` routes. The
  `inference/frame-v1-batch` capability only enters when the inference server has
  loaded a model and the multi-item scheduler is active; setup, TP, loading and a
  drop do not announce inference capability. Separate capabilities announce f8, i4g64
  and i4g128; f16 stays implicit and default. `runtime_capacity` only enters
  after `/health` confirms the model is loaded and carries `n_ctx` and
  `max_new_tokens` (a leg); apply, error, stop and thermal unload clear the field
  before announcing. There is no announcement or fallback
  for earlier frame formats, because the product has not shipped yet.
  `cluster_logs_enabled` is also announced, but only as a trigger of the ratchet:
  the effective policy comes from an authenticated read on the principal.
- `TpIdentity` (`{role, group_id, size, rank}`) - the tensor
  parallelism identity (path C) announced in `Announce.tp`/`PeerInfo.tp`
  (`None` = a node without TP). Each `TpMember` carries the endpoint id, the UDP
  address candidates and the capability of the preferred p2p tunnel across
  networks. `set_tp`/`tp_identity` set/read without a restart;
  `PeerInfo::is_tp_worker` is the predicate the planner uses to exclude workers from
  the chain.

`discovery.rs`:
- `Discovery::new` - binds the endpoint under the node key on UDP `p2p_port + 10000`
  (both stacks, IPv4 alone as fallback; `presets::Minimal`, the relay map with n0's
  relays unless `--no-default-relays` plus the dedicated `relay_peers` URLs), adds the
  operator's pinned IP as an external address, starts mDNS, iroh-gossip (64 KiB
  messages) and the embedded relay server, and spawns the `Router`. iroh's own pkarr
  publisher is not used: our `_pom` record lives under the same key.
- `handle_announce` - a gossip announce: drops it when the node is detached, when it
  has no `endpoint_id` or when the seal does not verify against that id
  (`open_announce`); builds the `PeerInfo` using `addr_candidates.first()` (the literal
  `0.0.0.0` only survives if nothing was announced), runs the admission ratchet, learns
  the dial hints and warms the tunnel connection of a tunnel-capable peer in the
  background.
- `seal_announce`/`open_announce` - the node key's signature over
  `ANNOUNCE_SIGNATURE_DOMAIN + announce`, prepended to the gossip message. iroh-gossip
  only authenticates the neighbor that delivered a message, so this is what binds an
  announce to its author. That bond does not replace the Ed25519 chain of
  administrative authority.
- `answer_tunnel_request` - one tunnel request: resolves the target port, refuses a
  file piece with 421 when the connection's selected path may not carry it
  (`Network::path_carries_transfers`) and with 416 without a bounded range, refuses a
  route outside the closed list with 403, and forwards the rest to loopback
  (`infer_tunnel::serve_loopback`) on the stream's own task.
- `answer_chat_request`/`accept_original_chat`/`relay_chat` - the Chat star: the
  principal validates an original message and relays it to every member (each send on
  its own task, so a slow member holds up nobody); a member accepts only what the
  principal relays. `answer_lobby_request` answers a non-member (see `network_lobby.rs`).
  The logs and audit handlers ingest on the receiving admin and never retransmit.
- `apply_policy` - every 5 s (`POLICY_INTERVAL`): the relay gate's member list and the
  URL it offers (`http://<public host>:<p2p_port+10000>`), the member relays added to or
  removed from the relay map (up to `MEMBER_RELAYS`, 2, beside n0's and the dedicated
  ones), the relays in use for the traversal panel, the gossip membership (every
  admitted peer joins the topic), the published endpoint addresses
  (`publishable_addrs`: relay URLs and public direct ones) and, from the NAT report's
  `global_v4`, the observed address and the reachability (`public` when it is one of
  the machine's own addresses).
- `link_tracker` - the hook every connection, in or out, goes through: it follows the
  connection's path list until it closes and writes the selected path into
  `Network::note_link`; a move from a relay path to a direct one is a hole punch
  (`remember_hole_punch`).
- `GateAccess`/`start_relay_server` - the embedded `iroh-relay` server on TCP, plain
  HTTP, asking `RelayGate::allows` per connecting endpoint. A taken port leaves the
  node without it (`RelayGate::set_available(false)`).
- `TraversalStatus`/`traversal_status` (in `network.rs`) - the NAT report state,
  published endpoint addresses, relays in use (`set_relays_in_use`) and the hole punch
  count; `/api/ui/cluster` publishes it in the `traversal` field, which also enters
  the cache's fingerprint.
- `network::RelayGate` - whom this node relays for, consulted by the relay server on
  every connecting endpoint (`allows`): everyone with `relay.everyone`/`--serve-relay`;
  the admitted members of its cluster when the internet reaches it and
  `relay.cluster` is on (the default); nobody otherwise, and nobody when the server
  did not come up (`set_available`). An iroh relay cannot tell an endpoint that homes
  on it from one that only sends through it, so a member relay admits members only.
  `offer()` is what goes into the `Announce` (`RelayOffer`, with whether transfers may
  cross it and the `url`). The options are atomics: `set` changes them live from the
  screen, `RelayOverrides` keeps what the command line fixed for the run, and
  `status()` is what `/api/ui/network/relay` answers.
- `Network::relay_carries_transfers`/`path_carries_transfers`/`transfer_links` - the
  rule of files and relays: a direct path carries a file; a relay path only through a
  member relay (its URL is an admitted member's announced offer) that allows it; never
  n0's relays, a relay in `relay_peers` or any other.

`addrs.rs`:
- `filter_candidates` - discards loopback/link-local/deprecated, stable before
  temporary, a ceiling of 2 per family (macOS creates several IPv6 on its own and the
  `PeerList` is already at the MTU limit); an address pinned by the operator comes
  first and does not count toward the ceiling. `Network` separates those candidates
  into public and private: the current public one comes before the one the relays
  observed, and the observed one stays a fallback for NAT. The comparison with
  what already entered is done without brackets on both sides - `pinned` arrives
  bracketed from `effective_host` and the local one arrives bare, and comparing the
  two forms duplicated the IPv6 in the announcement.
- `Network::effective_host` (via `choose_effective_host`) - the host by which the
  node describes itself: the pinned one, then the interface's current public one,
  then the observed one. It always returns an IPv6 **between brackets**, because
  whoever uses it concatenates `:port` (the node's own entry in `all_peers`, which
  the participants screen shows) or passes it through `ip_host`
  (`self_reply_hosts`). The interface's address arrives bare from
  `filter_candidates` and is bracketed there.
- `is_private_or_local` - decides whether an address observed by a peer is news
  about this node's public IP. Besides RFC 1918 and `fc00::/7`, it refuses
  **`100.64/10` (RFC 6598)**: it is the CGNAT range and the one Tailscale uses.
  Measured on 2026-08-16 on a laptop (then under libp2p): the first `identify` answer came through a
  peer reached over the tailnet and reported a `100.x.y.z`, which the node adopted as
  its *public* address and announced to the whole cluster.
- `local_addrs` - `/sbin/ifconfig` (macOS) / `ip -o addr` (Linux); macOS uses the
  absolute path because processes started by the GUI may not have `/sbin` in the
  `PATH`. Windows returns empty (it falls to the address observed by a peer). If the
  program cannot be run, `run` warns once in the log and returns empty: the node
  then announces no address of its own. That is why the Docker image installs
  `iproute2`.

`identity.rs`:
- `Identity::load_or_create` - loads the key or generates one; regenerating on every
  boot duplicated the peer in the list until the timeout and voided `blocked_nodes`
  (keyed by node_id). Corrupted, it generates a new one instead of stopping.
- `NodeId::from_pubkey` - SHA256 of the pubkey; `Display` prints the first 12.
- `key_path` - `.pom-node.key`, resolved against `config::project_root()`,
  overridable with `$POM_NODE_KEY`. It does not come from the cwd: launching the
  binary from another directory would create a second identity. There is no
  separate pkarr key.
- `Identity::secret_key` / `peer_id` - the same seed as the iroh endpoint's secret
  key; `peer_id` is `trust_peer_id`, the form the trust chain's tuples carry.

`route.rs`:
- `choose_route` - the best live candidate; it only swaps the current route if the
  gain passes 25% relative **and** 2 ms absolute (measured 9.6 vs 7.2 ms laptop to
  CUDA server - it cannot swap on every cycle).
- `RouteTable::measure_peer` - probes `GET /health` on the announced inference URL
  itself, not on the gateway. That way a healthy gateway does not promote an
  inference port that swallows SYN. The probe accepts the self-signed certificate and
  the peer's hostname; that does not change the pinned client used by the inference.
  A 3 s timeout and a 5 min interval.
- `RouteTable::mark_failure` / `mark_success` / `is_failed` / `usable` - the
  failure memory, keyed by `(node_id, URL)` and valid for 5 min. It writes when a
  **real request** failed on that address (a slow probe does not write: its timeout
  is too tight and discrediting everything would leave `resolve_step_url` without a
  route). Because the key is the whole URL, the inference port and the gateway port
  of the same host coexist - and it is the same memory the Dashboard's reading uses,
  instead of a parallel mechanism. `usable` returns the candidates still alive; empty
  means "every address of this pair failed", and the caller should ask for the current
  one instead of insisting.
- `RouteTable::invalidate` **preserves** the failure memory; only `forget` erases it.
  Erasing it in `invalidate` meant that the IPv6 rotation - the event that kills the
  address - was also what erased the proof that it died.
- `RouteTable::endpoint_is_discredited` - compares host and port when filtering
  inherited candidates. An HTTP failure on the gateway or on the inference does not
  discredit the same host's P2P/UDP port; that avoids removing the address of a live
  tunnel.
- `RouteTable::resolve` - the address chosen for the node_id; `None` =
  "not measured yet or expired", and the caller falls to the plan's `inference_url`.
- `RttTable` - keeps `target_node_id`, `rtt_ms` and `transport` locally;
  `p2p` is the normal measurement (through the tunnel) and `direct-health` identifies the probe
  fallback. `fresh_snapshot` is the only RTT source for selection; `snapshot_with_age`
  keeps the age for diagnostics. The renewal happens every 120 s, independently of the
  5 min re-measurement of addressing. If the peer announces the tunnel but its endpoint
  is not ready yet, the inference route's HTTP probe stays a `direct-health` fallback,
  instead of erasing the measurement. The authenticated endpoint
  `/api/ui/network/rtt` publishes only fresh measurements with `source_node_id`.

`pkarr_node.rs`:
- `PkarrNode::from_signing_key` - client over the node key; there is no
  separate key file.
- `PkarrNode::publish(&PkarrRecord)` - signs the `_pom` TXT (`a` direct
  endpoint addresses `ip:port`, `x` relay URLs, `h`, `i`, `r`, `g`), trimmed from
  the end until it fits in 900 B. The `g` field carries the
  geo label as metadata, without changing the key.
- `resolve`/`resolve_public_peer` - a `pk:` or hex key into
  `ResolvedPeer { endpoint_id, record }`; failure becomes `None`, never an error.
  This does not enumerate the DHT: `/api/public/networks` only resolves
  candidates that already arrived through UDP/gossip or sit in the local cache.
  The Mainline DHT used by pkarr keeps an actor on its own thread and uses UDP
  IPv4; on Windows its 50 ms read timeout becomes WSAETIMEDOUT (10060), an
  expected idle result the binary's log filter downgrades without hiding real
  errors.
- `dial_addr`/`resolve_dial_addr` - the direct addresses and the relays together
  in one `EndpointAddr`; iroh tries them all.
- `endpoint_id_from_uri` - a `pk:` key -> endpoint id (the same Ed25519 key).
  The record is ours, not iroh's pkarr format: one key holds one record, so
  iroh's publisher stays off.

## Links

- **Called by**: `main.rs` (builds `Network` + `Discovery`, publishes to gossip
  every 5 s the same announcement as the UDP, via
  `Network::gossip_announce_bytes` -> `self_announce`, with or without assignment;
  `spawn_pkarr_bootstrap` resolves `--pkarr-peers` and the trust chain principals
  by key and hands them to `Discovery`),
  `routing/pipeline.rs` (`resolve_step_url` uses `RouteTable`), `gateway/`
  (consumes `PeerInfo`/`alive_peers`).
- **Calls**: `network.rs` ← `identity.rs` (via main), `addrs.rs`
  (`self_addr_candidates`), `route.rs` (`Network.routes`); `discovery.rs` →
  `network.rs::insert_admitted_peer`, `p2p.rs`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
