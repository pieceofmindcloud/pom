# Spec: iroh-less nodes (the worker the outer world never sees)

Date: 2026-11-15. Status: **accepted in outline** - the two decisions marked
here were given by the operator the same day; the rest needs sign-off before a
plan is written. Builds on the admin-hub line
(`2026-11-14-admin-hub-routing-design.md`), whose visitor view is what makes
this safe to consider.

- Problem: every node today runs an iroh endpoint (gossip, relay client, hole
  punching, tunnels). A worker behind the hub is never dialled by the outer
  world and rarely by visitors, yet it still carries the whole iroh surface:
  a key on the public relays, gossip traffic, punch attempts.
- Decision (given): nodes can **disable iroh support as a whole**. Admins that
  serve the internet keep iroh - it is how people behind NAT reach them as
  visitors. A worker with iroh off declares itself **reachable by address**; a
  worker that needs punching or relays keeps iroh. It never serves as a member
  relay with iroh off (given: no).
- Decision proposed: such a worker **stops publishing to pkarr** and keeps only
  its consuming side (cold dial by key). The hub learned its addresses from its
  announce; the DHT needs none of them, and publishing exposes the machine the
  hub exists to hide.

---

## 1. Today's baseline (what the code does now)

Cited from `docs/subsystems/p2p-network.md` and the folder `AGENTS.md`s.

- **The iroh endpoint is the internet spine.** `discovery.rs` builds it under
  the node key (`iroh::Endpoint::builder(presets::Minimal)`, UDP
  `p2p_port + 10000`) with iroh-gossip (one topic per network cluster), the
  relay client map (`apply_relay_policy`), hole punching, and the codecs: the
  inference tunnel (`InferTunnelCodec`), the anonymous tunnel
  (`ANON_TUNNEL_PROTOCOL`), the direct announce (`AnnounceCodec`,
  `pom/announce-direct/1`), the lobby. Cold dial arrives through
  `Discovery::dial_tx` (`spawn_pkarr_bootstrap`, every 30 s).
- **The data plane is already iroh-free.** `quic.rs`: "The endpoint is optional
  on the client" - inference hops are HTTP/3 over plain QUIC on ordinary ports
  (`gateway_quic_port`, `inference_quic_port`), HTTP fallback, mTLS from node
  certs; control calls are plain HTTPS on the gateway port. iroh only enters
  when a *dial* cannot reach an `ip:port`.
- **How a worker reaches the network today.** Its announce rides the gossip
  topic; it learns peers from the same; the connection-key proof runs over the
  shared UDP socket on the LAN (`session::handle_handshake`) and over HTTP
  `POST /handshake` on a remote peer's gateway port (`network.rs`), so the
  proof has a plain-transport form already.
- **The visitor precedent.** A visitor does not publish on gossip: each round
  it sealed-direct-announces to the admins in `alive_peers` and to the
  endpoints it dialed through pkarr (`direct_announce_targets`), and it stays a
  subscriber to hear them. This is the shape an iroh-off worker copies, minus
  the subscription.
- **pkarr publication is unconditional.** `gateway::PkarrPublishHandle`
  publishes, always: direct addrs (`a`, max 4), relay URLs (`x`), gateway port
  (`h`), inference port (`i`), is-relay (`r`), geo (`g`) - private addresses
  excluded. `spawn_republisher` republishes on change and every 30 min.
  pkarr is "the key-to-address resolver, and only that": steady-state routing
  never consults it.
- **Member relays enter through announces.** A worker's relay offer
  (`peer.relay`, `RelayOffer`) reaches the network's relay map (`Network::install`) only
  via its announce. The out-of-band channel for relays is the trust document's
  `relay_hints` (`--serve-relay` adds one; `relay_peers`).
- **Planning already refuses unreachable candidates.** A peer with no usable
  route is not a deploy target and a hole in the ranges is a refusal
  (`deployment.md`); no new machinery is needed for the refusal, only a named
  reason.

## 2. What "iroh off" removes - and what keeps working

Removes, wholly:

1. the endpoint (no identity on n0's relays, no relay client, no home relay),
2. gossip (no topic publish *or* subscribe),
3. hole punching,
4. the inference tunnel and the anonymous tunnel, as caller and as carrier,
5. the embedded relay server (`--serve-relay` is refused with the flag),
6. the relay-announce machinery: `relay.offers`, `relay.everyone`,
   `relay.transfers`, `relay.anonymous` (the My machine screen shows them
   locked off),
7. pkarr **publication** (the record; see section 4),
8. the anon-tunnel homing (`AnonTunnelAddr` absent from its announce).

Keeps, untouched:

1. the UDP LAN path (broadcast, unicast to kept peers, mDNS, the LAN
   handshake),
2. plain QUIC + HTTP/3 inference hops and plain HTTPS control/API calls
   (mTLS unchanged),
3. pkarr **resolution** and cold dial by key (it dials out; outbound is not a
   NAT problem),
4. file transfers - already direct-only by rule (`file-transfer.md`: a file
   never crosses a public relay),
5. `/handshake` over HTTP on the admin's gateway port - the admission proof,
6. everything the hub line built: delegation, vouches, deploy, idle wakeups -
   all of it is gateway-port HTTP that the worker dials out to.

## 3. The role contract

- **Flag**: `--no-iroh` / `POM_NO_IROH=1`, persisted in `pom.json` per machine
  (`network.node_transport`, the same place the relay options live), so a
  restart keeps the mode.
- **Accepted on any profile; the principal refuses it** (it must answer pkarr
  lookups for the network's directory and cold dialing; if an operator truly
  wants that off, `--no-pkarr` already says it out loud).
- **What such a node may never do** - refused at the route level, an admin
  request included (the role-enforcement pattern of `http-api.md`): become a
  deploy target without a measured direct route (new refusal reason
  `worker_not_reachable`), serve as a member relay or an anonymous relay,
  accept a tunnel request, or appear in any relay map.
- **The operator's guarantee**: the machine answers on its announced addresses
  (public IP or a LAN the hub shares). The operator declares reachability; POM
  measures it; the hub refuses it when the measurement fails. Nothing tries to
  fix it behind the operator's back - no punch, no relay fallback.
- **Admins keep the choice per machine**: an admin exposed to the internet
  keeps iroh (that is where NAT-hidden visitors enter); an admin on a LAN or a
  VPS with a public IP may run iroh-off and still serve - clients reach it by
  address, and a visitor behind NAT simply cannot use that network's entry,
  which the entry screen should say.

## 4. pkarr: publish off, resolve on (proposed)

The record carries `a` (direct addrs), `h` (gateway port), `i` (inference
port). In this model a worker's record is consulted by exactly no one who is
allowed to dial it: visitors hold only admins (visitor view), the hub already
holds its addresses from the announce, and cold dialers are the node itself.
Publishing then only leaks: the operator's IPs and open ports, signed by the
node's identity key, into an append-only public DHT - the machine the hub line
exists to hide, made findable forever.

- The iroh-off node publishes nothing and republishes nothing; `resolve` and
  `spawn_pkarr_bootstrap` stay on - it finds principals and `--pkarr-peers` by
  key like everyone else.
- Leaving the record stale is **not** accepted: a vanished node must not stay
  dialable at old addresses. On the mode change the node deletes its record
  (`pkarr_node` delete path, then `spawn_republisher` re-checks emptiness).
- Related finding, out of scope here but recorded: an *admin's* record also
  publishes the inference port `i` and up to 4 direct addrs. API keys are local
  to the minting node, so the inference port is not an open door - but the
  same trimming argument applies to it in miniature (gateway port + relays is
  all an outsider needs). A follow-up knob, not part of this line.

## 5. The hole this opens, honestly

1. **Announce transport.** Without gossip or pkarr publication, the announce
   needs a dial already made: `bootstrap_nodes` (an IP or internal DNS name), a
   pkarr-resolved principal, or the LAN. The first connection is by hand; the
   sealed direct announce (`AnnounceCodec`) then maintains it. Design for the
   plan: seed `alive_peers` candidates from the chain's principals
   (node -> addrs, the same `dial_addrs` bootstrap uses), then run the
   announce round direct-only.
2. **Worker-run relays for NAT-only members.** With the rule "no", the answer
   for a NAT-only member is an admin or a reachable member that keeps iroh and
   relays, or `relay_hints`. This closes a real (small) use case: today a
   NAT-only worker's relay offer crosses the relay gap for other members.
3. **Idle wakeups and reload fan-out** reach idle peers through the plan's
   peers (`deployment.md`): they are plain HTTP dials out of the gateway, so
   they survive - but a *wake dial into* an iroh-off worker still needs its
   measured route; the refusal reason names it.
4. **Two nodes on one machine** (`operations.md`): both share the LAN;
   iroh-off works there today (direct addrs) and is unaffected.

## 6. Decisions

1. **Given (2026-11-15): the flag disables iroh as a whole; admins serving
   the internet keep it.**
2. **Given (2026-11-15): an iroh-off node never serves as a member relay.** A
   worker that relays or needs punching keeps iroh.
3. **Proposed: publish off, resolve on**, with record deletion on the mode
   change (section 4).
4. **Proposed: refusal at planning is the enforcement** (`worker_not_reachable`),
   not a connectivity guarantee: the mode is the operator's declaration, the
   measurement is POM's.
5. **Proposed: compatibility posture.** This ships as an *additive* capability:
   the announce gains "this node is direct-only" (an announce field optional
   in `frame/v1-batch` terms, older nodes ignore it), the new refusal reason is
   additive, and mixed clusters keep working - an unmigrated worker keeps iroh.
   Unlike the roster-vouch line, no version moves together for this one.

## 7. What implementation would touch (for the plan)

- `main.rs`: the flag + `pom.json` persistence; `discovery.rs`: the endpoint,
  gossip, relay map and tunnel codecs become conditional (today the endpoint
  is the spine of `Discovery::new`); `Network` carries the mode so screens and
  planning read it.
- Announce: a `direct_only` field; the announce round for a direct-only node
  runs the `direct_announce_targets` path seeded from the chain's principals
  (the visitor's pattern generalized); receivers treat a direct-only peer's
  announce exactly as today's sealed direct announce.
- Planning: `routing/` refuses a direct-only candidate whose measured route
  fails, with `worker_not_reachable`; the planner already knows peer addrs.
- `pkarr_node.rs` + `gateway::PkarrPublishHandle`: publication off, record
  deletion on the mode change, resolution untouched.
- UI: My machine gets the switch (worker/visitor), the relay options show
  locked; the Networks/entry screen names an entry without iroh.
- Docs same commit: `p2p-network.md` (the mechanism table gains the mode),
  `operations.md` (bring-up of a direct-only worker), `deployment.md` (the new
  refusal), My machine guide.