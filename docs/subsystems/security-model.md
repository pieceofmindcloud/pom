# Security model

The nodes themselves enforce every role, between nodes as well as on the
screen. The trust chain names the admins only, and every other machine proves
its role with a short-lived membership token signed by an admin. This guide is
the overview; `http-api.md` has the full route reference and `p2p-network.md`
the transport.

## Roles

A network has one principal, any number of secondary admins, and members that
are either workers or visitors.

| Role | Proven by | May do |
| --- | --- | --- |
| Principal | The trust chain (anchor and transfers) | Everything: settings, keys, grants and demotions, admissions, removals, deploys |
| Secondary admin | A grant signed by the principal in the trust chain | Deploy and undeploy models, view logs and telemetry, approve and reject admissions, remove workers and visitors |
| Worker | Membership token with role `worker` | Serve models; reach other nodes' serving routes (catalog, planning, transfers) |
| Visitor | Membership token with role `visitor` | Run inference through its own node; never holds or serves a model; no administrative call to any node |

- Every profile installs plugins on its own machine.
- A machine's own operator routes (API keys, plugins, its name, relay,
  networks, its logs) are reached by no other node, whoever signed the request.
- The role is set at admission. A machine admitted as a visitor stays one
  whatever it announces, and the principal refuses to promote it
  (`visitor_target`).
- Visitors never know each other, and so have no network Chat by default: it
  names every member who talks. An admin opens it to them with
  `NetworkSettings::chat_visitors` (off by default; on, visitors see each
  other there). It is a network setting, not part of the trust document: the
  trust document names admins only, and this rule is enforced by the
  principal, the hub every Chat message crosses, so a member's copy of the
  setting decides nothing for others. Off, the principal refuses a visitor's
  message and relays to no visitor (`relay_chat`), and a visitor node refuses
  to send or show it (`visitor_has_no_chat`, a 403 on `/api/ui/network-chat`
  and its action and file routes). The lobby of another network is not the
  Chat and stays open.
- The lobby is the admins' only: each candidate talks in a thread of its own
  that only the admins the chain proves receive, erased once an admin approves or
  declines it, or after a day without messages (`p2p-network.md`, 6.1).
- A visitor needs the admins and the workers
  that serve it, so its `Network::admit_peer` refuses another visitor and
  `alive_peers` leaves one out (`Network::hides`): it never probes, measures,
  gossips with or lists one. Every node answers a `PeerRequest` with visitors
  only to a requester it knows as an admin or a worker
  (`Network::peer_list_for`). The role is the peer's token checked against the
  chain, else its announced profile. A visitor's own announce goes straight to
  its admins and workers, never on the gossip topic or the LAN broadcast
  (`p2p-network.md`, "Admission before the `peers` table").
- The principal decides which iroh relays the network uses
  (`NetworkSettings.relay_policy`: `all`, `members` for only the network's own
  dedicated and member relays, `none` for direct connections only). A relay
  never reads the end-to-end encrypted QUIC it carries, but it sees which
  endpoints talk, when and how much; the policy keeps third parties out of
  that. Each machine enforces it on its own side by keeping a disallowed relay
  out of its relay map, so a machine that ignores the policy still cannot reach
  a compliant one through such a relay (`p2p-network.md`).
- A visitor may relay for its network anonymously (`relay.anonymous`, off by
  default): it admits anyone with an admin-signed relay pass for its URL
  (`crate::relay_pass`). An admin signs passes only for a relay URL on a
  public host that is one of that visitor's own addresses
  (`relay_url_is_peers`), so a visitor cannot send members to another host.
  The relay keeps at most 2 connections open per pass and 64 for all pass
  holders together (`PassLedger`, freed as each connection closes), so pass
  holders cannot take over the visitor's relay. The bytes a connection
  carries are not capped: `iroh-relay` only offers one rate limit for the
  whole server, which would also slow the member traffic of that relay.
  The tunnels through it run between ephemeral
  endpoints whose caller proves its node inside the encrypted session
  (`network/anon_tunnel.rs`). The relay learns the IP addresses of both ends
  and that an admin let them in, never which nodes they are. Admins know who
  received which pass.
- A visitor node refuses to hold or serve a model even when an admin asks
  (`visitor_profile`): `apply`, `launch`, `deploy`, `reconfigure`, `slice` and
  every `/api/transfer` route are refused before any other check.

## Identity and trust chain

Each node has a single Ed25519 key (`.pom-node.key`). Its node id, iroh
endpoint id, pkarr key, TLS certificate and authority key all derive from it,
and an IP address is never an identity.

- **Trust chain (`AdminTrustDocument`, `admin_trust.rs`)**: an anchored
  principal, secondary grants signed by the principal, signed demotions,
  principal transfers (an offer plus the successor's acceptance) and signed
  revocations. Every signature is checked back to the anchor.
- **Admins only**: the chain carries no member roles. Admitting a worker or a
  visitor changes no trust document.
- **Version** `<transfers>.<grants>.<demotions>.<open transfer>-<hash>`: a node
  adopts a copy with higher counters from any member, so a node that was away
  catches up while the principal is offline.
- **Signed control envelope**: node-to-node control calls carry a signature over
  the method, path, a hash of the body, a nonce and the issue time, with a
  5 minute window and replay protection. The receiver authorizes on that
  signature.
- **Connection key**: a shared secret every member holds, visitors included.
  It guards the transport and opens no administrative route on its own.
- **Admission handshake**: the prober sends a random 32-byte challenge, and the
  peer answers with an HMAC-SHA256 of it and both node ids, keyed by the
  connection key and signed with its node key. The key never travels and an
  answer cannot be faked.

## Membership tokens

A worker or visitor proves its role with a compact JWT signed with EdDSA by an
admin's node key (`membership.rs`). It lasts 10 minutes and is accepted only
from the node and key it names.

| Claim | Meaning |
| --- | --- |
| `iss`, `iss_key` | Issuing admin's node id and public key |
| `sub`, `sub_key` | Member's node id and public key |
| `net` | The chain's cluster id |
| `role` | `worker` or `visitor` |
| `iat`, `exp`, `jti` | Issued at, expiry (iat + 600 s), unique id |

1. **Request**: a non-admin node calls `POST /api/internal/membership/token`
   on an admin (the principal first, then the secondaries), signed with its own
   key.
2. **Issue**: the admin issues a token only for an approved, unrevoked
   admission, with the role admitted. On a network open to anyone with the
   anchor (`open_admission`), the announced role is used.
3. **Carry**: the node puts the token in its announce (`membership`) and
   renews it two thirds of the way through, or at once if it stops being
   valid. The keeper checks every 30 s.
4. **Check**: any node checks it against its own trust chain alone:
   - the header must be exactly `{"alg":"EdDSA","typ":"JWT"}`;
   - the signer must be an admin in the current chain;
   - `net` must match;
   - not expired, with 30 s of clock leeway;
   - lifetime no longer than 10 minutes;
   - `sub` and `sub_key` must equal the peer's node id and key, so a copied
     token proves nothing;
   - the member must not be revoked after `iat`.

Demoting an admin voids every token it signed once the chain syncs.

## Revocation

A ban is a revocation entry `{net, sub, sub_key, revoked_at, issuer,
issuer_key, signature}` (`membership::Revocation`), so it holds while the
principal is away.

- Any admin signs entries for workers and visitors. An admin cannot be revoked
  this way; only the principal removes an admin, through the trust chain.
- Each entry is signed on its own, so lists from different admins merge as a
  plain union.
- The banning admin pushes the entry to every live peer at once, and every node
  trades its list with an admin each 30 s tick
  (`/api/internal/membership/revocations`), so a missed push still arrives.
- A token for a revoked member is refused when it was issued at or before
  `revoked_at`, and admins do not renew it unless the member was approved again
  after the ban. Live requests are checked against the list too, so a ban takes
  effect on the next request, not at token expiry.
- The principal, on seeing a revocation of a member it approved, forgets the
  admission and rotates the connection key, as with a departure.

## Transport

- **Mutual TLS between nodes, always.** Each node presents its certificate,
  derived from its node key and verified by `SHA256(pubkey) == node_id`, as
  server and as client. There is no switch to turn it off.
- **The connection key alone opens nothing.** A call authenticated by the key is
  accepted only when the transport proves the caller is a current member: an
  admin of the chain, or a node holding a valid, unrevoked membership token.
- **The iroh endpoint** authenticates the endpoint id, which is the node key,
  on every connection; the receiving side hands that identity to the request
  the same way. Announces over gossip are signed by the node key and dropped
  when the signature does not verify.
- **Plain HTTP** is served only to loopback by default, so the operator opens
  `http://localhost` without a certificate warning. `--no-tls` / `POM_NO_TLS=1`
  allows remote HTTP for the browser/client surface behind an HTTPS reverse
  proxy; it never changes the peer mTLS policy. The published Docker image sets
  it by default and requires a private network between the proxy and port 8080.
  `POM_INSECURE_ADMIN_UI=1` is the older development-only switch.

## Request authorization

Every route has a scope in one table (`webui/routes/mod.rs`). A path missing
from the table is treated as administrative. The browser goes through the
session gate; another node goes through `caller_may_ask` (`gateway/state.rs`).

| Scope | Browser on this node | Another node | Refusal reason |
| --- | --- | --- | --- |
| PrincipalOnly | Principal | Signed by the principal | `signature_required`, `principal_required` |
| AnyAdmin | Any admin | Signed by any admin in the chain | `admin_required` |
| ServingMember | Admin or worker | Signed by an admin, or by a peer with a valid worker token | `visitor_caller`, `role_unattested`, `unknown_caller` |
| AnyMember | Every profile | Every member (inference, leaving) | - |
| LocalOnly | Every profile | Never | `local_operator_only` |
| HandlerAuth | The handler checks | The handler checks (sync protocols) | - |

- `/api/transfer/*` (model files) needs a signed request; the connection key
  alone pulls nothing.
- From another node, starting, cancelling, retrying or dismissing a model
  download (`POST /api/ui/model-provider/downloads...`) and
  `/api/ui/models/rescan` take an admin's signature, not a worker's
  (`node_to_node_scope`): a worker downloads for itself, only an admin makes
  another machine download. Reading the downloads stays `ServingMember`.
- A forwarded `POST /api/ui/leave` (removing another machine) needs an admin's
  signature, and the principal's when the machine leaving is an admin.
- **The administrative UI** uses an in-memory session with an `HttpOnly`,
  `SameSite=Strict` cookie, `Secure` on TLS, issued for `/api/ui`,
  `/api/transfer`, `/admin` and `/admin-ui`. The human password lives only in
  the tab's memory; the node keeps its Argon2id hash.
- **Plugins** are reached through the node: the plugin proxy and the public
  catalog relay (`/api/ui/plugin-catalog`) are same-origin, so the browser never
  calls a plugin or the store directly.

## API keys and inference

An API key never leaves the node that minted it.

- `POST /api/generate` and every `/v1/` route accept only an active key minted
  by this node (`ApiKeyAuth`, local `pom.db`). Keys minted elsewhere are
  forgotten at startup (`forget_foreign_api_keys`).
- Keys survive joining or leaving a network, so a client keeps its credential
  whichever cluster serves the inference.
- The cluster authenticates the inference by the gateway node's membership,
  never by the client's key: each hop's inference port requires the transport
  identity of a current member.
- The audit records the key's non-secret identity and the `trace_id`, and
  replicates them to the admins; the secret never leaves the node.
- The network Chat checks the browser's key on the sender's own node and clears
  it before the request leaves; the principal admits the message by the
  sender's transport identity and membership.

## Logs, audit and plugins

Admins see every member's logs and request audit; any other node sees only its
own.

- **Fan-out**: cluster logs and audit events go over the iroh endpoint only to
  the admins the chain proves, never to a node that merely announces `admin`.
- **Ingestion is bound to the sender**: a log batch is accepted only when its
  source and every entry's `node_id` equal the sending node. An audit event's
  entry node must be the sender, and only that sender completes it.
- **Reading**: on a non-admin, the request-log query is forced to the node's own
  `node_id`.
- **Plugins**: a peer call carries a `caller` object with its `role`
  (`principal`, `admin`, `member`, `visitor`) and `head: false` for visitors.
  Each plugin must check `caller.role` itself.

## GPU work metering

- **Three token kinds, three headers.** Work receipts (signed by the hop),
  bundles (countersigned by the gateway) and cycle statements (signed by the
  ledger admin's node) are compact Ed25519 tokens like membership tokens, each
  with its own `typ` (`<slug>-work-receipt/v1`, `-work-bundle/v1`,
  `-cycle-statement/v1`), so none can pass for another. Each names its signer's
  key and the signer's node id must be that key's hash.
- **A receipt never names the gateway** (only an opaque nonce); only the bundle,
  delivered to the ledger alone, does. A node reads only its own statements.
- **The node signs a statement only for its own plugin**
  (`node.sign_statement` requires the local marker the host API adds and the
  peer route strips). The other `node.*` metering operations answer admins only.
- **Visitors.** A visitor gateway's bundle pays the hops (it is the consumer)
  but never earns the gateway fee.

## Charging visitors

A charging network (`docs/subsystems/payments.md`) runs
`inference_access = member_gateway_only`: a visitor never reaches a worker's
inference port (`visitor_must_use_gateway`) and learns only the entry nodes
the policy allows, so it cannot skip the entry node that charges it. The
account is named by a home statement signed by the visitor's home principal
and checked against the key the transport proved (`crate::home_statement`); no
address or name enters it. Only the principal writes the authoritative book;
other entry nodes spend leases it grants, so with the principal away charging
fails closed.

## People signed in through a plugin

A node can serve people, not only its operator. The node knows no identity
provider: a plugin (the logs plugin) answers `auth.providers` with the login
screen's buttons and owns every route under `/auth/<code>/` (forwarded as the
query `auth.http`). When a sign-in completes, the plugin answers a `session`
and the node opens a session of kind `User` (`crate::user_session`); the
plugin never sees a cookie. Design:
`docs/superpowers/specs/2026-11-07-demo-mode-and-user-sign-in-design.md`.

- **User surface.** A user session reaches only the paths in
  `user_session::user_may_call` (dashboard reads, the Chat, request logs and
  statistics, the RPCs and assets of plugins that offer sign-in, `/api/ui/me`,
  sign-out and its own keys). Anything else answers 404. It never reads the
  connection key or other credentials (`user_session::redact`).
- **Inference.** `POST /api/ui/chat-key` gives a user session an in-memory
  token bound to it (`sk-pom-u-...`, eight hours) instead of the node key.
  `Caller::user_id` carries the person into the audit, the quota check and
  the logs plugin, which scopes every read by it (`profile: "user"`).
- **Own API keys** (`/api/ui/my-keys`) are stored with `api_keys.user_id`;
  refused in demo mode.
- **Operator path.** While any provider is offered the password header opens
  no session; the password is accepted only by `POST <POM_OPERATOR_PATH>/login`,
  with a per-address lock-out after three failures (doubling up to 15 min) and
  a global one past 30 failures a minute. That session ends after 30 minutes
  idle.
- **Demo mode needs the principal** (`user_session::demo_mode`). `DEMO_MODE=1`
  only asks for it (`demo_requested`): the demonstration runs while the
  network's principal allows it, set by `DEMO_MODE_ACTIVE=true` in the
  principal's environment (no screen sets it). The principal puts it in
  `NetworkSettings.demo_mode_active` of the network settings it answers and
  fans out (`/api/cluster/network-settings`), whatever an edit sent; a member
  records what it receives from the principal (`set_demo_authorized`), and the
  principal reads its own variable. Until then, and when the principal stops
  allowing it, a person's session gets 503 `demo_not_authorized` (never the
  ordinary user surface) and the guest sign-in is off. A node asked to be a
  demonstration keeps fetching the principal's settings instead of stopping
  after the first sync, so a withdrawal reaches it. An older principal sends no
  field, which reads as false.
- **Demo mode** (`DEMO_MODE=1`) removes own API keys and redacts from what a
  user session reads every address-shaped field and value and every node id
  (replaced by a stable per-process alias), on the dashboard reads, the
  request log and plugin RPC answers. A machine's name (`hostname`) stays, so
  a person can tell the machines apart, unless the value is itself an address;
  an alias the page sends back as the Chat's `inference_head` is resolved to
  the real node among this node and its live peers (`unalias_node`). It also shows a person, read-only, the
  installed plugins and the catalog (`GET /api/ui/plugins`,
  `/api/ui/plugins/github-repositories`, `/api/ui/plugin-catalog`), the local
  catalog (`GET /api/ui/models`), a slicing preview (`POST /api/ui/slice/plan`;
  starting a job stays refused), the current network (`/api/ui/network/info`,
  redacted) and their own budget (`GET /api/ui/quota`, answered from the
  sign-in plugin's `rpc.me`), the Networks screen (`GET /api/ui/networks/known`,
  `/api/ui/networks/pinned`, `/api/ui/network/relay`, redacted; adding,
  sharing, joining, leaving and saving the quick setup stay refused), the
  network Chat (`GET /api/ui/network-chat`, the members' room only: a `room`
  other than it answers 404 and the snapshot never carries the lobby) and
  the Deploy screen's survey (`POST /api/ui/deploy/plan`, `GET
  /api/ui/cache-types`, `/api/ui/model-sidecar-links`; `/api/ui/deploy` and
  the history stay refused). A demonstration node is a worker, which refuses
  the deploy routes, so the gateway lets that one survey through on a worker
  in demo mode (`demo_previews_deploy`). A person's message to the network
  Chat (`POST /api/ui/network-chat`) never reaches `network_chat_action`: the
  session gate takes only the text of a text message to the members' room,
  up to 2000 characters, prefixes it with `[demo] ` and posts it as this node
  (`demo_network_chat_post`); files, typing, notices and direct messages are
  refused. Plugins listed in `DEMO_PLUGINS` (every active
  one when unset) open their assets, RPCs and web proxy to a person; the
  sign-in plugin's `auth` screen and its `rpc.auth.*` never do.
- **Demo without sign-in** (`DEMO_NO_AUTH=1`, only with `DEMO_MODE`). A
  plugin whose `auth.providers` answer carries `"guest": true` opens guest
  sessions: `/api/access` names its `guest_url` and the login screen goes
  there at once, showing only the brand animation. The node passes
  `demo_no_auth` and `guest_key` (a hash of the visitor's address,
  `User-Agent` and `Accept-Language`) to the plugin's `auth.http`; the plugin
  refuses `/guest/start` unless `demo_no_auth` is true. A guest is a user
  session like any other (provider `guest`), so it gets the same seats,
  redaction and quota. When a guest's seat runs out the session is revoked
  and, for `DEMO_GUEST_COOLDOWN` minutes, every session of that guest ends
  and a new one is refused (`/admin-ui/?demo_cooldown=<secs>`; the `/` redirect keeps a query string, and a tab sent to the guest start that comes back without a session within 15 s stops at the gate instead of reloading). Anyone can change
  their fingerprint (another browser or address): the cool-down and the
  per-guest budget slow casual reuse, they are not identity.
- **Demo seats** (`backend/demo_slots.rs`). In demo mode a signed-in person
  takes one of `DEMO_MAX_SESSIONS` seats (default 10) for
  `DEMO_SESSION_TIME` minutes (default 20). Past the seats they wait in a FIFO
  queue and reach only `/api/ui/me`, `/api/ui/demo/session` and sign-out (423
  `demo_queue` elsewhere); `GET /api/ui/demo/session` answers the seat
  (`state`, `remaining_secs`, `expires_at`) or the place (`position`,
  `waiting`, `eta_secs`, estimated from the seats' remaining times), plus
  `active_sessions`, the seats held now, which the top bar shows beside the
  clock. The queue screen (and a guest start that came back without a
  session) reloads itself every 5 minutes to try again; a visit whose time
  ran out does not, so a tab left open never signs back in. After signing out,
  a guest tab stays at the gate ("You left the demonstration") until the
  visitor asks for a new visit. A queued
  place nobody polls for 45 s is dropped. Sign-out frees the seat at once; an
  expired seat revokes the session and its chat tokens (401
  `demo_session_expired`). One person holds one seat across browsers. The
  state is in memory: a restart empties it. The Chat's image generation
  (`POST /api/ui/chat/image`, `GET /api/ui/chat/image/:hash`) is on the
  user surface, and a demonstration visitor also reads
  `GET /api/ui/image-deployments` (the Dashboard's image models).
- **A person's folder.** `<workspace_root>/users/<hash>` (`user_folder`: 16 hex
  of SHA-256 over plugin and user id), created on first use. The plugin proxy
  sends it to the plugin as `x-pom-user` (the hash) and
  `x-pom-user-workspace` (the path), removing any such header the browser sent;
  RPCs carry it as `user_workspace`. Keeping each person inside it is the
  plugin's job.

## Known limits

- **A session chat token can be copied** from the browser and used for the
  life of the session; it carries the person's id and quota, no more.
- **Redaction is by shape**: a new field naming an address under an unexpected
  key passes through until `user_session::redact` learns it.

- **Rogue secondary admin**: while the principal is offline, a secondary admin
  can still issue tokens and ban members until the principal removes it;
  expiring admin grants is not implemented.
- **Principal offline**: secondaries issue tokens from their copy of the
  admission list but cannot decide admissions. A secondary that restarts while
  the principal is down has an empty copy until it returns.
- **Open-admission networks** trust the announced role.
- **Unsigned peer snapshots**: a `PeerList` never moves the addresses of a peer
  heard first hand in the last 30 s, but can seed a forged address for a peer
  not yet known; TLS pinning then refuses it, so that is a denial of service at
  most. The UDP `Heartbeat` is unsigned and can keep a departed peer looking
  alive.
- **Clock skew** beyond 30 s breaks token checks between the affected nodes.
- **Plugins** enforce roles themselves through `caller.role`.

Code: `crates/node/src/membership.rs`, `admin_trust.rs`,
`backend/webui/membership.rs`, `backend/gateway/state.rs` (`caller_may_ask`),
`backend/auth.rs`, `backend/webui/routes/mod.rs`,
`inference/infer_server/prefix.rs` (`require_inference_auth`).
