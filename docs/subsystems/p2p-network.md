# Subsystem: P2P network and discovery

The network subsystem solves the problem of a pom node finding the other nodes
of the cluster and announcing itself to them. There are two worlds in the same
directory (`crates/node/src/network/`): the native UDP transport
(`network.rs`, `addrs.rs`, `identity.rs`, `route.rs`), which is the heart of the
LAN, and the iroh/pkarr path (`p2p.rs`, `discovery.rs`, `pkarr_node.rs`),
designed for the internet (an iroh QUIC endpoint dialed by key, relays with
hole punching, iroh-gossip, mDNS, pkarr DHT). The house rule is that the address decision belongs to
whoever is going to dial, not to whoever announced. The operator's
`advertise_addr` is a priority hint (it goes in front of the candidates and
accepts a DNS name), never an exclusive endpoint; `addr_candidates` and
`RouteTable` keep the fallback (see `crates/node/src/network/AGENTS.md`).

`main.rs` builds `Network` and `Discovery`; `backend/gateway/` and
`routing/pipeline.rs` consume `PeerInfo` and `RouteTable`.

## 0. Identity is a key, the address is resolved

**A node is an Ed25519 key** (`.pom-node.key`, see section 4). From it come the
`node_id` (`SHA256(pubkey)`), the iroh endpoint id (the public key itself), the `pk:` key of the pkarr
record, the authority key of the trust chain and the TLS certificate. **An IP
never identifies a node**: nodes behind NAT or with a dynamic IP change
address, and an IP written into a document goes stale. Therefore:

- the trust document (`TrustNetworkMetadata`) and the public directory record
  (`PublicDirectoryNetwork`) carry **keys**, never addresses: the principal is
  found by its own key, and the dedicated relays by `relay_hints` (relay URLs
  whose host is a DNS name, never an IP - `p2p::relay_hint`);
- the Public networks screen lists nobody's IP; reachability is resolved by the
  node (`GET /api/public/networks/locate`);
- addresses only exist at dial time: in the `Announce` (read only by admitted
  members), in the pkarr record and in the route tables.

**Who does what:**

| Mechanism | Used for | Not used for |
|---|---|---|
| UDP broadcast + mDNS | finding peers on the LAN, and the wizard handshake | anything outside the LAN |
| pkarr (Mainline DHT) | resolving a key into "where to dial now", cold: the trust document's principal, `--pkarr-peers`, the directory publisher | steady-state routing; identity; private addresses (never go to the DHT) |
| iroh endpoint (QUIC, relays, hole punching, iroh-gossip, request/response ALPNs) | announcements across sites, NAT traversal, control and inference tunnel when there is no direct route | - |
| Direct HTTP/QUIC | data plane when `route_plan` measures a direct route that answers | - |

**Connection ladder to a peer** (measured per edge in `route_plan`):

1. **Direct** - candidates announced or resolved through pkarr, over QUIC and
   then HTTP; or a direct endpoint path.
2. **Through a relay**, with iroh punching the NAT over it right away: a
   dedicated relay (`relay_peers`, which includes the document's
   `relay_hints`), one of n0's public relays (unless `--no-default-relays`),
   or a member relay - another node of the same cluster that the internet
   reaches. iroh keeps one connection per peer and moves it from the relay
   path to a direct one when the punch closes; the edge is measured again.
   Member relays are kept **beside** the public ones, not instead of them,
   because a public relay carries no file transfer (see "Who relays for
   whom").

## 1. Overview and topology

- **UDP path (LAN):** dual-stack UDP socket, broadcast `255.255.255.255`,
  explicit bootstrap and periodic gossip. It works with no infrastructure at
  all on the local network and is the only transport that always talks (it
  announces even without a model).
- **iroh path (internet):** an iroh endpoint on UDP `p2p_port + 10000` under
  the node key, with iroh-gossip to exchange `Announce`, mDNS and pkarr to find
  addresses, relays to cross NAT and iroh's own hole punching. An embedded
  relay server listens on TCP on the same port number. See section 3.
- Both paths end in the same place: `Network`'s `peers` table
  (`HashMap<node_id, PeerInfo>`). The gossip path enters through
  `Network::insert_admitted_peer`.

### Admission before the `peers` table

Transport discovery is not network membership. The process can learn an address
through UDP, mDNS, pkarr or a gossip link, but none of those
sources registers a `PeerInfo` without local proof of admission. The same rule
applies to UDP `Announce`, gossip `Announce`, `PeerList` and direct entry
through `insert_peer`; policies, tunnels, routes, UI and pipeline only consume
the table after that point.

- On a private network, a non-empty `connection_key` selects the proof through
  `/handshake`. The key never leaves the node: it sends its identity and a
  fresh 32-byte challenge, and the target answers with an HMAC-SHA256 of the
  challenge and both node ids, keyed by the connection key and signed with the
  target's node key. Only an answer the prober can check - the signature
  against the announced key, the HMAC against its own key - admits the target;
  what the target says about itself counts for nothing. A wrong key, a missing key, a peer without a
  gateway or a timeout are refused. The key does not appear in the `Announce`.
  Once the direct addresses are exhausted, the same probe goes through the
  p2p tunnel; for a peer behind NAT/CGNAT that is the only path that gets
  there, and without it admission would never complete. Direct probes get 2 s
  each; the tunnel one, 10 s, because it may have to open a connection through
  a relay. The verdict is cached for 30 s (refusal: 5 s). A refusal only
  counts when it is authentic - through the tunnel (QUIC under the peer's key) or
  over HTTPS with the certificate pinned to the `node_id`; a clear-text HTTP
  answer does not prove who answered. Without an authentic answer, a peer
  approved less than 10 min ago (`ADMISSION_UNREACHABLE_GRACE`) stays admitted
  and the probe repeats every 5 s; without recent proof, it stays out.
- **The gossip `Announce` never probes inside the gossip loop.** Under libp2p
  the tunnel request was served by that same loop, so a probe awaited there
  waited for itself until the 2 s timeout and always failed; with iroh the
  protocols are served on their own tasks, but a probe per announce would
  still stall the gossip receiver. `discovery.rs` consults
  `Network::admission_without_probe` (public network, empty key or cache);
  without a verdict, `Network::admit_in_background` runs the probe in another
  task, one per peer, and the current announcement is dropped - the next one,
  within 5 s, finds the cache. UDP announcements still probe inline, in the UDP
  receive loop, which is not the tunnel's.
- On a public network, the local anchor must be valid, and the strictness
  depends on `network.access_policy`. In both cases the announced `pubkey` must
  derive the `node_id` and be the announced `endpoint_id` (it is a single key,
  see section 4), and the gossip `Announce` comes signed by that key
  (`discovery::seal_announce`). The admin's
  local `connection_key` takes no part in that admission and is not sent to
  peers.
  - `authorized` (default): besides the identity binding, admission requires
    the exact tuple `{node_id, peer_id, pubkey}` present as
    the current principal or a non-demoted secondary grant. The `Announce`'s
    `cluster_id` is only a chain selector and can be forged; alone it does not
    pass the gate. Without local trust, no public peer is a member.
  - `anyone_with_anchor`: the operator declared that holding the matching
    anchor is already proof of membership. Admission requires a valid local
    chain anchored exactly on the announced `cluster_id`
    (`AdminTrust::anchors_cluster`) plus the identity binding above - **without**
    an authority grant. That is what lets a regular worker node, co-located or
    not, that only has the anchor and never got a grant, enter `peers`. An
    `Announce` without `cluster_id`, with a `cluster_id` different from the
    anchor's, or with an inconsistent identity is still refused.
- `connection_verified` is a mark produced by the local gate. The value coming
  from `PeerList` is never accepted as proof, and the transitive list is
  revalidated before inserting an unknown peer or updating an existing one.

A public stranger can still show up in the endpoint's events, because mDNS and
gossip need to keep working to find addresses. It is
silently dropped at the application boundary, without logging credentials or
answering with failure details, and does not show up in `/api/ui/cluster`, in
the routes or in inference planning. The private probe has a short timeout and
caches the result per identity and address for 30 seconds; refusals are cached
for 5 seconds so an announcement storm is not possible.

Under `authorized`, the `cluster_id` alone does not protect against forgery:
protection comes from comparing the tuple with the signed chain loaded by
`AdminTrust`; announcing the right id without the matching public key and
grant does not pass the gate. Under `anyone_with_anchor` the operator swaps
that requirement for holding the anchor of the `cluster_id`, and what is left
protecting against a stranger is the identity binding and the gossip
`Announce` signature. In both cases the `admin-control-signature-scope`
decision applies: inference frames get no administrative signature, but
admission control uses the chain of trust before any routing.

## 2. UDP protocol

### Messages (`P2PMessage`, `network.rs`)

A JSON tagged enum (`serde(tag = "type")`) over UDP:

- **`Announce`** - self-introduction: `node_id`, `pubkey` (the node's only
  key, which derives `node_id`, `endpoint_id` and the pkarr key),
  `cluster_id` (if there is an anchored public chain), `endpoint_id`,
  `endpoint_addrs`, `addr`, `addr_candidates`, `model`, `start_layer`,
  `end_layer`, `total_layers`, `memory_mb`, `timestamp`,
  `inference_port`, `inference_quic_port`, `inference_frame_capabilities`,
  `gateway_port`, `gateway_quic_port`, `profile`, `hostname`, `capabilities`
  (the full `ModelCapability` catalog), `disk_total_mb`, `disk_free_mb`, and
  `tp` (the tensor parallelism `TpIdentity`: `{role, group_id, size, rank,
  rank0_url, members}`, `None` = node without TP - see `network.rs`).
  `rank0_url` is the primary's URL where the ranks do the collective's
  `POST /v1/tp/reduce`. Each `members` entry carries URL, `node_id`, endpoint
  id and tunnel capability. `idle` says this peer is alive with the model
  unloaded; it does not mean the deployment went offline. New fields have
  `serde(default)` so an old node stays parseable in a mixed-version cluster.
- **`PeerRequest`** - asks a known node for its peer list.
- **`PeerList`** - answer with `Vec<PeerInfo>`, sliced by MTU (see below).
- **`Heartbeat`** - liveness; updates `last_seen`. Defined and handled on
  receipt, but no code path emits it today (the 5 s `Announce` plays that
  role).
- **`LocalDiscover`** - mDNS-style discovery on the LAN. Also only has the
  receiving side (answers with `PeerRequest`).
- **`LocalAutoDiscovery`** - the explicit, temporary handshake of the first
  setup. `Probe` and `Offer` only carry the challenge, ephemeral keys and the
  public trust document. `Proof` binds the request to both ends; only after it
  does the admin send `Secret`, encrypted with X25519 and ChaCha20-Poly1305,
  containing the `connection_key`. There is no answer and no sensitive
  material for a passive listener.

`endpoint_id` is the hex of the node's public key - the same string as
`pubkey`, because the iroh endpoint runs under the node key. `endpoint_addrs`
is the list of addresses the endpoint is reachable at: direct `ip:port` (the
endpoint's UDP port, public addresses only) and the URLs of the relays it is
connected to. The relay URL is the only address a node behind CGNAT has to
offer, and the `host:port` convention does not carry it. It comes from
`Network::set_endpoint_addrs`, which `Discovery` updates on every policy round
(5 s), with the operator's pin (`advertise_addr` on the endpoint port) in front;
the new announcement goes out in the next gossip. Whoever dials
(`PeerInfo::endpoint_dialable_addrs`) adds the peer's UDP candidates on the
endpoint port, which is how a LAN or VPN peer is reached where mDNS does not
cross.

`PeerInfo` mirrors the `Announce` fields plus `last_seen`,
`connection_verified` and `api_keys_active`. `PeerInfo::is_alive()` compares
`last_seen` with `PEER_TIMEOUT_SECS` (30 s).

The `Announce` is not the source of truth for the network metadata. After
validating that the sender is the anchored principal, the node triggers a
signed read of `/api/cluster/network-settings` - through the principal's
addresses and, last, through the p2p tunnel, like the other ratchets - and
persists the full `NetworkSettings` for the Network Information screen. Until
it succeeds, the read repeats on every Announce from the principal; the failure
warning only goes out on the 1st, 2nd, 4th, 8th... consecutive time, with the
tunnel's reason. On a public network the
answer does not carry the `connection_key`; the public record stays limited to
the invitation and discovery.

### Signed announces

UDP authenticates nothing, so every `Announce` carries `signature`: Ed25519 by
the node key over the announce serialized without it, under the label
`pom-announce/v1` (`network::sign_announce`, signed by the only builder,
`self_announce`, with the key `main.rs` hands over through
`Network::set_announce_key`). The receive loop drops an announce that arrived
over UDP when it is unsigned, when the signature does not match the announced
`pubkey`, when that key does not yield the announced `node_id`, or when its
`timestamp` is more than `ANNOUNCE_FRESHNESS_SECS` (300 s) from the local
clock (`verify_udp_announce`). Without it, a machine on the LAN could announce
a member's identity at its own address, relay the admission probe to the real
member, and take its place in the peers table. The gossip path needs no such
check: its announce comes sealed by the sender's endpoint key
(`open_announce`), checked against the declared one.

A `PeerList` is another node's snapshot and carries no signature. It never
moves the addresses, ports, `pubkey` or endpoint fields of a peer whose own
announce arrived in the last `FIRSTHAND_WINDOW` (30 s, `Network::firsthand`);
the rest of the snapshot still applies. A peer known only through `PeerList`s
still takes them whole, which keeps transitive discovery behind NAT working;
a forged address there reaches only a node the TLS pinning then refuses.

### Lifecycle

1. **Bind:** `Network::new` builds a dual-stack `[::]:<port>` socket by hand
   with `socket2`, turning off `IPV6_V6ONLY` **before** `bind` (afterwards it
   has no effect; without IPv6 in the OS it falls back to `0.0.0.0`). Default
   port `DISCOVERY_PORT` = 42777. If the configuration asks for port `0`, the
   OS picks an ephemeral port and the node keeps that real port in the
   announcement; the endpoint port also derives from it. Tests use `Network::testkit`,
   which only binds `127.0.0.1:0` and injects a loopback target for the local
   announcement; `testkit_with_broadcast_addr` accepts an explicit loopback
   endpoint and refuses the global broadcast.
2. **Bootstrap:** `Network::start` calls `bootstrap_discovery` right at the
   start (sends `PeerRequest` to each configured bootstrap).
3. **Gossip (5 s):** every `GOSSIP_INTERVAL_SECS` the node reannounces
   unconditionally (bootstrap + live peers + local broadcast), runs
   `discovery_sweep` and expires the dead. When there is an `advertise_addr`,
   the announcement only contains that endpoint, not the enumeration of local
   interfaces.
4. **Discovery sweep:** `discovery_sweep` asks peers again every 6 ticks (30 s),
   rotating `SWEEP_FANOUT` = 3 per sweep starting at `k * 3` over the list
   sorted by `node_id`. That is what learns transitive peers outside the LAN;
   without it discovery would be a snapshot of startup.
5. **Timeout (30 s):** `cleanup_dead_peers` removes peers with `last_seen`
   older than `PEER_TIMEOUT_SECS`.

The `LocalDiscoveryRuntime` service does not replace normal gossip. During the
first setup it receives `Probe` on the local UDP port and stays active only
until configuration finishes. After setup the `local_auto_discovery` setting
starts `false`; the administrator can turn it on manually in Preferences, and
the process keeps the trust document and the key only in memory while it is
answering. The `Offer` only exposes the fingerprint and the document's size
metadata. After the `Proof`, the payload with the trust document and the
`connection_key` is encrypted with ChaCha20-Poly1305 and sent in fragments of
at most 640 bytes, so each UDP envelope stays within the 1200-byte budget. The
node only reassembles and validates the fingerprint after receiving every
fragment; the accepted material is capped at 512 KiB. Turning it off clears the
handshake sessions and the sensitive material.

### Handling `Announce` on receipt

- An `addr` starting with `0.0.0.0` is rewritten with the packet's source
  address (the "sentinel" of whoever announces without an address). The
  effective source address is added to `addr_candidates` through
  `merge_observed` only when the announcement did not bring an explicit list.
  With `advertise_addr`, the announced list is authoritative and gets no LAN,
  bridge or observed address as an extra candidate.
- `PeerRequest` answers with an immediate `Announce` (even without an
  assignment) and with the `PeerList` of live peers, sliced to fit
  `MAX_DATAGRAM_PAYLOAD` (1200 B, the floor IPv6 guarantees; `PeerList` grows
  ~530 B per peer).
- A `PeerList` is also a state update: if the snapshot's `last_seen` is newer,
  the existing record is replaced to propagate model and range changes between
  transitive peers, keeping the addresses of a peer heard first hand (see
  "Signed announces"). Older snapshots do not revert a recent announcement; local
  verification is kept on refresh, and the local address candidates are
  inherited **minus the ones this node already proved dead**
  (`RouteTable::endpoint_is_discredited`). Inheriting the whole list made a dead
  candidate immortal - see "Address that stopped answering".
- A detached node (`is_detached`, `--no-discovery`) neither learns nor answers
  anything; the socket stays bound, and `rejoin` redoes bootstrap + announce.

### Address that stopped answering

**Temporary IPv6 addresses rotate on their own.** Privacy extensions
(RFC 4941) generate a new address from time to time and retire the previous
one; macOS also keeps the old one marked `deprecated` for a while before
removing it. The cluster has to cope with this without configuration: the
address a peer announced yesterday may exist on no machine today. Pinning
`--advertise-addr` is **not** the answer - it is a per-machine patch for a
defect that belongs to all of them.

Without a pin, the announcer already handles the rotation: `self_announce`
enumerates the interfaces once per announcement and `filter_candidates` drops
`deprecated` and puts stable before temporary. That enumeration is a local
read, with no network probe; it happens in the gossip every 5 s and also on
immediate announcements. With a pin, that list is not published. The one that
**stores** the address did not cope:

- `PeerInfo::gateway_url()` was just `peer.addr`, one address and no plan B,
  while `inference_url()` already preferred `addr_candidates`. The whole control
  plane (Dashboard, `deploy`, transfer, `devupdate`) dialed that single
  address; once it died, the Dashboard showed "did not answer `/api/ui/node`"
  forever, with the peer alive at another address already inside
  `addr_candidates`.
- The `RouteTable` probe measures `inference_candidates()` - another port,
  another URL space. It would never catch a dead gateway, and the Dashboard
  never asked the failure memory.
- `PeerList` inherited the whole local candidate list on top of the new
  snapshot, so the corpse came back every gossip round and the set only grew.
- `invalidate` erased failures by `node_id`. The IPv6 rotation - the event that
  **kills** the address - was also what erased the proof that it died.

Today:

- `gateway_candidates()` returns one URL per known address, with `addr` first
  (a known fact: it is where the packets came from) and the announced
  candidates after; `gateway_url()` is only the first of them. The same
  discipline as `pipeline::resolve_step_url`. The promotion control plane uses
  the whole list in both directions: the first target that fails on transport
  gives way to the next. The authenticated TLS, logs and Chat ratchets also use
  the whole list: an HTTP 401/403 answer can come from a ghost process at that
  address and does not prove the principal refused the key. A future
  announcement should update that list; the stored address is not a permanent
  anchor.
- The inference path (`inference_candidates`) and UDP discovery
  (`dialable_addrs`) also keep every valid address. The transport sorts the
  candidates by failure memory, tries the next when the previous fails and lets
  the failure expire; dynamic DNS is resolved again on each attempt, so a
  hostname change needs no restart.
- The cluster read (`/api/ui/cluster`) walks those candidates and feeds the
  **same** failure memory as the inference path: `mark_failure` on a transport
  error or timeout, `mark_success` on a good answer. An invalid answer or an
  HTTP error does **not** discredit the address - whoever answers 401 or 500 is
  alive there, and hiding a payload defect behind a route search would trade
  one defect for another.
- `RouteTable::usable` returns the candidates not yet discredited. Empty is
  information: it means "every known address of this peer failed".
- A route failure is tied to the endpoint, host and port. A failure on the
  inference HTTP URL does not erase the P2P port's `addr_candidates` on the same
  host. That keeps the P2P transport eligible when only the inference service
  had a hiccup, without stopping a new `Announce` from replacing old addresses.
- That is where `Network::request_peer_addresses` comes in: it sends
  `PeerRequest` to the bootstrap nodes and to the other live peers (whoever
  answers sends `Announce` + `PeerList`) and **prunes the discredited
  candidates from the local entry**, so the next `Announce` repopulates it
  instead of adding to the corpse. Limited to once per `REDISCOVERY_INTERVAL`
  (30 s) per peer, because the Dashboard rereads the cluster on a short cycle.
  No LAN broadcast: the peer is still alive and its `Announce` arrives every
  5 s anyway, and broadcasting from the route suite would break the rule of
  never speaking real UDP in tests.
- The warning "Reencontrando ... nenhum endereço conhecido responde"
  ("rediscovering ... no known address answers") can come from that pruning.
  The investigation found a link: the old comparison treated an HTTP failure on
  any port as a failure of the whole host, also removing the P2P address, even
  when the inference service or the tunnel on another port still answered. Now
  the pruning compares host and port. That rediscovery does not erase a KV
  session by itself, but no longer removing a live P2P port keeps a transient
  HTTP failure from contaminating the candidates used by the pipeline.
- Meanwhile `/api/ui/cluster` returns `rediscovering: true` for the peer, and
  the screen says "rediscovering" instead of "nothing below can be read". The
  distinction is a product one: one is work in progress that resolves itself,
  the other is a dead end. **The promise has a deadline**: after
  `REDISCOVERY_GIVE_UP` (90 s, three questions) without a good read, the peer
  goes back to `unreachable` saying none of the known addresses answered and
  the network returned no new one. A peer that announces over UDP and does not
  answer HTTP exists, and calling it "rediscovering" forever would bring back
  the dishonest warning under another word. A successful read
  (`Network::note_peer_read`) ends the search, so the next rotation starts from
  scratch.
- The Dashboard's coverage comes from gossip, so an unreadable peer keeps
  contributing layers - and it is honest that it does, because its inference
  server may be perfect and only the control address died. On a request's
  admission, however, the gateway also reads the current range from the infer
  server's `/health`. That read can correct the late P2P snapshot for planning,
  without making `/health` the source of truth for the visual coverage. What
  must not happen is for it to be invisible: `model_inventory[].unreadable_nodes`
  names those peers, and the screen removes the "complete" green while the list
  is not empty.
- `cleanup_dead_peers` calls `RouteTable::forget` on the expired peer, so a
  node that comes back with the same address is not born discredited.

Proved by `network::tests::gossip_does_not_resurrect_an_address_already_proved_dead`,
`the_gateway_has_one_url_per_known_address_with_the_proven_one_first`,
`an_address_rotation_does_not_erase_the_proof_that_the_old_one_died` and by the
`webui::dead_address_tests` module, which brings up a real `/api/ui/node` and
makes the Dashboard recover the peer through the live address in a single read
- including `rediscovering_gives_up_and_says_so_instead_of_promising_forever`,
which pins the deadline.

### Dual-stack `normalize`

The socket is `AF_INET6`, and macOS refuses a `sockaddr_in` target with EINVAL.
Every send goes through `Network::normalize`, which maps IPv4 to
`::ffff:a.b.c.d`. Without it, discovery dies in one direction only (Linux
accepts the target, which is why the break looked one-sided). There is a test
that speaks real IPv4 (`ipv4_destinations_are_mapped_on_a_dual_stack_socket`).

### Advertised addresses (`addrs.rs` + `network.rs`)

- `self_addr_candidates` enumerates the interfaces (`ifconfig` on macOS,
  `ip -o addr` on Linux; Windows stays empty and falls back to the observed
  address). Without `ip` (a Docker image without `iproute2`) the list is also
  empty, with a single warning in the log: the node does not even announce the
  public IPv6, LAN peers keep the packet's private source and pass it on in
  `PeerList`, and a remote peer ends up dialing the LAN IP. It filters with
  `filter_candidates`: drops loopback/link-local/deprecated and virtual
  container interfaces (`docker0`, `br-*`, `veth*`, `cni*`, `podman*` and
  equivalents), keeps stable before temporary and caps at 2 per family. Private
  IPs on real interfaces stay valid for LAN clusters. The address pinned by the
  operator (`--advertise-addr`) comes first and does not count toward the cap;
  without a pin, the current public addresses come before the one the relays
  observed (the NAT report) and the other local addresses.
- Port `0` is never announced nor used as a target: explicit pins, observed
  addresses and peer candidates with that port are discarded. A peer without
  any dialable candidate does not enter the target table.
- `self_announce` builds `Announce.addr` and `addr_candidates` from the same
  snapshot of the interfaces: the pinned one, the first local public address
  with the discovery port, the one the relays observed, or the literal
  `0.0.0.0:<port>`. The snapshot is refreshed on each announcement, without an
  expensive network probe; in the gossip, the effective frequency is 5 s.
- The wildcard literal can still show up in the `Announce` as a listening
  sentinel when the node has no announceable address yet. Before any use as a
  target, `network::is_valid_destination` requires a non-zero port and rejects
  unspecified, multicast and broadcast addresses. So the sentinel never becomes
  a UDP target, an inference HTTP URL or an endpoint address; the following
  valid candidates stay available.
- `advertised_addr` normalizes what the operator types by hand into
  `host:port` (a bare IPv6 needs brackets to get a port).
- `self_announce` builds the `Announce`; it announces zero layers when there is
  no `LayerAssignment`, so a node configured through the UI stays visible.

**Advertised address precedence** (`announce_addr()`/`self_addr_candidates()`,
the same for UDP and gossip - both read from the same `Network`):

1. the operator's hint - `--advertise-addr` or `--public-addr` feed the same
   field, `--advertise-addr` wins if both come; it accepts an IP or a DNS name;
2. the current public address of the local interfaces, with stable before
   temporary IPv6;
3. the public address the relays observed (`Network::set_observed_addr`,
   called from `discovery.rs`'s policy round with the `global_v4` of the
   endpoint's NAT report and this node's UDP port - the same principle as
   STUN, over the connections the endpoint already keeps with its relays);
4. the other local interfaces of this machine;
5. the sentinel `0.0.0.0:<port>`, only when nothing above exists.

The operator's hint **is not exclusive**: `self_announce` and
`self_addr_candidates` put it in front and keep items 2 to 4 behind it, and
mDNS stays on. A dynamic IP that changed, or a DNS name that has not propagated
yet, does not isolate the node. The hint also goes in front of
`Network::endpoint_addrs` (same host on the endpoint port, `UDP + 10000`; only
an IP literal, since iroh dials addresses and resolving a name there would
block every announce), read on the spot: changing the pin in the UI reaches
the next `Announce` and the pkarr record without waiting for the policy round.

`addrs::is_private_or_local` decides whether an observed address is news: RFC
1918, **`100.64/10` (RFC 6598, CGNAT - and what Tailscale uses)** and IPv6
unique-local do not count, because they only prove whoever answered is on the
same private network.

Checked live on 2026-08-16, without restarting anything: pinning the address
through the Network screen (`POST /api/ui/network/advertise-addr`) reaches the
other nodes in ~10 s; clearing the field goes back to the interface's current
public address when there is one, or to the one observed from outside when the
node is behind NAT (then by libp2p `identify`, today by the relays); and a Tailscale address (`100.x.y.z`), previously
adopted as public, is no longer accepted.

**A candidate that is the node itself.** The target builders of a peer
(`gateway_candidates`, `inference_candidates`, `dialable_addrs`,
`quic_addr_candidates`, `endpoint_dialable_addrs`) refuse the `host:port` that is
this process (`is_self_endpoint`: the IP of a local interface, without
loopback, with this node's p2p port). Two nodes on a Docker bridge get the same
internal IP and the same port: seen from machine B, the `172.18.0.2:42778` that
machine A announced was machine B itself. The Dashboard showed the same machine
twice, because machine A's `/health` was answered by machine B, and the deploy
planned for machine A landed on machine B. The same IP on another port is still valid (two nodes on
the same machine), and the node's own entry in the table keeps every address.

**Who answered is checked, not just the IP.** The gateway and the inference
server stamp `x-pom-node-id` on every answer (`peer_client::stamp_node_id`). In
clear text, the control plane (`send_to_gateway_over`, through which Dashboard,
survey and deployment go), the inference HTTP frame, the route plan
measurement, the admission probe and the session lifecycle refuse an answer
stamped by another node (`answered_by_other_node`) and move on to the next
address. QUIC and HTTPS already pin the `node_id` in the certificate. An answer
without a stamp passes: whoever answers may not be a node. The p2p tunnel
carries no headers, but the QUIC handshake under the target's key already
identifies it.

### Dynamic routes (`route.rs`)

A node can have several valid addresses and which one works depends on who
asks. `RouteTable` keeps the measurement per candidate: it probes
`GET /health` on the **inference port** the pipeline will use, with a 3 s
timeout and remeasured every 5 min. Probing the gateway would be insufficient -
it can answer while the inference port swallows SYNs. When the peer announces a
different address, candidates or ports, the previous choice is invalidated at
once so an old URL is not reused during the 5-minute window. The probe client
accepts the peer's self-signed certificate and hostname; the inference clients
keep pinning. The measuring task reapplies the current TLS policy to the client
before probing. `choose_route` only switches the current route if the gain
exceeds 25% relative **and** 2 ms absolute (9.6 vs 7.2 ms between a laptop and a CUDA server must
not switch every cycle). `resolve` returns `None` = "not measured yet or
expired", and the caller falls back to the plan's `inference_url`.

The **failure memory** is the second half of the table, keyed by
`(node_id, URL)`: `mark_failure` records when a real request failed at that
address and removes the route at once, `mark_success` clears it, `is_failed`
answers, and `usable` filters a list of candidates. Since the key is the whole
URL, the inference port and the gateway port of the same host live together
without confusion - and it is on purpose that both live here: the control plane
suffered from the same defect as the inference path, and a second table would
be the same decision to make in two places. A slow probe does **not** write to
that memory: its timeout is much tighter than a real request's, and
discrediting every candidate would leave `resolve_step_url` with no route at
all. `invalidate` drops the choice and **keeps** the failures; only `forget`,
when the peer expires, erases them.

That addressing table is not the map's RTT table. `RttTable` measures
`GET /health` through the p2p tunnel's `TunnelRequest`, the same transport used
by inference frames, and keeps the local `origin -> target` direction. The peer
only publishes fresh measurements at `GET /api/ui/network/rtt`, authenticated
by the connection key. `snapshot_with_age` keeps the age for diagnosis and
`fresh_snapshot` is the selection's source. The gateway aggregates the reports
and only uses the `A -> B` measurement on the `A -> B` edge. If the peer does
not offer the tunnel, the report can mark `direct-health` as a fallback. The RTT
is refreshed every 120 s, independently of the 5-min addressing remeasure. If a
peer announces the tunnel but its endpoint is not ready to answer yet, the
inference HTTP route itself stays as the `direct-health` fallback; the
announced capability does not make the measurement disappear. No latency of
another peer is transitive or reused as if it were the real edge.

The P2P announcement is still data capacity: `capabilities` reports
model/range and `runtime_capacity` reports `n_ctx` and `max_new_tokens` only
after the `/health` confirmation. `profile` identifies the node's profile and
`pubkey` identifies the chain's signing key; neither grants a vote or
control power to an infer. An infer can be discovered and chosen as a head, but
gets no voice in control plane decisions. For the Chat, the
`/api/ui/infer/heads` endpoint only exposes the L0 candidates of the requested
model, not the whole network. The head is chosen by coverage and, among equally
useful ranges, by a weighted score of fresh RTT and in-flight load, with a
deterministic `node_id` tie-break; without a candidate the state is visible and
inference does not start.

## 3. iroh path (internet)

The internet path runs on an **iroh endpoint** (iroh 1.3): QUIC connections
dialed by public key, not by address. `p2p.rs` holds the transport pieces
(endpoint id, address text, request/response framing, outbound connections)
and `discovery.rs` builds the endpoint and everything that rides on it.

- **Identity.** The endpoint runs under the node key (`p2p::secret_key`), so
  its `EndpointId` is the node's public key and prints as the same hex as
  `pubkey`. There is no separate transport identity to bind.
- **Port.** The endpoint binds UDP `p2p_port + 10000` (`P2P_PORT_OFFSET`) on
  both stacks, falling back to IPv4 alone on a host without IPv6; the embedded
  relay server listens on TCP on the same number. Using the real bound UDP
  port, never the configured `0`. The Windows firewall rules
  (`windows_firewall::desired_rules`) open both, and `initial_ports` reserves
  both when it picks free ports.
- **Addresses.** An address is a hint, never an identity. Its textual form,
  used in the `Announce` (`endpoint_addrs`), the pkarr record and the screens,
  is one string per transport address: `ip:port` (`[v6]:port`) for a direct UDP
  address, the relay URL (`https://...`, `http://...`) for a relay
  (`p2p::endpoint_addr`, `p2p::addr_texts`). `Discovery` learns addresses from
  announces, pkarr and the operator, and feeds them to the endpoint through a
  `MemoryLookup`; iroh tries all of them at once and keeps what answers.
- **Relays.** The endpoint's relay map holds n0's public relays (iroh's
  default, off with `--no-default-relays`), the **dedicated** relays of
  `relay_peers` (relay URLs; `--relay-peers` adds to them, and the setup of a
  joining node copies the trust document's `relay_hints` in), and up to two
  **member** relays (`MEMBER_RELAYS`): other nodes of the cluster whose
  `Announce` carries a relay offer with a URL. The member relays are added and
  removed live (`Endpoint::insert_relay`/`remove_relay`) by the policy round.
  iroh keeps a connection to a home relay (the lowest latency) and publishes
  it as one of the endpoint's addresses. A dedicated relay is either a real
  iroh relay (`https://`) or a pom node with `--serve-relay` (`http://<ip>:<p2p_port+10000>`).
  QUIC address discovery is only asked of HTTPS relays (`relay_config`): a
  node's embedded relay does not serve it. `examples/relay-node/docker-compose.yml`
  is the example of a dedicated relay, outside the network it serves.
- **Hole punching.** iroh punches the NAT on its own, coordinated over the
  relay, and moves the same connection from the relay path to a direct one when
  it closes. There is no separate event to wire, nor a reservation to ask for:
  a node is reachable through its home relay from the moment it connects to it.
- **The embedded relay server** (`iroh-relay` with the `server` feature) runs
  on TCP `p2p_port + 10000`, plain HTTP: the payload it carries is end-to-end
  encrypted QUIC between the two endpoints, and a node on a bare IP has no
  certificate to offer. `RelayGate` decides, per connecting endpoint
  (`GateAccess`), who may use it - see "Who relays for whom". When its port is
  taken the node runs without it (`RelayGate::set_available(false)`) and
  offers nothing.
- **Who relays for whom.** `network::RelayGate`. A node relays for
  **everyone** when the operator asked for it (`--serve-relay`, or
  `relay.everyone` in the config file): that is a public relay. Otherwise a
  node the internet reaches (the NAT report says its public address is one of
  its own interfaces, or `--public-addr`) relays for the **admitted members of
  its own cluster** and nobody else; this is on by default and
  `relay.cluster: false` (`--no-cluster-relay`) turns it off. A node behind NAT
  relays for nobody. An iroh relay cannot tell an endpoint that makes it its
  home from one that only sends through it, so a member relay admits members
  only, sender included: a candidate that is not a member yet reaches a member
  behind NAT through a public relay (n0's or a dedicated one). The member list
  is refreshed from the peers table every 5 s. While it relays, a node says so
  in its `Announce` (`relay`, a `RelayOffer` with its `url`,
  `http://<public host>:<p2p_port+10000>`), together with whether file
  transfers may cross it (`relay.transfers`, on by default;
  `--no-relay-transfers`). The three options are also on the My machine screen
  and change live (`RelayGate::set`): new relay connections follow them at
  once, and the pkarr record's `is_relay` follows `relay.everyone`. An option
  given on the command line wins over the saved one for that run
  (`RelayOverrides`), and the screen shows it locked.
- **Reachability (the NAT report).** Every policy round reads the endpoint's
  NAT report (`Endpoint::net_report`, the `unstable-net-report` feature). Its
  `global_v4` is the public address the relays saw: it becomes the observed
  address (`Network::set_observed_addr`, with this node's UDP port), and when
  it is one of the machine's own interface addresses the node is `public` and
  reachable (`RelayGate::set_reachable(true)`); otherwise `private`. This
  replaces libp2p's AutoNAT.
- **Gossip** (`iroh-gossip`, topic `SHA256("pom/announce/1.0.0")`, messages up
  to 64 KiB). `main.rs` publishes the `Announce` every 5 s, with or without an
  assignment, and the frame is the same as UDP's
  (`Network::gossip_announce_bytes` -> `self_announce`). iroh-gossip
  authenticates only the neighbor that **delivered** a message, not its author,
  so every announce goes out sealed: the node key's signature over
  `"pom/announce-signature/1:" + announce`, then the announce
  (`seal_announce`). The receiver (`handle_announce`) verifies the signature
  against the `endpoint_id` the announce declares and drops it otherwise; that
  binding proves who wrote the announcement, but it is not the root of
  administrative authority: grants and commands still require the Ed25519 key
  and the local chain. The announce becomes a `PeerInfo` preferring
  `addr_candidates.first()` over `addr` (the `0.0.0.0` literal only survives if
  nothing was announced), goes through admission, and its addresses go into the
  address book; a tunnel-capable peer gets its tunnel connection warmed up in
  the background. Peers join the topic from three sources: admitted peers of
  the table (every policy round), mDNS discoveries and pkarr resolutions (the
  `dial_tx` channel).
- **mDNS** (`iroh-mdns-address-lookup`): LAN discovery feeding the endpoint's
  address lookup and the gossip membership. It is always on: `advertise_addr`
  is a priority hint, not isolation, and the LAN is the shortest path between
  two nodes on the same network.
- **Request/response protocols.** One ALPN per protocol, served by an
  `iroh::protocol::Router`: the inference tunnel (`/pom/infer-tunnel/1.0.0`),
  cluster logs, the network Chat, the lobby and the request audit. Each call is
  one bidirectional QUIC stream with the protocol's framing (`p2p::Codec`);
  the outbound connection is kept per peer and ALPN (`P2p::connect`), and each
  inbound stream is answered on its own task, so a long prefill forwarded to
  loopback holds up nothing else. Outbound calls are handed to `Discovery` over
  channels (`TunnelOutbound`, `LogOutbound`, `ChatOutbound`, `AuditOutbound`)
  and each runs on its own task.
- **Links.** Every connection, in or out, is followed by `link_tracker` until
  it closes: its selected path becomes a `P2pLink` (`Network::note_link`) -
  direct `ip:port (UDP)` or a relay URL - and a connection that started on a
  relay and moved to a direct path counts as a hole punch.

### Idleness and `reload_model`

The `idle` field rides on every `Announce` and `PeerInfo`, but takes no part
in liveness expiry: an idle peer stays alive, reachable and part of the plan.
On receiving a request for a plan that contains idle peers, the gateway reloads
the local peer and fires `POST /api/internal/reload_model` in parallel to the
other idle peers. That call uses the `connection_key` and the internal marker,
and only orders each recipient's local load: it returns at once with
`state: loading|ready` and the gateway repeats it until every peer is ready
(`gateway/generation.rs`, `reload_idle_pipeline`). A peer keeps `idle` set
while it loads and clears it when its model answers. There is no extra synchronous
connection on the normal inference hop and no timer to decide idleness; the
decision uses the recorded local activity and the existing monitor. Shutting
down a generation's QUIC endpoint is a local one-shot signal, with no
confirmation or barrier between peers. It waits at most 2 s for the transport
and aborts the task if the endpoint does not close, so the reload does not get
stuck on a lost notification or on `endpoint.wait_idle()`.

### pkarr (`pkarr_node.rs`)

pkarr is the **key-to-address resolver**, and only that. Each node publishes,
under its own key (the same as the `node_id` and the endpoint id), a signed
`_pom` TXT in the Mainline DHT: compact JSON (`a` direct endpoint addresses
`ip:port`, `x` relay URLs, `h` gateway port, `i` inference port, `r` is relay,
`g` geo label), trimmed from the end until it fits in 900 B
(`MAX_RECORD_ADDRS` 4, `MAX_RECORD_RELAYS` 3). iroh's own pkarr publisher is
deliberately not used: it signs a record under the same key, and one key holds
one record in the DHT, so the two would overwrite each other.

- **Publication** (`gateway::PkarrPublishHandle`): always, with or without a
  deployment - an idle admin is exactly who a new node needs to find.
  `desired_record` comes from `Network::endpoint_addrs` on the spot;
  `record_addrs` keeps the relay URLs and the public direct addresses and
  **leaves out every private address** (LAN, CGNAT, Tailscale, loopback): the
  DHT is public. `spawn_republisher` checks every 15 s and republishes when the
  record changed (home relay changed, public address observed, pin changed) or
  every 30 min (`PKARR_REFRESH`) so it does not vanish from the DHT.
  `set_geo_label` republishes immediately when the admin edits the network.
- **Resolution**: `resolve` returns `ResolvedPeer { endpoint_id, record }`; any
  failure becomes `None`, never an error. `resolve_dial_addr` turns the record
  into one `EndpointAddr` holding the direct addresses and the relays together
  (`dial_addr`); iroh tries them all. `endpoint_id_from_uri` converts a `pk:`
  key into an endpoint id (the same key).
- **Cold dial by key** (`main::spawn_pkarr_bootstrap`, every 30 s): resolves
  `--pkarr-peers` and the principals (current and last secondary) of the local
  trust chain that are not yet in `alive_peers`, and hands the address to
  `Discovery` (`dial_tx`), which adds it to the address book and asks the peer
  to join the gossip topic. It replaces the IP the trust document used to
  carry: a node that only has the document finds the principal by its key.
  `Discovery` does not redial a target that is already connected or backing
  off. A key that stays out of the table backs off on its own
  (`main::pkarr_bootstrap_wait`): 30 s after the first miss, doubling up to
  10 min, and back to every tick once it is alive.
- **No internet** (`--no-pkarr` / `POM_NO_PKARR=1`): no pkarr client is
  created - no publication, no key resolution and no cold dial by key;
  `/api/public/networks/locate` answers `unresolved`. Paths stay on UDP/mDNS on
  the LAN and on `bootstrap_nodes` (IP or internal DNS name) between segments;
  the endpoint reaches the peers the table learned, on their candidates.
  An internal relay with `--serve-relay` goes in by its URL in `relay_peers`
  (which marks it as dedicated, and so as one that carries no file transfer),
  with `--no-default-relays` so the node does not keep trying n0's. Without the flag, a failed publication waits twice as
  long after each consecutive failure, from 15 s up to 30 min
  (`gateway::PublishBackoff`), and only the 1st, 2nd, 4th, 8th... becomes a
  `warn`.
- The local outgoing IP (`addrs::outbound_local_ip`, a UDP socket "connected"
  to `8.8.8.8:80`) is only a last resort for the node to dial itself; it is
  never published.

The pkarr client's Mainline DHT is UDP IPv4 and runs in an actor on its own
thread. On Windows, when there is no answer, the 50 ms read timeout is reported
by the dependency as WSAETIMEDOUT (10060); that is idle polling, not a QUIC or
model load retry, and the actor does not share the UI loop. The binary filters
that expected warning, keeping real errors. The `/api/public/networks` endpoint
cannot enumerate arbitrary DHT records: it only resolves candidates already
learned through UDP/gossip discovery or the local cache. So a global IPv6
address announced without an external route is not enough for a new node to
discover the network.

### Federated directory of public networks

The global catalog, separate from the p2p transport, lives in `public_directory.rs`.
The principal of a public network signs a record with the trust document,
description, rules and channels in Markdown, location, known ingresses and
`relay_hints` (URLs of the dedicated relays whose host is a DNS name, at most
8; an IP there, bare or in a URL, is refused by `validate_network`). **The record carries no address**: the
network is found by the publisher's key (`publisher_pubkey`, the same as the
principal's and the pkarr record's). `PUBLIC_DIRECTORY_VERSION` is 3. The
record carries `version`, `sequence`, `issued_at`, `expires_at`
and a 16-bit proof of work nonce. The logical identity is
`(publisher_pubkey, sequence)`; the merge keeps the highest sequence and
resolves an identical conflict deterministically by the smallest hash.

Each node can configure up to 16 HTTP(S) ingresses in `POM_PUBLIC_DIRECTORY`
or in administration. The wizard accepts an empty list; the publisher sends its
record to every configured ingress, and a reader queries all of them in
parallel and writes the merge to `pom.db`. A replica is transport, not a root
of trust: the Ed25519 signature, chain of trust, cluster and fingerprint are
validated by the reader and by the ingress. Records expire in seven days and the
cache keeps at most 1024 items.

A private node can query the catalog over outgoing HTTP(S), without a public
IPv6, an inbound port or a relay. To keep isolation, it does not query UDP/
gossip/pkarr candidates of its own private network and publishes no public
record. The v1 runtime keeps a single P2P network per process: joining a public
one replaces the private network before restarting. So a public peer gets no
private bootstrap or addresses and there is no parallel public interface to
forward into the private network.

The Public networks screen shows reachability without showing an address:
`GET /api/public/networks/locate?key=pk:...` resolves the publisher's key
through pkarr **on the node** (the browser does not speak the p2p transport) and answers
only `{reachability: direct|relayed|unresolved, relays}`. The route is public,
so it only accepts keys this node already lists (publishers in the directory
cache, the local principal and live peers or public candidates) and caches
each answer for 30 s; without that limit it would become a DHT resolver for
anyone.

### NAT traversal: what works and what is still missing

Under libp2p, two nodes behind NAT or CGNAT long could not reach each other:
the announced `host:port` could not express a relay circuit, and without a
published circuit there was no relayed connection over which DCuTR could
coordinate the hole punch. Getting there took publishing real multiaddresses,
reserving circuits and confirming external addresses by hand.

With iroh, that path is the transport's own behavior. A node is reachable
through its home relay from the moment it connects to it, and publishes that
relay's URL in `endpoint_addrs` and in its pkarr record; a dialer connects
through the relay at once and iroh punches the NAT underneath the same
connection, moving it to a direct path when the punch closes. n0's public
relays give every node a home relay out of the box (`--no-default-relays`
turns them off); dedicated and member relays sit beside them.

The control plane goes through the same tunnel. `TunnelRequest` got a
`target`: `Inference` is still the default and the only one an older binary's
peer knows; `Gateway` is served by the target's gateway. With that, the
admission `/handshake` probe and the policy ratchets
(`/api/cluster/{tls,quic,logs,network-chat,network-settings}`) reach a peer no
direct address reaches - without it the peer did not enter `peers` even with a
perfect hole punch, and routing, UI and pipeline stayed blind to it. Every
ratchet tries the principal's addresses in the order of
`Network::order_gateway_bases` (the host that already answered first) before
the tunnel; the QUIC one used to try only the first announced address, without
the tunnel, and repeated "could not talk to the principal" every 5 s with a
principal behind an unreachable IPv6.

**A request that comes out of the tunnel is never local, and the gateway's
route list is closed - both are security invariants.** The forwarder
(`infer_tunnel::serve_loopback`) hands the request to the gateway through
`127.0.0.1`, and the gateway decides what is local by the socket address
(`serve::PeerAddr`): in localhost mode, `AdminAuth` opens administration to
whoever comes from loopback, and `install_trust_anchor` only looks at that.
With nothing else, any peer that reached a route through the tunnel would
be an administrator. That is why the forwarder marks every request with
`x-pom-tunnel-origin` and a random secret of this process (`tunnel_mark`),
overwriting whatever the peer sent under that name, and `serve::request_origin`
replaces the origin of a loopback request carrying that mark with a
non-loopback address (`TUNNELED_ORIGIN`); the header is stripped from every
request, marked or not. And `infer_tunnel::gateway_path_is_allowed` still
compares the exact path, with no prefix or wildcard, on the receiving side.
The one shape matched by segment is a plugin call between nodes
(`plugin_path_is_allowed`): `/api/internal/plugins/services`,
`/api/internal/plugins/<code>/peer/<op>` and
`/api/internal/plugins/<code>/engine/generate`, each segment a plain name. The
engine's session files and stage links stay off the tunnel. The
sender picks the `target` variant; the receiver resolves the port, with the two
ports it chose itself.

With the mark, the list also carries the **deployment and cluster read control
plane** (`/api/ui/{node,models,apply,reconfigure,unload,...}`, full list in the
code): `GatewayState::control_get`/`control_post_json` (over
`peer_client::send_to_gateway_over`) try each announced address and, last, the
tunnel, and return the answer as a regular `reqwest::Response`. On the other
side the request authenticates the calling node through the control signature
or the connection key, as in a direct call. That is what allows deploying to a
peer behind a symmetric NAT, which no address reaches and with which the hole
punch does not close. The tunnel itself imposes no deadline: each caller
imposes its own, and slicing (`/api/ui/slice-ranges`) waits up to an hour.

The **return** of the direct reply uses the same tunnel. The gateway sends in
`reply_to` the addresses through which it itself is reachable, and they come
from its local interfaces: with the gateway behind NAT, the tail on another
network dials `http://<lan-ip>:8080/v1/relay-result` and never arrives.
Since then `reply_to` also carries the gateway's `endpoint_id` and
`endpoint_addrs`, and `direct::deliver_via_tunnel` is the last path after every
URL failed. It was the hole that was left: the forward leg already crossed the
NAT and the return died silently until the frame deadline expired.

Administrative sync with the principal (`admin.rs`: reconciling the trust
chain, completing succession and infer admission) also tries the announced
addresses and falls back to the tunnel (`peer_client::send_to_gateway_over`);
the matching `/api/internal/{admin-trust,profile}/*` routes are on the closed
list, and all of them require `InternalNodeAuth`. API keys are not synced at
all: they never leave the node that minted them.

**Files and relays.** A file comes by direct HTTP when an address answers.
When none connects, the download falls back to the tunnel, in bounded pieces,
and then one rule holds: **a file never crosses a public relay**. A public
relay is one of n0's, one configured in `relay_peers` (which includes
`--relay-peers` and the trust document's hints), or any relay whose URL is not
the announced relay of an admitted member of this cluster. A piece travels
only when the connection's **selected path** is direct (dialed or hole
punched) or goes through a member relay whose announce allows transfers
(`Network::path_carries_transfers`, on `p2p::selected_path`). A relay only sees
an encrypted stream and cannot tell a file from an inference frame, so the rule
is held by the two ends: the serving node checks the path of the connection
every piece arrives on and refuses with 421, and the receiving node does not
start without a connection that may carry the file. A relayed connection may
turn direct when the punch closes, so a refused piece is worth retrying. See `file-transfer.md`, section 5.0.

**The tunnel frame is binary.** A request is
`[u32 length][JSON head][u32 length][raw body]` and a response
`[u16 status][u32 length][raw body]`. The body used to travel as a JSON array
of numbers, about 3.6 bytes on the wire per byte of payload, paid on every
frame that crossed a relay.

What stays outside the tunnel: the inference server's `/health` between peers.
It takes no part in admission, so a peer behind NAT joins the cluster and
routes without it.

### Per-edge route plan (`route_plan.rs`)

The transport ladder (QUIC, HTTP/HTTPS, tunnel) used to be walked on every
frame. With a dead announced address - an IPv6 a container without IPv6 cannot
reach, the public IPv4 of a router without a forwarded port - each frame paid
for the failing setup before reaching the path that works, and that ate the
frame's deadline. Now the route is **measured once and pinned**.

- **A directed edge, measured by the sender.** Going from A to B says nothing
  about going from B to A: a node behind NAT goes out through the tailnet and is
  only reached through the tunnel. Each process measures its own outgoing
  edges, and none of it goes into gossip. There are two plans per peer, because
  they are different servers: `Inference` (prefill and decode) and `Gateway`
  (the return of the direct reply and of remote-stream).
- **When it measures.** The planner (`spawn_planner`, started by
  `Network::start`) measures every edge with no analysis, with an analysis
  older than 3 min or whose peer changed addresses, ports, `deploy_id` or layer
  range. A deployment that becomes `serving` changes the announced range, so
  the analysis runs before the first request. An analysis that found no route
  repeats every 5 s in the background. With a route already there, an empty
  analysis **keeps** it - the probe has a short cap and a busy peer did not
  answer in time - and renews its deadline so frames keep using it, but the edge
  stays due and is measured again 5 s later instead of a full 3 min later. The route only goes away when a real
  frame fails on it (`record_failure`).
- **How it measures.** With the real transport, in parallel, two samples per
  route (the smaller wins, because the first pays the handshake): `GET /health`
  over HTTP with the pinned client, an HTTP/3 POST per QUIC endpoint (any status
  proves the path) and `/health` through the tunnel. The tunnel's own 502 for a
  server that is not listening does not count: the direct probes fail against
  that same closed port, and counting it left the tunnel as the only route of a
  peer measured while its deployment was still loading. A 500 ms ceiling per direct
  probe and 2 s through the tunnel. The list comes out sorted by RTT; within
  2 ms QUIC wins, then HTTP, then tunnel. QUIC only enters with the cluster's
  QUIC policy on.
- **How it uses it.** The frame only goes through the pinned transport
  (`relay::HopTransport`): falling to another within the same call would hide
  from the plan that the route failed. A failure before delivery retries the
  same route once; the second failure moves to the next measured route, in the
  same frame; an exhausted list erases the edge, wakes the planner and the frame
  still goes down the full ladder. A failure after delivery never switches
  routes, because the peer may have executed the frame.
- **Route pinned during inference.** Every frame
  (`relay::send_frame_or_tunnel`) and every return delivery (`direct`,
  `Gateway` plane) holds a `RouteBook::begin_use` guard on the edge. With a
  frame in flight, or until `PIN_GRACE` (60 s) after the last one, the planner
  does not probe the edge (`claim_measurement`) and drops an analysis that
  finishes in between (`install`), including an empty one; and `active` keeps
  returning the route even with the analysis expired (`PLAN_TTL`), so a long
  generation does not drop to the full ladder halfway. The frame's own failure
  still switches routes (`record_failure`). Once the edge is idle, the stale
  fingerprint or deadline leads to a new analysis on the next round. What the
  pin does not hold: a switch underneath the tunnel (relay to direct through a
  hole punch) and the `prefer_tunnel` mark of `PeerClients`, which only applies
  to an edge without an analysis.
- **Connection wait.** QUIC's setup budget is 3x the peer's measured RTT,
  between 100 ms and 300 ms; without a measurement, 150 ms. A target with no
  local route (missing address family) fails at once through
  `quic::ensure_local_route`. The frame deadline is still proportional to the
  work: it is what protects a long prefill, and with the route pinned it is no
  longer eaten by connection attempts. TCP's `CONNECT_TIMEOUT` (400 ms) stays as
  an absolute ceiling, because it also applies to the pinned route and must fit
  a high-RTT link.
- **Diagnosis.** `/api/ui/cluster` carries `route_plan`: per edge, the active
  route, the alternatives and the analysis age. The logs state the chosen route
  (`rota inference para <peer>: http ... (26.0 ms)` - "inference route to"), the
  switch (`falhou duas vezes; passando para ...` - "failed twice; moving to")
  and the reanalysis.
- **Session lifecycle.** `rewind`, `release` and `rollback`
  (`gateway/generation.rs::post_session_lifecycle`) follow the pinned route:
  with the inference plan on the tunnel, they go straight through it
  (`relay::post_json_via_tunnel`, JSON with `Content-Type` preserved); without a
  plan, they try QUIC and HTTP on the announced addresses and fall to the tunnel
  if the transport fails. Before, they ignored the plan and only tried the
  announced addresses: on a peer behind NAT the rewind failed (lost prefix) and
  so did the release, leaving the session stuck in the KV and the peer with a
  queue for the planner. KV snapshots (`POST /v1/session/slot`,
  `slot_call_step`: save, restore and erase, including the automatic ones of
  the persistent prefix cache) follow the same rule; an HTTP refusal from the
  peer is still final, and `post_json_via_tunnel` returns the refusal reason
  together with the status. The lifecycle deadline is 60 s
  (`SESSION_LIFECYCLE_TIMEOUT`): the peer's handler needs the engine lock, and
  on a peer busy with another session's prefill chunk the previous 3 s expired
  (lost prefix, session stuck in the KV).
- **Relay.** On every round (2 s) the planner records in
  `RouteBook::relayed_nodes` the peers with which every live endpoint
  connection has its selected path on a relay - the hole punch did not close. A `tunnel` route to one of
  them crosses the relay on every frame. `/health` publishes the list in
  `relayed_peers`, and the Dashboard paints the edge and the route orange
  (`relayed` in `/api/ui/chat/telemetry`) with the explanation of why it is
  slow. The list is rebuilt every round, so a hole punch that closes after the
  measurement removes the mark.

The preference learned through the tunnel (below) still applies to edges with
no analysis yet.

### Preference learned through the tunnel

Punching through the NAT is not enough if every frame keeps paying the whole
direct ladder. `PeerClients::prefer_tunnel` is a **per-peer**, learned mark: it
turns on when the tunnel delivers after the whole direct ladder failed, and
turns off on the first direct success. Since, while it is on, frames no longer
try the direct path, a peer with that mark gets its direct path probed in the
background (`/health` through the pinned client) at most every
`DIRECT_REPROBE_INTERVAL` (5 s); success clears the mark and the current frame
never waits for the probe. Without it, a single stumble left the peer on the
tunnel until the process restarted. While on, the first attempt of each frame
skips QUIC and HTTP and goes straight to the tunnel; a tunnel failure brings
back the full ladder on the next attempt, because the direct route may come
back (a new hole punch, a peer that changed networks). Without it, behind NAT
every prefill and every decode burned a QUIC timeout plus an HTTP timeout
before the only transport that delivers. The mark is not configuration and does
not spread between peers: a peer behind NAT does not push its reachable
neighbor onto the tunnel.

### Diagnosis

`Network::traversal_status` (`TraversalStatus`) keeps the NAT report state
(`nat`), the published endpoint addresses (direct ones and relay URLs, shown
apart on the screen), the relays the endpoint holds a live connection to
(`reservations`, by URL) and the count of connections that moved from a relay
to a direct path (`hole_punch_ok`; iroh reports no failed punch, so
`hole_punch_failed` stays at zero). It goes out in `/api/ui/cluster` in the
`traversal` field and on the Network screen, panel "NAT traversal". Two nodes
behind NAT that do not see each other fail at one of two steps - no relay is
connected, or the punch never closed and the traffic stays relayed. The field
enters the cluster cache fingerprint; without it the snapshot would freeze,
because neither the NAT report nor a change of relay touches `peers`.

A failing dial is logged with the reason (`p2p: dial to <peer> failed (N in a
row): ...`), only on the 1st, 2nd, 4th, 8th... consecutive failure.

**Wait between redials (`DialBackoff`, `p2p.rs`).** After a failing dial, the
peer goes into a wait that doubles from 250 ms up to 10 s; an established
connection resets it. While waiting, a batch of cluster logs does not dial (it
is dropped and counted as lost), a tunnel call fails at once (the route plan
moves on to the next path) and gossip warm-up does not dial. Without it, with
the tunnel down, every logs batch - every 100 ms - became a dial, and the log
of each failure went into the next batch: a self-sustaining loop.

**The route in use with each peer** is another question, and comes out in
`GET /admin/nodes` in the `route` field (`Network::peer_route`), shown on the
participants screen in place of the announced address - which stays in the
row's `title`, for comparison. It is observed, in the order the control plane
tries the paths: `http` (a direct address answered less than 5 min ago and has
not failed since - `RouteTable::last_success`, fed by `mark_success`),
`p2p_direct` (an endpoint connection whose selected path is direct, dialed or
born from a hole punch), `relayed` (only a path through a relay: the hole punch
did not close) and `none`. In `relayed`, `relay_kind` says whether the relay
is `dedicated` (it is in `relay_peers`), `public` (one of n0's) or `member`
(another node acting as a relay), and the screen shows "member relay" for the
last one. The links come from `discovery::link_tracker`, which follows every
connection's path list until it closes (`Network::note_link` /
`drop_link`): the selected path's `ip:port (UDP)`, or the relay's host and URL.

## 4. Identity

`Identity` (`identity.rs`) is a long-lived Ed25519 pair, living in
`<project_root>/.pom-node.key` (or `$POM_NODE_KEY`). The `node_id` is the hex of
`SHA256(pubkey)` (`NodeId::from_pubkey`), printed with its first 12
characters. The key must survive restarts: regenerating on every boot
duplicated the peer in the list until the timeout and voided `blocked_nodes`,
which is keyed by node_id. A corrupt file generates a new identity instead of
stopping.

It is **the node's only key**: `Identity::secret_key` is the iroh endpoint's
secret key, so the endpoint id is the public key itself (`Identity::peer_id`
returns its hex, the same string as `public_key_hex`);
`PkarrNode::from_signing_key` uses the same seed for the `pk:` record, and
`admin_trust::local_authority_identity` uses it as the authority key. That is
why `Announce` and `PeerInfo` have a single `pubkey` field, and public
admission checks `node_id == SHA256(pubkey)` and `endpoint_id == pubkey`. The
trust chain's `AuthorityPrincipal::peer_id` keeps the libp2p `PeerId` text form
(`12D3KooW...`), derived from the key alone by `identity::trust_peer_id`: every
chain signed before the move to iroh records it that way and authority is
matched on the whole `{node_id, peer_id, pubkey}` tuple. Taking the endpoint
id there instead stripped a principal of authority over its own network
(every principal-only route answered 403). There is no
`.pom.key` or `$POM_PKARR_KEY` any more; a `.pom.key` left over from an
earlier version is ignored. Changing the key changes `node_id`, the endpoint
id, `pk:` and the trust chain identity at once: a principal that changes its
key needs a new chain. The lobby client is the one exception: it runs its own
endpoint under a key derived from the node key (`network_lobby::transport_key`),
because two endpoints under one key would knock each other off the relays.

## Known limitations for the internet

Current code behavior:

1. **The gossip address can be `0.0.0.0:<real port>`.** The gossip
   `Announce` carries `addr = announce_addr()` (literal `0.0.0.0` without
   `--advertise-addr`). Today it also carries `addr_candidates`, and
   `handle_announce` uses the first candidate, so `0.0.0.0` only survives when the
   peer announced no candidate at all (e.g. Windows, where the enumeration is
   empty). The UDP path rewrites `addr` with the source IP. The gossip path
   does not rewrite it, but the target guard keeps that sentinel from
   being dialed or turned into a URL or endpoint address. A valid observed or announced
   candidate is still used.
2. **Inference traffic picks a transport by preference.** When QUIC is on, the
   gateway and the relays try HTTP/3 through `relay::send_frame_or_tunnel` and,
   if it fails, fall to the pinned TCP client: HTTP when TLS is off and HTTPS
   when it is on. Only after every direct address failed can the
   `network/infer_tunnel.rs` tunnel be used, when the hop announces
   `transport/iroh-tunnel-v1` and a valid endpoint id; it is protected by QUIC
   under the target's key and carries the inference credential to the target's
   loopback. When QUIC is off, the same direct-before-tunnel order keeps
   HTTP/HTTPS as the preferred effective transport. Each call carries the
   endpoint addresses from the plan (`endpoint_addr_candidates`) or the live
   announcement as dial hints, so a gateway that knows the endpoint id before
   the gossip announce arrived still has somewhere to dial. An explicit HTTP answer from the peer is not retried
   on another transport. In TP, `broadcast_to` and `POST /v1/tp/reduce` follow
   the same rule and the `TunnelRequest` keeps the decode, barrier, rank and
   authorization headers. The loopback only injects `application/octet-stream`
   when the call did not state another type, keeping `application/json` on the
   session lifecycle routes.
3. **The inference port requires the internal credential.** `/v1/prefill`,
   `/v1/step`, `/v1/step-stream`, `/v1/session/release`,
   `/v1/session/rollback` and `/v1/tp/reduce` only accept the shared
   `connection_key` with the internal marker (or the secure transport plus the
   marker on public networks); client API keys are refused
   (`inference_listener_accepts_only_the_internal_connection_key`). `/health`
   stays public for readiness.

## 5. TLS between peers

Always on between nodes. There is no switch: `main.rs` sets the URL scheme
to `https`, the announce's `tls` to true and the server to strict at startup.
A `pom.json` that still carries the old `tls_required` key parses, and the
key is dropped (`Config::retired_tls_required`).

**Every peer connection is mutual TLS.** The node presents the same
certificate as its client certificate (`tls::set_client_identity`,
`tls::client_config`), so the server learns which node is calling
(`serve::TransportPeer`, from the certificate's key). A call authorized only
by the connection key also needs that identity to be a member today
(`auth::caller_is_member`, `Network::is_member_key`): an admin of the trust
chain, or a peer whose announced membership token holds and is not revoked.
Every member holds the connection key and a banned one keeps it, so the key
alone, from a network address, opens nothing. The libp2p tunnel carries the
same identity: the forwarder names the PeerId Noise authenticated
(`infer_tunnel::TUNNEL_PEER_HEADER`, believed only beside the process mark),
and the PeerId embeds the node key. Plain HTTP from loopback is the one case
without an identity, and is accepted: it is this machine.

**The certificate is derived from the identity, not issued by anyone**
(`network/tls.rs`). `node_id` is already `hex(SHA256(pubkey))` of the
long-lived Ed25519 key, and the cluster already spreads that hash through
gossip - so the certificate only needs to **prove possession of the key that
yields that node_id**, and that is what the TLS handshake does. `rcgen` builds a
self-signed one with the `.pom-node.key` itself, in memory; nothing goes to
disk, there is no CA, nothing to renew, revoke or distribute.

**Verification is pinning** (`PinnedNodeId`): the client extracts the pubkey
from the presented certificate's SPKI, computes SHA256 and compares it with the
node_id it already learned for that peer. Validity, chain and name are
**ignored on purpose** - trust does not come from them. Checking validity would
only produce failures from a skewed clock, and checking the name would only
produce failures from dialing an IP instead of the CN; neither would say
anything about whom we are talking to. Without pinning the self-signed
certificate would encrypt without authenticating, and a man in the middle would
present their own self-signed certificate and be accepted.

The test `the_certificate_fingerprint_is_the_same_as_from_pubkey` ties both
computations together: if they ever diverge, pinning refuses everyone, and the
test is what warns.

**One port, two languages** (`backend/serve.rs`). The server peeks at the first
byte without consuming it (`TcpStream::peek`): `0x16` is a TLS handshake,
anything else is clear-text HTTP; both land on the same router. That is what
keeps `http://localhost` working for the operator's browser, which would not
accept the self-signed certificate. Clear text from outside loopback is
refused right after the peek (or redirected, below).

**Loopback always speaks clear text**, and that is a requirement:
`infer_tunnel::serve_loopback`, the `runtime.rs` probe, `gateway::testkit` and
the operator's browser hit `127.0.0.1`. Traffic that does not leave the
machine has nothing to protect.

**The client becomes per target** (`network/peer_client.rs`), because the
verifier carries which node_id to expect and is fixed at construction. Without
TLS (only on test benches) it goes back to a single one. The URL
scheme (`http`/`https`) comes from a global process state,
`peer_client::tls_enabled`, because it is a property of the node and not of the
connection.

The peer HTTP clients enable `TCP_NODELAY` on every TCP path, clear, TLS and
probes. That keeps small inference requests from paying the combination of
Nagle with the OS's delayed ACK. The clear `PeerClients` is shared and the TLS
client is cached per `node_id`; `for_peer` calls reuse reqwest's internal pool
instead of creating a client per frame.

**This applies to the control plane too, not only to inference.** The first
version converted the inference and transfer paths and left out
`/api/ui/cluster/models`, the `deploy::survey`, the forwarding of
`plan`/`leave`/`apply`/`unload`/`stop-inference` and job polling. With the
policy on, **every** query to a peer failed silently: the deploy screen said
"no machine in the cluster has slices of <model>" and there was no way to
deploy anything. So the defect does not come back, `GatewayState` **no longer
has** a shared client without pinning - there is only `peer_http`, and whoever
talks to a target that is not a peer (loopback, Ollama) asks for
`peer_http.plain()`, which says what it is doing. `webui::catalog_under_tls_tests`
pins the two routes the operator uses to deploy.

**What is left out, and why:**

- **UDP gossip.** TLS does not apply to datagrams, and the content is address,
  layer range and capabilities - no user data, no weights, no credentials.
  Each announce is signed by the node key instead (section 2, "Signed
  announces").
- **The p2p endpoint.** It is already encrypted end to end by QUIC (TLS 1.3 under the node key); layering TLS on top
  would add no security. A hop through the tunnel reports `tls: false` in
  telemetry on purpose: it is encrypted, but not by TLS, and saying `true` there
  would claim a guarantee that hop did not give.
- **Nothing, on the browser side** - see "Two certificates on the same port"
  below. The first version left the UI out, and the result was that turning TLS
  on locked the operator out of the panel.

### Two certificates on the same port

The pinning certificate is **Ed25519**, because its key is the node's identity
- and that is exactly what a browser cannot verify. Measured with macOS's
curl/LibreSSL: `sslv3 alert handshake failure`. Together with strict mode,
turning TLS on left the UI unreachable from any other machine: in clear text
the connection was dropped, in TLS the handshake did not close.

So the node has **two** server configurations on the same port, chosen per
connection from the ClientHello (`tls::server_configs`, picked by
`tls::wants_peer` through rustls' `LazyConfigAcceptor` in `serve.rs`):

- **Peer** - the Ed25519 derived from `.pom-node.key`, which holds up
  pinning, and a request for the peer's own certificate (optional, so a client
  without one connects and carries no identity).
- **Browser** - an ECDSA P-256, which it can verify, and no certificate
  request, since asking would make the browser offer the operator's own
  certificates. The node's addresses go
  in the SAN (`serve::browser_sans`: local ones, the operator's pinned one, the
  hostname and loopback). The SANs exist so the warning is "unknown issuer" and
  not "wrong host" - the second makes the operator distrust the machine instead
  of the certificate.

**The discriminator is ALPN**, and the choice is not cosmetic: SNI does not
work (a peer dials an IP, and an IP carries no SNI) and the client's algorithm
list varies between browsers. ALPN is under our control - the peer client
offers `pom/1` (`tls::PEER_ALPN`), no browser does. If that choice ever goes
wrong for a peer, it gets the ECDSA one and the verifier, which only accepts
Ed25519, refuses: fail closed.

Being self-signed, the browser warns on the first visit to each machine. That
is inherent to having no CA; distributing an own CA and installing it on each
machine is another project.

### Clear text: redirect, except where it must not

With the policy on, the gateway port answers **307 to `https://` on the same
port** (not 301, which a browser caches for good) to whoever arrives in clear text, instead of closing the connection.
Path and query are kept, and the host comes from the `Host` header - rebuilding
it from the socket would give the IP and swap the name the operator typed for
an address that may not even be in the certificate.

Two exceptions, both for a concrete reason:

- **Peer-only routes** (`/api/transfer/{manifest,pom,download,offer,offers/answer}`, `/v1/relay-result`,
  `/v1/remote-stream-result`, `/v1/prefill`, `/v1/step`, `/v1/step-stream`)
  answer **403**, not 307. `reqwest` follows redirects by default: a 307 here
  would make an old peer try TLS without knowing how to verify anything and
  fail with an error that does not say why.
- **The inference port (`serve::PlainPolicy::Refuse`)** closes the connection,
  as before. There is no browser there to accommodate.

For development, `POM_INSECURE_ADMIN_UI=1` turns the 307 off: the gateway port
serves the admin UI in clear text to a remote browser as well, so a machine
reached by IP works without trusting its self signed certificate. The two
exceptions still hold (peer-only routes answer 403, the inference port closes),
and a clear-text node-to-node call carries no `TransportPeer`, so the key alone
still opens nothing. Loopback is clear text with or without it.

### 5.1 Optional QUIC for inference

`Config.quic_enabled` is a network-wide preference, on by default (a new
network starts with it, and a config without the field reads as on) and only
editable by the principal at `POST /api/ui/network/quic`. The announcement
carries `quic` only as a trigger: the peer confirms the value at
`GET /api/cluster/quic`, authenticated by the `connection_key`. The trigger is only accepted when the announced
`pubkey` matches the locally anchored principal; a secondary admin
can neither force the query nor revert the preference. A failed query keeps
the local value. The switch is atomic and only applies to new calls;
connections and inferences in progress are not interrupted.

When on, the inference server and the gateway listen for HTTP/3 on dedicated UDP
endpoints, derived from the node's **own** `p2p_port`:
`quic::inference_quic_port` = `p2p_port + 1000` and `quic::gateway_quic_port` =
`p2p_port + 2000`. The derivation never starts from
`inference_port`/`gateway_port`: in a co-located cluster (host networking)
those numbers are consecutive between nodes, so `port + 1` would fall on the
neighbor node's gateway port and break discovery. `p2p_port` is already unique
per co-located node; a `p2p_port` of 0 (ephemeral bind) or an offset that
overflows `u16` turns QUIC off and keeps TCP. The ports are published in the
`Announce` as `inference_quic_port` and `gateway_quic_port`. The client builds
`host:port` from the peer's announced public address, including the bracketed
IPv6 form; it never extracts a UDP target from `inference_url` nor tries the
TCP port.

The server binds `[::]` with `IPV6_V6ONLY=0` explicitly set through `socket2`
before handing it to Quinn. That keeps IPv4 and IPv6 on the same endpoint on
Windows, Linux and macOS; there is no alternative IPv4-only bind. Quinn's client
already sets the same option on the wildcard IPv6 socket.

The QUIC endpoint is created from the node's identity. Quinn negotiates its
own TLS 1.3, the client pins the `node_id`'s Ed25519 certificate and presents
its own, and the server passes the caller's identity to the request
(`serve::TransportPeer`) as the TCP listener does.
If the peer has no QUIC endpoint, the port is not listening, the route fails or
answers 404/405, the relay falls to HTTP/HTTPS and marks
`transport_fallback`; discovery shares a budget (3x the RTT measured by the
route plan, between 100 and 300 ms; 150 ms without a measurement) across the
announced QUIC addresses, instead of serializing one timeout per address, and
quarantines a failed endpoint for 5 s. A healthy session is reused for the next
frames. If a call fails on a cached session, the client closes and removes that
session and retries the request once with a new handshake within the existing
budget. So the `reset by peer` expected when the remote process restarts does
not turn an old connection into a permanent state. The quarantine still applies
to the endpoint's final failure and lasts 5 s, avoiding repeated handshakes per
token. The tunnel is only the last reserve after a transport failure on the TCP
path too. Each complete transport attempt, including handshake, send, headers
and body, has a single deadline: the frame's, (1 s + 50 ms per token + 2 s per
image) x remaining hops (`relay::frame_deadline`), the same on QUIC,
HTTP/HTTPS, the persistent channel and the tunnel. Decoding one token over 2
hops = 2.1 s. A prefill chunk does not use that formula: it has a liveness
deadline of 10 min per hop (`relay::prefill_frame_deadline`), because it waits
in the peer's queue behind another session. The deadline is born
in `send_frame_or_tunnel` and never exceeds its caller's. A peer that went down
is caught earlier by the connection's lifetime: `QUIC_MAX_IDLE` (1.2 s) and TCP
keep-alive (~4 s). Per-hop telemetry publishes the effective transport as
`http`, `https` or `quic`. For QUIC, `transport_rtt_us` is the RTT measured by
the UDP connection and the network quantiles stay separate from remote
processing time. `quic_request_body_send_us` sums the wall time awaited by
`send_data` and `finish` before reading the response headers. The summary
counts the measured physical operations, so a shared batch upload is recorded
once. That field measures backpressure and local acceptance of the body, not
confirmation of remote delivery.

## Cluster process logs

The process log uses its own endpoint request/response protocol,
`/pom/cluster-logs/1.0.0`, separate from `infer_tunnel.rs`. Each node sends a
copy of the batch to every live peer announced as admin **that the sender's
trust chain also proves an admin** (`Network::admin_peers`): an announce alone,
which any member can forge, would hand it the network's logs and prompts. The
network Chat's `principal_peer` likewise takes only the chain's principal.
Each effective admin
persists the entry in its own replica's arrival order, without sorting by the
remote clock, and keeps `origin_at` only as informative data. The receiver
checks its own effective authority in `AdminTrust`; an `admin` profile without
a grant does not become a read target nor accepts the copy. It also binds what
arrives to the swarm connection: the libp2p `PeerId` must be a live peer
(`Network::live_node_of`), a log batch must name that node as its source and
every line as its own, and a request-audit begin must name it as the entry
node, with only the node that began a request allowed to complete it
(`ClusterLogs::try_ingest_remote`, `AuditLog::order_remote_events`).

Request audit follows the same topology on `/pom/request-audit/1.0.0`: POM's
transport replicates `Begin` and `Complete` to every known admin, even when the
local event was accepted by the `pom-plugin-logs` plugin. That plugin still
owns storage and queries in `enterprise.db`; POM handles admin discovery,
fanout and transport. On the receiver, a bounded queue keeps the order and
delivers the events to the active plugin host. If the host does not accept, POM
uses the legacy adapter in `pom.db`. Received copies are never retransmitted.
So `/api/ui/cluster-logs`, `/api/ui/requests` and their statistics do not
depend on the principal being reachable once the local replica was received. A
completion that arrives before the start is held until the start event.

`cluster_logs_enabled` in the `Announce` is only a trigger. When it diverges
from the local policy, the node does an authenticated fetch of
`GET /api/cluster/logs` on the principal with the `connection_key` and
`x-pom-internal-node: 1`, and only writes the returned value; a failed fetch
keeps the local value. The administrative switch applies the change without a
restart.

The local backpressure proof runs with
`cargo test -p node --bin pom cluster_logs --no-default-features -- --nocapture`.
It fills a queue of capacity 1, requires the second emission to return
immediately as a drop and checks the counter. The microbenchmark included in
the same suite, on an Apple M4, did 50,000 emissions and 782 batches. Over five
runs, the medians were `305 ns/line` for `try_push` and `373 ns/line` to
receive and form batches of up to 64 entries. The measurement is of the capture
and batching path, not of inference; the SQLite writer and the transport stay
off the emitting thread.

`tracing` goes into its own queue bounded at 1024 entries. Capture uses a
lock-free snapshot of the context and `try_send`, so it never waits on an
identity update, inference or the network; a full queue drops the entry and
accumulates the count. A task off the hot path sends up to 64 entries every
100 ms, best-effort, without aggressive retries. Each admin writes the lines to
its own `pom.db`, tied to the `deploy_id`, and also persists the drop counter.
That choice replicates the current full retention: with `N` admins there are up
to `N` writes and storage copies per entry. There is no different secondary
retention policy to hide that cost; a future reduction must be an explicit
product decision, because it would change the audit semantics.

## 6. Network Chat

The network Chat is a surface separate from the AI Chat and uses the same
p2p endpoint, with no XMPP or external server. Its own request/response
protocol is `/pom/network-chat/1.0.0` (one ALPN), with QUIC, relays and hole
punching from iroh, and addressing
exclusively by `node_id`. Text, files and the typing indicator share the
`network` room; files are capped at 8 MiB and text at 8 KiB. The text history
and file metadata live in `network_chat_messages` in the same `pom.db` as the
AI history. The UI snapshot does not repeat file bytes on every poll; the
authenticated download uses `GET /api/ui/network-chat/file/:message_id`.

The design is a star. A participant sends each action with POM's active API key
in the original frame. The principal node checks the key in the database, binds
the key to `sender_node_id`, refuses `blocked_nodes`, applies a rate limit and
only then relays the already validated frame to the participants. The relay
strips the secret and carries `relayed: true`; a node that is not the principal
does not accept that frame as an origin. A file stays within the protocol's
frame limit and is not stored outside each node's local database.

`network_chat_enabled` is on by default, like `quic_enabled`: a new network
starts with its Chat, and the principal can turn it off. In the `Announce` it
is a trigger, never a value. The
`POST /api/ui/network/chat` switch writes the authoritative configuration on the
real principal, whose `pubkey` comes from `AdminTrust`, and updates
the announced atomic copy. When the UI is on a secondary admin, the same route
forwards the mutation to the principal with the connection key. A node that
observes a divergence queries `GET /api/cluster/network-chat` with the
connection key and `x-pom-internal-node: 1`; the route is common/internal and
does not require a human token. It only adopts an authenticated answer; a
failure keeps the local value. The announced profile does not replace the
principal's identity. When off, the channel neither accepts, persists nor
relays actions.

Communication is deliberately low priority: the protocol is separate from the
inference tunnel (its own ALPN and connection), every call runs on its own
task, the queues use `try_send`, and persistence runs on a dedicated writer
with a bounded queue. Under pressure, the Chat drops and increments `dropped`;
it never waits on SQLite or holds a `/v1/tp/reduce`, prefill or decode call.
The backpressure test and the latency measurement live in
`network_chat_tests.rs` and in `discovery.rs`.

**Plugin notices.** The `plugin_notice` action (`ChatAction::PluginNotice`)
carries the notice a plugin sends to every node: `plugin_code`, `title` (up to
200 bytes), `body` (up to 2 KiB) and `level` (`info`, `success`, `warning` or
`error`). It travels the same authenticated star and the same rate limit as
messages, but does not enter `network_chat_messages`: each node keeps the last
50 notices for 24 h in memory, without repeating the same `message_id`, and
publishes them in the `notices` field of `GET /api/ui/network-chat`. The UI
shows them as toasts and delivers them to plugins as the
`notification.received` event (see `plugins-ui.md`, "Events and
notifications"). With the network Chat off, the notice is refused like any other
action. An older peer does not know the variant and refuses the frame (first
version policy).

### 6.1 Lobby: talking to a network before joining it

A network answers machines that are **not** members in its lobby, which is
where they ask to be admitted and where an admin can talk to a candidate
first. The code is `network/network_lobby.rs`. There is no switch: the lobby
is the only way into a network, so it exists whenever discovery is on
(`public` or `private`, never `local`; `NetworkChat::lobby_enabled`). It does
not depend on the network Chat. With the Chat off the lobby still answers and
takes admission requests; only the conversation is closed: a post is refused
with `lobby_messages_closed`, no presence is relayed to members, and the
answers to `Post`, `Fetch` and `Admission` carry `messages_closed: true`, on
which the candidate's window shows no conversation at all.

Who decides a request is `network.access_policy`. Under `authorized` an admin
approves it from the bell. Under `auto_approve` the principal approves it on
arrival (`NetworkChat::auto_approves`, applied in the lobby handler of
`discovery.rs`): the role still has to be one of `accepted_enrollments`, a
blocked machine is still refused, and a machine an admin rejected still waits
out its retry time. The candidate then collects the grant like any approved
one. A
`private` network is listed by no relay: a candidate reaches its lobby by the
principal's key, taken from the trust document its admin exported and the
candidate imported on the Networks screen (`webui/networks.rs`).

A candidate has no API key, no announce and no admission, so the star's
credential does not apply. Its own request/response protocol is
`/pom/network-lobby/1.0.0`, and it is **pull only**:

- `Post { card, message_id, text }` says something; `Fetch { card }` reads.
  Both are answered with the last 50 text messages of the lobby room, each
  flagged `member` or not. The principal never opens a stream to a candidate
  and keeps no session, only a presence entry that expires 90 s after the last
  request - the candidate's poll is what keeps it listed.
- The lobby connection runs under a **transport key derived from the node
  key** (`transport_keypair`), never the node key itself, and `Post`/`Fetch`
  carry `LobbyIdentity`: the node's public key and its signature over the
  transport endpoint id. The principal names the candidate by the key that signed
  for the transport it actually sees (`LobbyIdentity::verified_node_id`), so a
  proof replayed from another connection is worthless; without a valid one the
  request answers `identity_invalid`. The public reads (`Info`, `Describe`,
  `Directory`) carry no identity and are rate limited by the transport peer.
  The separate key is not cosmetic: the node's own endpoint already runs under
  the node key, and two endpoints under one key confuse the relays, which hold
  one connection per key - the second would knock the first off its home
  relay.
- A candidate's text becomes an ordinary `ChatAction::Text` in room `lobby`,
  stamped with the principal's clock and with a `message_id` namespaced by the
  sender, persisted and relayed to the members by the existing star. Members
  answer through the normal authenticated path with `room_id: "lobby"`.
- `card` is the candidate's self-declared hardware (hostname, OS, CPU count,
  RAM, GPUs with VRAM), clamped on both ends. The principal relays it to the
  members as `ChatAction::LobbyPresence` when the candidate appears, when the
  card changes and every 45 s while present; it is kept in memory only, and a
  member's API key can never originate one (`lobby_presence_not_allowed`).

Because the frame is accepted before any credential, the bounds are tight and
separate from the Chat's: 64 KiB per frame, 2 KiB of text, text only, 4
requests per second and 6 texts per 30 s per candidate, and at most 64
candidates tracked. A blocked `node_id` and a current member are refused
(`node_blocked`, `already_member`); with the lobby off every request answers
`lobby_disabled`. What a candidate reads is the lobby room only - the
`network` room, files and direct messages never leave the members.

**Admission.** A machine joins a network by asking, and an admin decides
later. `Admission { identity, card, role }` on the lobby protocol records the
request on the principal (`NetworkChat::request_admission`, persisted in
`local_settings` as `lobby_admissions`, at most 128); `role` must be one the
network currently accepts. Every answer to that machine's `Post`, `Fetch` and
`Admission` then carries `admission: { status, role, grant }`. The grant - the
trust document, the **connection key**, the relay keys and the network's
current settings - is present only once an admin approved, and it leaves the
principal only there: in an answer to the node whose signature over the
transport was just verified, on that Noise connection. Admission, the member list, key claims, departures and the
published stats belong to the **principal** alone: a secondary admin has the
`admin` profile but refuses them (`Network::is_principal` compares this
node's key with the principal of the local trust chain, and the network chat
relay uses the same test). A rejected machine may
ask again after an hour, and is told how long is left: a rejected `admission`
carries `retry_after_secs` (`admission_retry_after`, counted from the
decision on the principal's clock). The candidate turns it into a moment on
its own clock (`retry_at`), so the screen can count down without the two
clocks having to agree. An approval is never undone by asking again.

The principal fills two things an admin weighs: `card.relayed`, from the
connection it actually sees (`Network::p2p_links`), overwriting whatever
the candidate sent, and it keeps `card.rtt_ms`, the round trip the candidate
measured on its previous exchange (self-declared, like the hardware). The
admin lists and decides in `GET|POST /api/ui/network/admissions`; approving
sends nothing, the candidate collects the grant on its next poll.

On the candidate, `webui::spawn_admission_poller` fetches every pending
request each 30 s so an approval that comes hours later is noticed with no
window open. The grant stays in memory there, never reaches the browser and
is not written to the database. The node also records whether it **asked**
for the admission it sees. An approval it did not ask for is a machine that
was admitted before, left, and finds the network still holding its approval:
that is reported as `standing`, raises no notification, and the lobby offers
to switch in one click instead of announcing an answer. Joining is the operator's explicit act
(`POST /api/ui/network/join`): the node checks the granted trust document
belongs to the key it asked, replaces its trust root, rewrites its config as
a member with the granted key and settings, and restarts. It leaves the
network it was in - usually its own local one.

**A node's own network is never dropped.** The network a node created at
first launch is inherent to it. `join_network` first writes it to a standby
file (`webui/standby.rs`: the trust root plus the network block of the
config - profile, connection key, settings, relays, blocked nodes and the
cluster policies - in `.<slug>-primary-network.json`, owner-only because it
holds a key). Only the network the node is the principal of is saved, and
only when nothing is in standby, so moving between two foreign networks
keeps the original. `leave_network_and_reset` restores it exactly and removes
the file once the config is saved; it refuses (`409 own_network`) on the
node's own network, which cannot be left. A node that never had one - set up
as a member by the wizard - still falls back to a fresh local network.

**Leaving costs the key.** Before going silent the leaving node tells the
principal: `POST /api/cluster/member-left`, signed with its own key and sent
over HTTP or the p2p tunnel (`notify_principal_of_departure`, 5 s, best
effort). The principal acts only when the signer is the node named in the
body and a peer it currently admits. It forgets that node's admission record
and, unless the network is in the open mode, generates a new connection key
and pushes it in the admin-signed settings envelope to every admitted peer
except the one that left (`rotate_connection_key`). The departed node drops
the key it held along with the rest of that network's config, and coming
back takes a new admission request. When the control channel is not there -
the node does not see the principal as a peer, or the principal never
admitted it, which happens when one side could not probe the other - the
notice goes by the principal's key over the lobby connection instead
(`LobbyRequest::Leave`), and the gateway rotates from the queue the lobby
handler fills (`spawn_departure_worker`). Either way the principal honours it
for a node on its member list, not only for a live peer: an approval that
outlives a departure is a machine that can walk back in. An admin revoking
an admitted machine (`POST /api/ui/network/admissions` with `forget`) is
treated the same. A node that leaves while the principal cannot be reached
at all keeps a key that still works until the next rotation.

**A rotation cuts nothing off.** The new key travels with the Unix second
from which it takes effect, 30 s ahead (`key_effective_at` in the settings
envelope, `auth::KEY_TRANSITION_LEAD_SECS`). Until that instant every node,
the principal included, keeps **sending** the old key; from it on, the new
one. Each node **accepts** both from the moment it holds the new key until
30 s past the switch (`KEY_TRANSITION_GRACE_SECS`), which covers clocks that
disagree. So two nodes that received the push never refuse each other: an
inference in flight, the `/handshake` admission probe and the node-to-node
calls all go on, and the members do not drop one another. The key a departed
node still holds stops working about a minute after the rotation. The
transition is kept in memory (`auth::begin_key_transition`); a node that
restarts in the middle of one simply uses the new key, which its peers
already accept. A member that never got the push is the case below.

**The member list, and claiming a rotated key.** The principal's approved
admission records are the network's member list: the node identities that
may be handed the connection key. `spawn_member_list_keeper` adds every peer
the principal currently admits that has no record, which is what covers
members that joined before admission requests existed; a node that just left
is held off the list for 10 minutes (`DEPARTED_HOLD`) so its last seconds in
the peer table do not put it back. The list lives on the principal only, so
there is nothing to distribute or to keep consistent.

A member whose key stopped being accepted - it was offline during a rotation
- gets back in by itself. `spawn_key_reclaimer` runs on every member: when
the principal is not among its admitted peers, it sends `ClaimKey { identity
}` to the principal's key over the lobby connection. The principal answers
with the grant only to a node on the list whose signature over the transport
verifies, and the member adopts the key without restarting. It works with
the lobby closed and on unlisted networks. A node that was removed is
answered without a grant. Both the rotation and the claim need the principal
online, and a claim is tried once a minute.

**What gates membership.** `NetworkSettings::open_admission` is true only for
a public network left on `anyone_with_anchor`, the mode networks created
before admission requests use: there the trust anchor alone admits
(`Network::public_peer_is_admitted`). Every other network, listed or not, is
gated by the connection key (`admission_without_probe` falls through to the
`/handshake` probe), and the interface no longer offers the open mode. The key
leaves the principal in three ways only: an approved admission grant, a
`ClaimKey` answered to a node on the member list, and the admin-signed
settings push to admitted peers. `network_settings_policy`, which answers
any well-formed signed envelope, never carries it.

**Other networks are not probed.** Every network shares one announce topic,
so a node hears the announces of all of them. A peer that announces another
`cluster_id` than the one this node is anchored to is refused outright
(`admission_without_probe`), with no `/handshake` probe: the probe falls back
to a relay for a peer behind NAT, and under libp2p those circuits - which
could never succeed - were what exhausted the relay.

**Discovery off.** A network with `visibility: local` announces nothing: the
node starts detached (`Network::detach`), publishes no pkarr record
(`PkarrPublishHandle::publish_if_changed`) and no directory record. It also
ignores the announces the shared gossip topic still delivers through its
relays (`discovery.rs`, `handle_announce` and the mDNS arm); before that, a
detached node dialed every announcing peer through relays. It is what
a first launch creates. Saving another visibility in Network settings rejoins;
saving `local` again detaches. Such a node still lists other networks, because
`GET /api/public/networks` also asks its relays (`directory_relay_keys`:
built-in, environment and configured relays, never itself) for their
`Directory`, and it still reaches lobbies, since the lobby client is an
endpoint of its own.

**Published stats.** The same protocol carries a third request, `Info`, that
is not part of the conversation: no card, no presence, and its own switch,
`network.public_stats` (off by default, public networks only), independent of
the lobby chat and of the network Chat. The answer is `NetworkInfo { models,
node_count, vram_total_mb }` - the three facts the Public networks screen
shows for the local network, computed the same way from the cluster view
(`webui::network_info_from_cluster`). The lobby handler never computes them,
because the cluster view reads every peer over HTTP: it answers from
`PublicStats` and, when the slot is missing or older than 30 s, wakes
`webui::spawn_public_stats_publisher`. So the first request after a quiet
period answers `info: null` and the next one has the numbers, nothing is
computed while nobody asks, and stats older than 5 minutes are not served.
`Info` is limited to 6 requests per 10 s per machine; with the switch off it
answers `stats_disabled`. Latency is deliberately absent: nobody measures a
route to a network that was not joined.

**Asking a node about public networks.** Two more requests carry public
data only and are answered by **any** node, principal or not:

- `Describe` answers the public network the node belongs to (zero or one
  entry) - what its unauthenticated `GET /api/public/network` answers, over
  the keyed connection, so it also reaches a node behind NAT. The public
  networks listing uses it as the fallback for a gossip candidate whose
  gateway does not answer over HTTP (`gateway::describe_by_key`); admitted
  peers are never asked, since they can only describe this node's own
  network.
- `Directory` answers the public networks the node knows. It is answered by
  nodes that gather them: a node in a public network and, whatever its own
  network, a node that serves as a relay (`gateway::gathers_public_networks`).
  Every cluster that uses a relay meets on it - all clusters share the one
  announce topic - so a relay is who a machine that has joined nothing can
  ask. Any other node answers `directory_disabled`.

- `Publish { identity, network }` is how a relay learns a network without
  having to reach it: the network's **principal** dials the relay directly
  and hands over its own description every 90 s
  (`gateway::spawn_directory_publisher`). The relay keeps it for 5 minutes,
  and only when the node key that signed for the connection is the primary of
  the trust document published (`gateway::validated_publication`); what a
  principal published is listed ahead of anything the relay gathered. This is
  the path that works when every node of a network is behind NAT - gathering
  needs the relay to reach a node it only knows by announce, and that fails. A network that turns discovery off simply stops
  publishing and falls out of the list within the 5 minutes.

Both are served from `DirectoryBoard`, filled by
`gateway::spawn_directory_board_publisher`: once at startup and then on
demand, at most once per 30 s. What principals published is answered at once
and never waits for the gathered part, which comes from a scan of every
announcing node that is bounded at 25 s and keeps the previous result when it
runs out; `pending: true` only happens before the first fill. An answer carries at most 64 networks and
2 MiB (`LobbyLimits::RESPONSE_BYTES`; requests are bounded at 768 KiB, sized
for `Publish`), and the
rate is the `Info` one. **An answer is a claim, not a fact**: the asker keeps
an entry only when its trust document matches its fingerprint and
`cluster_id`, and derives the publisher key from that document
(`gateway::validated_listed_network`). A relay can omit a network; it cannot
forge one or point it at another key. The descriptive fields (name, city,
`public_stats`) are as the answering node reports them.

Which relays a fresh installation asks: `DEFAULT_DIRECTORY_RELAYS`
(`network_lobby.rs`, one relay key today), the `POM_DIRECTORY_RELAYS` environment
variable (comma separated `pk:` keys), the `relay_peers` already in the
config, and the keys typed in the wizard. They are keys, resolved through
pkarr at dial time, never addresses.

The candidate's side is `LobbyClient`: a small endpoint of its own under the
derived transport key, dial only, on an ephemeral port and n0's relays, that
resolves the publisher's `pk:` key through pkarr at dial time. It is separate
from the node's endpoint on
purpose, so it also runs in setup mode, where the node has no cluster yet. It
is started on first use (`network_lobby::client`). End to end coverage of the
transport, the codec and the identity derivation is
`network_lobby::tests::a_candidate_reaches_a_principal_and_is_named_by_its_transport_key`;
the principal's handler (`discovery.rs::answer_lobby_request`) has no test
with a live cluster endpoint.

## 7. Defects noted

- **DEFECT NOTED: `Heartbeat` is unsigned.** A `Heartbeat` names a `node_id`
  and a `timestamp` and nothing proves either, so anyone on the LAN can keep
  a peer that went away looking alive for as long as it keeps sending them.
  It moves no address and grants nothing, and no code path emits it today
  (the signed `Announce` plays that role), so it is only a nuisance. Dropping
  the variant or signing it like the `Announce` fixes it.
