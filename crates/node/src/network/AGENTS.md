# network/ - the P2P protocol

The node's discovery transports and addressing. Two worlds in the same
directory: the native UDP (`network.rs`, `addrs.rs`, `identity.rs`, `route.rs`)
and the libp2p/pkarr for the internet (`discovery.rs`, `pkarr_node.rs`).
`main.rs` assembles `Network` and `Discovery`; `gateway.rs` and
`routing/pipeline.rs` consume `PeerInfo` and `RouteTable`. The house rule: the
address decision belongs to whoever is going to dial, not to whoever announced.
Identity is a key, never an IP: the operator's `advertise_addr` is only a hint
that goes in front of the candidates (`addr_candidates` + `RouteTable` keep the
fallback), and signed documents (trust, public directory) carry `pk:` keys, never
addresses. The rules for when to use UDP, pkarr, libp2p and a direct route, and
the ladder direct -> dedicated relay (with DCuTR) -> member relay, are in
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
- `infer_tunnel.rs` - the request/response protocol that carries the inference
  call through the libp2p swarm as the preferred transport when the peer
  announces that capability. If the tunnel fails on transport, the relay uses the
  pinned direct client as fallback. `TunnelRequest` carries method, path, body and
  headers, including the `connection_key` and the inference port's internal
  marker. The frame is binary - `[u32][JSON head][u32][raw body]`
  for a request, `[u16 status][u32][raw body]` for a response - because a JSON
  array of numbers cost about 3.6 bytes per byte of payload on every relayed
  frame. `is_transfer_data_path`, `closed_range` and
  `transfer_chunk_is_bounded` are what the receiving side uses to hold the
  file rule: a piece of a file is a bounded range, and it is refused
  (`TRANSFER_ROUTE_REFUSED_STATUS`, 421) on a connection that crosses a public
  relay;
  `serve_loopback` hands the call to the loopback, always in clear text
  (`serve::plain_allowed` frees loopback even under strict TLS and the node's
  certificate does not cover `127.0.0.1`). `TunnelTarget` chooses **which** local
  server answers: `Inference` (the default, and the only one a peer of an earlier
  binary knows) or `Gateway`, the control plane. The sender names the variant; the
  receiver is the one that maps it to one of the two ports it itself chose - a
  remote peer never names a port. For the gateway there is
  `gateway_path_is_allowed`, a **closed** list of exact comparison: admission,
  policies and direct return (`/handshake`, `/health`, `/api/cluster/*`,
  `/v1/relay-result`, `/v1/remote-stream-result`) and the control plane of the
  deployment and of the cluster reads (`/api/ui/{node,apply,...}`), plus
  administrative sync with the principal
  (`/api/internal/{api-keys,admin-trust,profile}/*`, all `InternalNodeAuth`). Everything
  that leaves the tunnel reaches the gateway through `127.0.0.1`, and the gateway
  decides what is local by the socket's address; that is why `serve_loopback`
  marks the request with `TUNNEL_ORIGIN_HEADER` and the process secret
  (`tunnel_mark`), and `serve::request_origin` treats it as remote. Without the
  mark, any route on the list would hand the node's administration (localhost
  mode, `install_trust_anchor`) to any peer of the swarm. The request's method is
  passed on as it came (GET, POST, ...). `peer_client::send_to_gateway_over` is the
  control plane's sender: each announced address and, last, the tunnel. POSTs
  without a `Content-Type` get the binary default, but an explicit
  `Content-Type`, like `application/json` in the session lifecycle, is never
  duplicated nor overwritten. `measure_rtt` measures the round trip through the
  same Noise tunnel.
- `cluster_logs.rs` - bounded `tracing` capture and a separate request/response
  transport (`/pom/cluster-logs/1.0.0`); the queue discards under pressure, and
  each batch goes by fanout to every peer announced as admin. `CaptureLayer` uses
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
  announced field only triggers reading the policy on the principal.
- `network_chat.rs` - also transports `ChatAction::PluginNotice`, the notice a
  plugin sends to every node (`notification.notify` with
  `scope: "network"`): the same authenticated path and the same rate limit as the
  messages, but kept only in memory (50 notices, 24 h, `notice_snapshot`), never
  in `network_chat_messages`.
- `network_lobby.rs` - the lobby: the `/pom/network-lobby/1.0.0` protocol a
  machine that is **not** a member uses to talk to a public network's principal
  before joining (`docs/subsystems/p2p-network.md`, 6.1). Holds the frame types
  and bounds (`LobbyLimits`), the self-declared hardware card (`LobbyCard`,
  clamped by `sanitized`), `transport_keypair` and `LobbyIdentity` (the lobby connects
  under a key derived from the node key and proves the node with a signature
  over that transport `PeerId`; it must never share the cluster swarm's
  `PeerId`), `LobbyGuests` (presence plus the
  per-candidate rate, used by the principal and, fed by relayed frames, by
  every member) and `LobbyClient`, the candidate's dial-only swarm that also
  runs in setup mode. The principal's handler is
  `discovery.rs::answer_lobby_request`; inside the network the lobby is the
  `lobby` room of `network_chat.rs`, which gained `ChatAction::LobbyPresence`
  and `NetworkChat::{lobby_guests, lobby_enabled, auto_approves}`. The lobby
  has no switch: it answers whenever discovery is not off, and under
  `access_policy = auto_approve` the handler approves an admission request as
  it arrives. The `Info` request and
  `PublicStats` publish models, node count and VRAM under a separate switch
  (`NetworkChat::{public_stats, public_stats_enabled}`); the slot is filled by
  `webui::spawn_public_stats_publisher`, never by the swarm loop.
  `Describe` and `Directory` are answered by any node from `DirectoryBoard`
  (own public network; known public networks when the node is a relay or in a
  public network), filled by `gateway::spawn_directory_board_publisher`.
  `Publish` lets a network's principal hand its description to a relay over a
  direct dial (`DirectoryBoard::accept_publication`), the listing path that
  needs no circuit. `default_directory_relays` is the list of relay keys a fresh installation
  asks (`DEFAULT_DIRECTORY_RELAYS` plus `POM_DIRECTORY_RELAYS`).
  Admission requests live here too: `LobbyRequest::Admission`,
  `AdmissionRecord` / `AdmissionView` / `AdmissionGrant` and
  `apply_admission_request` (pending is refreshed, approved is kept, rejected
  waits an hour); the principal's store is `NetworkChat::{request_admission,
  admission_of, admissions, decide_admission}`. `network.rs` gates membership
  by the connection key unless `NetworkSettings::open_admission`, and starts
  detached when the network's visibility is `local`. `LobbyRequest::ClaimKey` hands the
  current key to a node on the member list (`NetworkChat::record_member` /
  `member_departed` keep it). `infer_tunnel.rs` lets
  `/api/cluster/member-left` through the tunnel, the only way a member behind
  NAT reaches its principal to say it is leaving.
- `network_chat.rs` - a conversation channel over the libp2p swarm, with its own
  request/response, text/file limits, a bounded queue, rate limit and a
  persistence writer outside the swarm loop. The originating action carries the
  API key; internal retransmissions carry only the already validated frame.
- `network_chat_policy.rs` - the authenticated ratchet of the Network Chat switch;
  the announced boolean is a trigger and the authenticated read on the principal is
  the effective source.
- `discovery.rs` - the libp2p swarm: gossipsub (IDONTWANT only above
  `GOSSIP_IDONTWANT_THRESHOLD_BYTES` = 64 KiB, so announces never trigger it), Kademlia, mDNS, identify, relay
  (client and server), DCuTR, AutoNAT, the inference tunnel and the protocols of
  the cluster logs, the request audit and the Network Chat. The audit received
  enters a bounded queue outside the swarm loop and goes on to the active plugin
  or the local fallback. It puts peers into the `Network` only
  after the admission ratchet, which on the gossipsub never probes inside the
  swarm loop (`admission_without_probe`, and without a verdict
  `admit_in_background`): the probe would land on the tunnel, which that loop
  serves. It rehydrates the admitted peers' addresses on the swarm before sending
  through the tunnel when the announcement arrived before the route record. The
  gossipsub's native signature proves the transport's origin, but does not replace
  the swarm's membership proof.
- `identity.rs` - the node's **only** Ed25519 key and the `node_id` (SHA256 of
  the pubkey). It lives in `<project_root>/.pom-node.key` (or `$POM_NODE_KEY`).
  The `PeerId` (`libp2p_keypair`/`peer_id`), the pkarr key and the authority
  key also come from it; `Announce`/`PeerInfo` have a single `pubkey`.
- `network.rs` - the heart of the UDP P2P: `P2PMessage`, `PeerInfo`, dual-stack
  socket, gossip, peer lifecycle, discovery sweep and admission. The
  `connection_key` does not go in the `Announce`: on a private network the handler
  runs a probe on `/handshake` before registering the peer. On a public network the
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
  and never proof; `is_principal` / `peer_is_principal` tell the
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
- `peer_client.rs` - `prefer_tunnel`/`remember_tunnel_win`/`remember_direct_win`
  are the **per peer** mark that the direct path is not answering: when on, the
  frame skips QUIC and HTTP on the first attempt. Learned, not configured - a
  direct success erases it. `relay.rs` consults it on both sending ladders.
  `TunnelPeer`/`tunnel_gateway_call` carry a control plane call through the tunnel
  when no direct address answers; that is what makes the admission of a pair behind
  NAT complete.
  `get_internal_from_candidates` tries the direct addresses and only then the
  tunnel, and returns an `InternalResponse` (status + body already read), because
  the tunnel does not produce a `reqwest::Response`.
  `PeerClients`, one `reqwest::Client` per peer, because the TLS
  verifier is per destination. Without TLS it goes back to being a single one. It
  also keeps the global scheme of the URLs (`tls_enabled`/`peer_scheme`) and
  `peer_base_url`, which is the one that brackets IPv6, besides the preference and
  the temporary quarantine of routes that failed. `route_order` only reorders the
  candidates: even when all are in quarantine, it returns the list so the real
  attempt decides.
  `get_internal_from_candidates` walks every announced gateway for an authenticated
  control route; even 401/403 make the probe try the next address, because a ghost
  address can answer as another node. The client destined for another node marks
  control requests with `x-pom-internal-node`, uses the shared `connection_key` also
  in the inference HTTP fallback on a public network and limits the TCP open to 2 s;
  HTTPS and QUIC may validate through the secure transport and the same marker. The
  `plain()` client for Ollama and
  loopback does not send that mark. `for_peer_with_state` tells the trace whether the
  TLS client was created or reused; local transport, tunnel and a client without TLS
  stay explicitly unknown. `effective_quic_enabled`
  combines the pool's flag with the global preference confirmed by the principal,
  so that lazy clients created before the propagation do not divert inference to
  the tunnel. Every peer HTTP client enables `TCP_NODELAY`, including the probes and
  TLS clients, so that small decode requests do not combine Nagle with the operating
  system's delayed ACK. The shared `reqwest::Client` and the cache by `node_id` also
  preserve the connection pool; `for_peer` does not create a new client on every
  frame.
- `quic.rs` - the establishment budget of each call comes from
  `route_plan::book().establish_budget(peer)` (ceiling `QUIC_FALLBACK_BUDGET`);
  `ClientPool::for_probes` pins the probe's budget, which is what measures the RTT.
  `ensure_local_route` does a UDP `connect` before the handshake and fails at once
  when the machine has no route to the destination's family (a container without
  IPv6 dialing an announced IPv6) - quinn only recorded the refused `sendmsg` and
  waited for the whole budget.
- `quic.rs` - the HTTP/3 client and server over Quinn. The pool keeps one QUIC
  connection per peer and announced address, uses the same Ed25519 pinning as the
  TCP connection and leaves the UDP endpoint ready on the dedicated announced port
  (`inference_quic_port`/`gateway_quic_port`) even when the preference is off. The
  dial requires `host:port` and never derives the destination from an HTTP URL.
  The listener's configuration is independent of the TCP's `tls_required`, because
  QUIC carries its own TLS 1.3. Each answer also exposes the Quinn connection's
  smoothed RTT in microseconds, separate from the remote handler's processing.
  `request_body_send_us` measures the wait for the HTTP/3 body's `send_data` and
  `finish`; it is not the response time nor a delivery confirmation by the peer.
  Every complete attempt, from the handshake to the response body, has a single
  deadline: the frame's deadline (`relay::frame_deadline`: (1 s + 50 ms per token + 2 s per
  image) x hops remaining; the prefill chunk has a liveness deadline of 10 min
  per hop, `frame_deadline_for_path`), fixed in `send_frame_or_tunnel` and read by
  the transports with `relay::frame_time_left`. There is no nested smaller ceiling; a peer that dropped is detected by
  the connection's life (`QUIC_MAX_IDLE` 1.2 s, TCP keep-alive of ~4 s in
  `peer_client::with_liveness`). The failure memories (route, QUIC quarantine,
  opening the persistent channel, admission) use the single window
  `peer_client::FAILURE_MEMORY` (5 s). A peer in `tunnel_first` has its direct path
  probed in the background every `DIRECT_REPROBE_INTERVAL` (5 s,
  `relay::spawn_direct_reprobe`); a success erases the mark.
  Discovering a dead QUIC endpoint shares an aggregate budget of 300 ms across
  addresses, and a failure puts that address in quarantine for 5 s so a handshake is
  not repeated on every token; the healthy session is reused.
  If a request fails on a cached session, as with the `ApplicationClose` caused by
  the peer restarting, the pool closes and removes that instance and tries a new
  handshake once inside the same budget. The quarantine records only the endpoint's
  final failure. The removal compares the cached instance so it does not erase a new
  connection created concurrently.
- `pkarr_node.rs` - key -> address resolver on the pkarr DHT: a signed `_pom`
  TXT with public multiaddresses and the keys of the relays holding a
  reservation; `publish`/`resolve`/`resolve_dial_addrs`. The live publisher is
  `gateway::PkarrPublishHandle`.
- `public_directory.rs` - the federated protocol of the public catalog. Complete
  records are signed by the network's principal, have a 16 bit proof of work, a
  monotonic sequence, a TTL of seven days and a deterministic merge. The local
  cache accepts at most 1024 records; replicas are only configurable HTTP(S)
  tickets and not authority. The signed record includes the description, rules and
  channels in Markdown together with the invitation, and `relay_hints` (`pk:` keys
  only). The record carries no address: the network is found by the
  publisher's key. It also carries `accepted_enrollments`, the enrollment roles
  the publisher advertises as open (empty = not accepting new members); the
  field is signed with the rest of the record. A private node queries those tickets by egress, but does not use its
  private swarm as public discovery.
- `tls.rs` - the self-signed certificate derived from the `.pom-node.key` and the
  `ServerCertVerifier` that checks `SHA256(pubkey) == node_id`. Nothing goes to
  disk; there is no CA. See `docs/subsystems/p2p-network.md`, section 5.
- `tls_policy.rs` - the ratchet: the Announce's `tls` field is a trigger, and only
  the authenticated read of `GET /api/cluster/tls` on the principal writes the
  policy. A failed read preserves the local value.
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
  principal validates an active API key and `blocked_nodes` before sending to other
  `node_id`s.
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
- `route_plan.rs` - the **per-edge route plan**: which transport (QUIC,
  HTTP/HTTPS or the libp2p tunnel) this node speaks with each peer, measured in the
  background and pinned. `RouteBook` (global to the process, `route_plan::book()`)
  keeps per `(node_id, Plane)` the list of measured routes in RTT order (a 2 ms tie
  decides QUIC, then HTTP, then the tunnel). Two planes per peer:
  `Inference` (prefill/decode) and `Gateway` (the return of the direct reply and of
  the remote-stream). `relayed_nodes` lists the peers reached only by a circuit on a
  relay, rebuilt on each turn of the planner from `Network::libp2p_links`.
  `spawn_planner`, started by `Network::start`, measures every edge without analysis,
  when beaten (3 min) or when the peer changed addresses, ports, `deploy_id` or layer
  range - which is why a deployment that becomes `serving` already triggers the
  analysis before the first request. `measure_edge` probes in parallel with the real
  transport: `GET /health` over HTTP with the pinned client, an HTTP/3 POST over the
  QUIC endpoint (any status proves the path) and `/health` through the tunnel; two
  samples, the smaller counts. `record_failure` repeats the route once, then moves to
  the next measured one and, with the list exhausted, erases the edge and wakes the
  planner (reanalysis). `begin_use` returns the `RouteUse`
  guard every frame and every return delivery holds: with a frame in flight or
  until `PIN_GRACE` (60 s) after the last one, the planner neither probes the
  edge nor swaps its route, and `active` ignores `PLAN_TTL`; only the frame's
  own failure swaps it. An empty analysis keeps the existing route (renewing
  its deadline, so `active` still returns it) but leaves the edge due, so the
  analysis repeats after `EMPTY_RETRY` (5 s); only an exhausted `record_failure` erases it. The tunnel probe does
  not count the tunnel's own "nothing is listening" 502
  (`infer_tunnel::NOTHING_LISTENING_STATUS`) as a route: a peer measured while
  its server was down used to get the tunnel as its only route for a whole `PLAN_TTL`.
  `establish_budget` gives the connection
  budget: 3x the measured RTT between 100 ms and the ceiling, 150 ms without a
  measurement. `snapshot`/`signature` feed the `route_plan` field of
  `/api/ui/cluster`.
- `Network::gateway_bases` - a peer's gateway addresses in the order worth trying:
  the host that already answered (`RouteTable::last_success`, compared by host
  because it may have been the inference port), then the ones that did not fail,
  then the discredited ones. Whoever can only use one address (a download, an
  offer's `from_gateway`) uses the first.
  `inference_bases` does the same with the inference addresses (the probe of
  admission to the plan), and `order_gateway_bases` orders a list assembled by the
  caller (the policy ratchets, with the announced scheme).
- `Network::peer_route` / `libp2p_links` - the route in use with each peer for the
  participants screen: direct HTTP that answered, a direct libp2p connection
  (marked when it was born of a hole punch) or only a circuit through a relay. The
  connections come from `note_libp2p_link`/`drop_libp2p_link`/`mark_hole_punched`,
  called by the swarm loop in `discovery.rs`; see `p2p-network.md`, "Diagnosis".

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
  inference port and measures the RTT over libp2p when the peer announces the tunnel.
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
  `PeerInfo::libp2p_dialable_addrs` reduces the set to one canonical endpoint,
  preventing the pipeline or the Chat from turning old listen addresses into a
  tunnel fan-out.
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
  the address observed by `identify`, the other local interfaces and the
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
  (`None` = a node without TP). Each `TpMember` carries the `PeerId`, the UDP
  address candidates and the capability of the preferred libp2p tunnel across
  networks. `set_tp`/`tp_identity` set/read without a restart;
  `PeerInfo::is_tp_worker` is the predicate the planner uses to exclude workers from
  the chain.

`discovery.rs`:
- `Discovery::new` - builds the swarm with TCP **and** QUIC on port UDP+10000
  (four listen addresses: v4/v6 x tcp/quic-v1), dials the bootstrap converted by
  `udp_to_libp2p_multiaddr` and the relays of `--relay-peers`. QUIC is there for
  DCuTR: a TCP hole punch depends on simultaneous open, which CGNAT refuses.
  `DEFAULT_RELAY_NODES` is empty - the IPFS bootstrappers that were there do not
  grant a Circuit Relay v2 reservation and the addresses were already obsolete, so
  no reservation was accepted and DCuTR never ran.
- `handle_event` - a Gossipsub Announce becomes a `PeerInfo` using
  `addr_candidates.first()` (the literal `0.0.0.0` only survives if nothing was
  announced) and registers only the canonical endpoint as the multiaddress
  `/p2p/<peer-id>` before warming the tunnel connection; mDNS discovers on the
  LAN and is always on. Identify does not write `listen_addrs` into the address
  book; a peer announcing `relay::HOP_PROTOCOL_NAME` goes through
  `reserve_circuit` if it is a dedicated relay, a relay outside the network,
  or a member announcing a relay offer (`member_relay_offer`);
  AutoNAT `Public` becomes `Swarm::add_external_address` and tells
  `RelayGate` the node is reachable (`Private` tells it the opposite); a file
  piece arriving through the tunnel is refused with 421 on a connection that
  may not carry it (`Network::connection_carries_transfers`) and with 416
  without a bounded range; `ListenerClosed` ends
  the reservation of the relay owning the listener; requests received through the
  tunnel are forwarded in separate tasks so a long prefill, decode or TP does not
  stop the loop that drains GossipSub and the other protocols' answers;
  DCuTR/AutoNAT only log the rest. The request/responses of logs, audit and
  Network Chat use their own behaviours, separate from the inference tunnel. Process
  logs are accepted by every effective admin; audit events are also persisted by
  every effective admin and no received event is retransmitted. The Chat validates on
  the principal, relays without a secret and refuses relays that did not come from an
  administrator. The `select!` is biased to the swarm and the inference tunnel; Chat
  queues use `try_send`, discard under pressure and never wait on SQLite.
- `tunnel_peer_multiaddrs` - a peer's dialable addresses. Accepts a complete
  multiaddress (what `Announce.libp2p_addrs` carries, including a circuit) and the
  legacy `host:port`, derived by `udp_to_libp2p_multiaddr`. The final
  `/p2p/<peer-id>` is added only when missing: on a circuit the relay already
  appears before the `/p2p-circuit`, and duplicating the suffix would dial the relay
  instead of the destination.
- `udp_to_libp2p_multiaddr` - a `host:port` UDP -> a `/ip4`/`/ip6`/`/dns`
  multiaddr on the TCP port. It fixes the IPv6 bug where it was glued into an
  `/ip4/`, the parse failed, and it was dialed without a single word in the log.
- `TraversalStatus`/`traversal_status` (in `network.rs`) - the AutoNAT state,
  published addresses, accepted reservations and the hole punch count with the last
  failure. `discovery.rs` feeds it (`set_nat_status`,
  `remember_reservation`, `remember_hole_punch`) and `/api/ui/cluster` publishes it
  in the `traversal` field, which also enters the cache's fingerprint.
- `publish_swarm_addrs` - collects from the swarm the confirmed external
  addresses, listeners with a public IP and circuit listeners, adds `/p2p/<self>`
  and writes it into `Network::set_libp2p_addrs`. Private/loopback/unspecified stays
  out; a circuit always enters, because the IP there is the relay's. Called on
  `NewListenAddr`, `ExpiredListenAddr`, `ExternalAddrConfirmed`,
  `ExternalAddrExpired`, AutoNAT `Public` and an accepted reservation.
- `RelayPolicy`/`apply_relay_policy` - where this node holds reservations:
  on every dedicated relay (key in `relay_peers`) and, **beside** them, on up
  to `MEMBER_RELAY_RESERVATIONS` (2) member relays - cluster nodes whose
  `Announce` carries a relay offer. Every 5 s the loop hands the member list
  to `RelayGate`, asks connected dedicated relays without a reservation again
  and reserves on announced members. `Network::set_dedicated_relays` feeds
  `PeerRoute.relay_kind` and the transfer rule.
- `network::RelayGate` - whom this node relays for, consulted by the relay
  server on every reservation (`relay::RateLimiter`): everyone with
  `relay.everyone`/`--serve-relay`; the admitted members of its cluster when
  the internet reaches it and `relay.cluster` is on (the default); nobody
  otherwise. `offer()` is what goes into the `Announce` (`RelayOffer`, with
  whether transfers may cross it). The options are atomics: `set` changes
  them live from the screen, `RelayOverrides` keeps what the command line
  fixed for the run, and `status()` is what `/api/ui/network/relay` answers.
- `Network::relay_carries_transfers`/`connection_carries_transfers`/
  `transfer_links` - the rule of files and relays: a direct connection carries
  a file; a circuit only through a member relay that allows it; never a relay
  in `relay_peers` or one that is not a member.
- `reserve_circuit` - asks `listen_on(<relay>/p2p-circuit)` and keeps the
  `ListenerId`s in `RelayPolicy::listeners`. The relay's address
  comes from `relay_reservation_addrs`: the configured list, a live announcement
  and, last, the address this node dialed (`dialed_addrs`, recorded on the
  `ConnectionEstablished`) - that is the case of a dedicated relay outside the
  network. Never from the identify's `listen_addrs`. Without a published circuit
  there is no relayed connection, and without it the DCuTR has no way to coordinate
  the punch.
- `observed_host_port` - the inverse of the previous one: it takes this node's
  `host:port` out of the `identify`'s `observed_addr`. It is the "what is my public
  IP" without a STUN dependency. It refuses a private/CGNAT address
  (`addrs::is_private_or_local`) and anything that is not
  `/ip4|ip6/.../tcp/<port>`.
- `announce_message_id` - a deterministic id of the topic + payload. The gossipsub
  now uses signed authentication and strict validation; `handle_event` checks that
  the `libp2p_peer_id` declared in the `Announce` is the authenticated `source`
  before inserting the peer. That transport bond does not replace the Ed25519 chain
  of administrative authority.

`addrs.rs`:
- `filter_candidates` - discards loopback/link-local/deprecated, stable before
  temporary, a ceiling of 2 per family (macOS creates several IPv6 on its own and the
  `PeerList` is already at the MTU limit); an address pinned by the operator comes
  first and does not count toward the ceiling. `Network` separates those candidates
  into public and private: the current public one comes before the one observed by
  `identify`, and the observed one stays a fallback for NAT. The comparison with
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
  Measured on 2026-08-16 on a laptop: the first `identify` answer came through a
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
- `Identity::libp2p_keypair` / `peer_id` - the same seed as a libp2p identity.

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
  `libp2p` is the normal measurement and `direct-health` identifies the probe
  fallback. `fresh_snapshot` is the only RTT source for selection; `snapshot_with_age`
  keeps the age for diagnostics. The renewal happens every 120 s, independently of the
  5 min re-measurement of addressing. If the peer announces the tunnel but the swarm
  is not ready yet, the inference route's HTTP probe stays a `direct-health` fallback,
  instead of erasing the measurement. The authenticated endpoint
  `/api/ui/network/rtt` publishes only fresh measurements with `source_node_id`.

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
- `peer_id_from_uri`/`uri_from_peer_id` - a `pk:` key <-> `PeerId` (the same
  Ed25519 key).

## Links

- **Called by**: `main.rs` (builds `Network` + `Discovery`, resolves
  relays/pkarr, publishes to the gossipsub every 5 s the same announcement as the
  UDP, via `Network::gossip_announce_bytes` -> `self_announce`, with or without
  assignment; `spawn_pkarr_bootstrap` dials `--pkarr-peers`, relays and trust chain
  principals by key),
  `routing/pipeline.rs` (`resolve_step_url` uses `RouteTable`), `gateway.rs`
  (consumes `PeerInfo`/`alive_peers`).
- **Calls**: `network.rs` ← `identity.rs` (via main), `addrs.rs`
  (`self_addr_candidates`), `route.rs` (`Network.routes`); `discovery.rs` →
  `network.rs::insert_peer`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.