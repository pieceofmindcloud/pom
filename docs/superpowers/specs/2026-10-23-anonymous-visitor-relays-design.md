# Anonymous visitor relays

Date: 2026-10-23 - Status: proposal for review

## Problem

Visitors never know each other: a visitor does not admit, list, probe or dial
another visitor, and its announce goes only to admins and workers
(`docs/subsystems/security-model.md`, roles; `p2p-network.md`, "Admission
before the `peers` table"). That closes the privacy gap, but it also means a
visitor the internet reaches cannot help the network carry traffic.

Relaying today cannot be anonymous:

- The iroh endpoint id **is** the node key (`p2p-network.md`, section 0). A
  relay forwards packets by endpoint id, so it learns exactly which two nodes
  talk, and from which IPs. The payload is end-to-end encrypted QUIC; the
  relay never reads it.
- The embedded relay server admits by that same id: `RelayGate::allows`
  checks the connecting endpoint against the admitted members
  (`set_members`, refreshed by the policy round in `discovery.rs`), unless the
  node relays for everyone (`serve_relay`).
- A visitor relay that admitted other visitors would therefore learn their
  node ids, the network they belong to and their addresses.

The goal: let a reachable visitor relay for its network while learning only
"someone this network allows", never who.

## Goals

- A visitor can serve as a relay for its own network.
- The relay learns no node id, no role and no hostname of whoever uses it,
  and nothing that links two connections of the same client.
- The client of a visitor relay learns a relay URL, not that the relay is a
  visitor, nor its node id.
- No change for admin and worker relays, the n0 relays or `serve_relay`.
- The inner identity a call carries today (who is the caller, its membership)
  is still proved end to end.

## Non-goals

- **Hiding IP addresses from the relay.** A relay sees the IPs of both ends.
  Hiding them needs two or more hops in series (onion routing), which roughly
  doubles latency per hop; that is a poor fit for hidden state between
  pipeline stages. Out of scope; see "Later".
- **Traffic analysis.** Timing and volume can still suggest who talks to whom,
  more so on a small network.
- **File transfers.** A file never crosses a relay outside the cluster's own
  members (root `AGENTS.md`); a visitor relay does not carry them.

## Decisions

### 1. Ephemeral endpoints for relayed connections

A connection that goes through a visitor relay is dialed from an **ephemeral
endpoint**: a second iroh endpoint with a random key, created per relay and
rotated (for example every hour and at each restart). The relay only ever sees
ephemeral ids.

- Each node keeps its main endpoint (node key) as today. The ephemeral
  endpoint carries only the protocols a visitor relay may carry: the inference
  tunnels (`InferTunnelCodec`, `infer_tunnel::INFER_TUNNEL_STREAM_PROTOCOL`).
  Gossip, lobby, chat, logs, audit and direct announces stay on the main
  endpoint.
- Both ends need one: the callee also answers on an ephemeral endpoint, or the
  relay would learn the callee's node key from the destination id. The
  callee's ephemeral id reaches the caller inside its signed `Announce`
  (a new `relay_endpoints` field: ephemeral id plus visitor relay URL), never
  in a `PeerList` given to a visitor, and never on the gossip topic for a
  visitor's own announce (it already goes direct).

### 2. Identity proved inside the session

Today the QUIC handshake itself proves "I am node X", because the endpoint key
is the node key, and `TransportPeer` takes the caller from it
(`backend/serve.rs`). Over an ephemeral endpoint that proof moves one layer in:

- The first message on a connection from an ephemeral endpoint is an
  **identity binding**: the node id, the ephemeral id, a fresh nonce from the
  other side, and the node key's signature over all of them, plus the
  membership token. The callee checks the signature against the node id
  (`SHA256(pubkey) == node_id`) and the token against the chain, exactly as
  `is_member_key` does today.
- Only the two ends see the binding: it travels inside the encrypted QUIC
  session, so the relay sees ciphertext.
- `TransportPeer` is set from the verified binding instead of the endpoint id.
  Everything downstream (`caller_is_member`, route scopes, mutual TLS inside
  the tunnel) stays as it is.
- A binding that fails closes the connection. The nonce prevents replaying a
  captured binding on another ephemeral connection.

### 3. Relay passes from admins

The visitor relay cannot admit by membership any more, since it never sees a
node id. It admits by a **relay pass**:

- A compact EdDSA token like the membership token (`src/membership.rs`):
  `{net, relay, exp, jti}` signed by any admin of the chain. `net` is the
  cluster id, `relay` the relay URL it opens, `exp` short (10 min, like
  membership). No `sub`, no role, no key of the holder.
- Passes are issued in batches with the membership token renewal
  (`spawn_token_keeper`, `internal_token`): each renewal returns the token plus
  a few passes per visitor relay. A client uses a fresh pass per connection, so
  two connections of the same client do not share a `jti`.
- The relay checks the admin's signature against its own copy of the chain,
  `net`, `relay` and `exp`, and keeps the seen `jti`s until they expire
  (single use). It learns that an admin of its network let someone in.
- Admins learn who received which pass. That is accepted: admins already know
  every member. Blind signatures would hide it from admins too; see "Later".
- iroh's relay handshake carries no application token today. The pass travels
  either in a header of the relay's HTTP upgrade request, if the embedded
  server exposes it to `AccessControl::on_connect` (to verify against the
  iroh-relay version in use), or in a small pre-admission exchange on the relay
  port. Which one is the first open question of the plan.

### 4. Visitor relays announce only to admins

- A visitor whose NAT report confirms a public address (`RelayGate::reachable`)
  and whose operator allows it (a new option, off by default, in the relay
  settings of `RelayOptions.tsx`) tells the admins it can relay: a field in the
  direct announce, which only admins and workers receive.
- Admins list the visitor relays of the network and hand their URLs, without
  node ids, with the token renewal, next to the passes.
- Workers and other visitors only ever see a relay URL. A worker that also
  receives the visitor's direct announce knows it as a visitor already; the
  URL in its relay list carries no link to it.
- The relay URL itself must not name the visitor: a host name derived from the
  node, or its pkarr name, would. The URL uses the bare public address and
  port.

### 5. Who uses a visitor relay

- Only for inference tunnels between members when no direct path and no
  member relay of the cluster is available, ahead of the n0 relays. The order
  becomes: direct, hole punched, member relay (admin or worker), visitor
  relay, n0 relay.
- Never for file transfers (`Network::transfer_links` keeps refusing them).
- A visitor relay that refuses or drops connections loses its place in the
  list the same way a member relay does today.

## Changes by area

- `network/discovery.rs`: ephemeral endpoint per node, its router with the
  tunnel ALPNs only, relay pass check in `GateAccess` for a visitor relay,
  rotation.
- `network/network.rs`: `relay_endpoints` in the `Announce` and `PeerInfo`,
  kept out of `PeerList`s given to visitors; the relay order in
  `peer_route` and the link choice.
- `backend/serve.rs`, `backend/auth.rs`: `TransportPeer` from the identity
  binding on an ephemeral connection.
- `src/membership.rs`: the relay pass format, issue and check.
- `backend/webui/membership.rs`: passes and relay URLs in the token renewal,
  `jti` ledger on the relay.
- Frontend: the opt-in on the relay options, and the visitor relay count on
  the admin's network screen.
- Docs: `security-model.md` (what a visitor relay learns), `p2p-network.md`
  (ephemeral endpoints, relay order), `http-api.md` (renewal answer).

## Testing

- The binding: a valid one sets `TransportPeer`; a wrong signature, a token of
  another network, a revoked member or a replayed nonce closes the connection.
- The pass: a pass for another network, another relay, expired, or reused is
  refused; a fresh valid one is admitted.
- Two visitors and an admin on `Network::testkit` plus a loopback relay: the
  relay's access log holds only ephemeral ids, none equal to a node id, and
  the client's call still reaches the callee as its real node.
- A `PeerList` given to a visitor never carries `relay_endpoints`.

## Open questions

- Where the pass travels in the relay handshake (decision 3).
- Whether the callee needs one ephemeral endpoint per relay or one shared by
  all relays (one per relay leaks less across relays, costs more sockets).
- Rotation period, and what happens to an inference session open across a
  rotation (keep the old endpoint until its connections close).
- Whether a visitor relay should also be offered to visitors only, or to
  workers and admins as well.

## Later

- Two-hop routing for a visitor's own chat and lobby traffic, where latency
  matters less, to hide IPs from a single relay.
- Blind-signed passes, so admins cannot link a pass to the member who used it.

## Out of scope

Hiding IP addresses from a relay, resistance to traffic analysis, and relaying
file transfers through a visitor.
