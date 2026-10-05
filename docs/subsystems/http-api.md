# Subsystem: HTTP API (gateway) and authentication

The HTTP gateway is the pom node's network surface, mounted by
`start_gateway_with_ready` (`crates/node/src/backend/gateway/http.rs`) as a
single axum router. **Every node serves the same set of routes on the gateway
port** (`--gateway-port`, default 8080), so any peer address works as an entry
point - there is no "gateway node". The router binds with
`axum::serve(...).tcp_nodelay(true)` (Nagle delayed the small SSE writes) and
has a `CompressionLayer` (which already skips `text/event-stream`, so the token
stream is not compressed). The gateway listener stays dual-stack because it
also carries the pipeline's HTTP fallback; administration protection is a route
boundary enforced on the server.

Ports, for orientation: UDP P2P `--port` (42777), libp2p TCP = UDP + 10000
(52777), HTTP gateway `--gateway-port` (8080), and the inference server on
`inference_port` in `pom.json` (8181, `inference/infer_server.rs`, a separate
router that serves `/v1/prefill`, `/v1/step`, `/v1/step-stream`, `/v1/link`,
`/health` - it does not go through this gateway). The gateway also registers
`/v1/link` for the full-duplex return of `remote-stream`; both listeners use the
same framing, but the persistent callback ends in the gateway's `ReplyInbox`.
The inference server's `/health` answers `model_loaded` (which promotes the
slice record to `serving`, see `slicing.md`) and exposes the executable identity
`model_label`/`deploy_id` and its range `start_layer`/`end_layer`/
`total_layers`. The gateway uses that range during admission when the P2P
announcement is still late. The endpoint also exposes `kv_can_shift`, the
condition for the head to draft in speculative decoding. It **does not wait for
the engine**: with the `InferState` lock held (a prefill chunk may take
seconds), it returns the last snapshot with `busy: true` and `snapshot_age_ms`,
recomputing only the scheduler queue and the routes; the snapshot is stored
again whenever the lock is free. With no snapshot yet, it waits for the lock on
a blocking thread. It used to wait, and the gateway's readiness probe (3 s
through the tunnel) and the route plan's probe (2 s) expired on a busy peer -
the peer dropped out of the plan ("Gap at layer 0") and the tunnel route was
erased. The internal routes of
that listener require the `connection_key` and `x-pom-internal-node: 1` on
private networks; on public networks, HTTPS and QUIC can use the secure
transport and the marker, while the HTTP fallback carries the same shared
`connection_key` with the marker. They do not accept client API keys. That
recheck does not sign the inference frames. The Ed25519 public key and
signature are used separately in the handshake to prove the peer's identity.

## 1. The router and the route groups

`start_gateway_with_ready` registers, in this order (webapp is deliberately
last):

- **Base** (`gateway.rs`): `POST /api/generate` (+ alias `POST /generate`),
  `GET /peers`, `GET /health`, `POST /handshake` (`session::handle_handshake`),
  `POST /v1/relay-result` (`direct::RELAY_RESULT_PATH`, 1 MB body limit),
  `POST /v1/remote-stream-result` (`direct::REMOTE_STREAM_RESULT_PATH`, the
  HTTPS fallback callback of the remote transport) and `POST /v1/link`
  (persistent full-duplex channel of the remote return, with no outer body
  limit).
- **Federated public directory** (`gateway/http.rs`): `GET` and `POST
  /api/public/directory/records` are public metadata ingresses. GET delivers
  signed records and POST only accepts records with a compatible signature,
  proof of work, chain of trust and principal. The local cache is bounded and
  expiring; none of these routes carries bootstrap data, addresses or private
  network credentials: the record names the network by the publisher's key and
  the relays by `relay_hints` (`pk:` keys).
- **Public networks** (`gateway/http.rs`, no authentication): `GET
  /api/public/network` (the local network, when public) and `GET
  /api/public/networks` (local, directory and pkarr candidates) return
  `PublicNetworkView` with `publisher_key` and `relay_hints`, never an IP. `GET
  /api/public/networks/locate?key=pk:...` resolves the key through pkarr on the
  node and answers `{reachability: "direct"|"relayed"|"unresolved", relays}`; it
  only accepts keys the node already lists (404 otherwise) and caches the answer
  for 30 s.
- **OpenAI** (`openai.rs`): `GET /v1/models`, `GET /v1/models/:id`,
  `POST /v1/chat/completions` + OPTIONS, `POST /v1/completions` + OPTIONS.
- **Typed decisions** (`decisions.rs`): `POST /v1/systemone`, alias
  `POST /v1/decisions`, the Jev API shape. Body `{model?, state, questions}`,
  where each question is `{type: noul|choice|score, instructions, criteria}`;
  the answer carries one typed answer per question id plus
  `x_pom.questions.<id>` (`label_mass`, tokens, `cached_tokens`, duration).
  An unknown `model` name (such as `jev-latest`) falls back to the active
  model. Same API key authentication as the rest of `/v1`. See
  `inference.md` 3.9.
- **Anthropic** (`messages.rs`): `POST /v1/messages` + OPTIONS,
  `POST /v1/messages/count_tokens` + OPTIONS.
- **Dashboard** (`webui.rs`): the `/api/ui/*` plane (see below).
- **Deployment** (`deploy/`): `/api/ui/deploy/*` + `POST /api/ui/undeploy`.
- **Transfer** (`transfer.rs`): `/api/transfer/*` (see `file-transfer.md`).
- **Dev update** (`devupdate.rs`): `POST /api/ui/dev/update`,
  `POST /api/ui/dev/restart` (gated by `POM_DEV_UPDATE`).
- **Operator** (`admin.rs`): `/admin/*`.
- **API key sync** (`admin.rs`): the principal node exposes
  `/api/internal/api-keys/{upsert,revoke,revoke-origin,validate}` only for
  node-to-node calls authenticated by the `connection_key` and the internal
  marker. The central registry keeps `origin_node_id`; create and revoke
  notifications are idempotent and have three attempts with a bounded timeout.
- **UI** (`webapp.rs`): `GET /` redirects to `/admin-ui/`, which serves the
  bundle from disk (`config::ui_dist_dir()`); assets and SPA routes live under
  that mount. The API fallback stays a JSON 404, and a miss with an extension
  becomes "asset not found".

The registration of the UI control routes is organized in
`crates/node/src/backend/webui/routes/`: `common.rs` for generic routes,
`infer.rs` for inference profile actions, `node.rs` for machine and model
operations, and `admin.rs` for administration and lifecycle. `mod.rs` composes
them; profile authorization stays in the gateway's central middleware, so the
file organization is not a security barrier.

The same `webui/routes/mod.rs` holds the `ROUTE_RULES` data table, which is the
source of the scope the gateway enforces:

- `admin-only`: administration, logs, development and lifecycle (deploy,
  undeploy, apply, launch, release, the plugin development rebuild);
- `node+admin`: models, machine preparation, slicing, provider, transfer and
  the **read** of the network snapshot (`GET /admin/nodes`), which is what the
  Network screen shows;
- `infer+node+admin`: generation, the `/v1` API, chat history, the infer to
  node entry request, the API keys (`/admin/api-keys`, the credential of whoever
  serves `/v1`, and every profile serves), correcting this machine's own
  advertised address (`POST /api/ui/network/advertise-addr`), leaving/rejoining
  the network (`/api/ui/leave`, `/api/ui/rejoin`) and the node's local plugins
  (`/api/ui/plugins*`: activating licenses, GitHub repositories, installing,
  updating and the plugin UIs);
- `common/identity internal`: the node's own catalog, shared views, telemetry
  and the node-to-node protocol.

A rule can name the **method**, and the most specific one wins: first the
longest prefix, and on a tie the one that names the method. That is what keeps
`GET /admin/nodes` open to `node` while blocking, unblocking, removing and
promoting a machine - POST and DELETE under the same prefix - stay
`admin-only`. Without a declared method, the rule applies to all.

The advertised address is a description of the process itself: the route takes
no `node_id` and does not forward calls to another peer. That is why `infer` can
correct it without getting administrative authority. The administration
endpoints that take a target node stay reserved by the target rule and use
Axum 0.7's `:id` syntax.

The UI fallback is only applied after the concrete routes were tried and after
the surface authorization. So an unknown POST or DELETE from an already
authorized caller returns 404, and not 405 for matching a wildcard registered
only for GET; profiles without scope are still refused before the fallback.

A decision the table does **not** express, because it depends on the body and
not the path, stays in the handler: `POST /api/ui/leave` without a `node_id`, or
with its own, is giving up one's own post, and any profile does it; with
**another** machine's `node_id` the request is forwarded, and then it requires
effective administrative authority. The forward travels with the internal
credential, which the target does not judge by the requester's profile, so
without that lock the route's wider scope would turn "I can leave" into "I can
kick anyone out". As always, `profile: admin` alone is not enough: the signed
grant decides (`admin_trust::effective_authority`).

The middleware reads the `profile` persisted in `pom.json` and refuses a
browser outside the scope with HTTP 403 and JSON naming the route and the
profile. A node-to-node call that proves `InternalNodeAuth` can cross the
receiver's local profile for the control fan-out; the handler still requires
the administrative signature when the operation changes authority. On the
`common/identity internal` routes, calls between private nodes require the
right `connection_key` and the internal marker validated by the handler, never
the marker alone. On public networks, control uses `x-pom-control-signature`:
an Ed25519 envelope covers method, path, nonce, epoch, body hash and the issue
time (`issued_at`), and the sender must be a current authority in the chain.
The receiver refuses an envelope outside `CONTROL_WINDOW_SECS` (5 min, on
either side of its own clock) and records the nonce and command id of every
signed POST/PUT/DELETE in the local replay ledger, which forgets the entry when
the envelope's window closes. Nodes whose clocks differ by more than 5 min make
signed control fail. Inference frames stay outside
that signature and depend on TLS transport. `/api/setup/hardware`,
`/api/setup/models`, `/api/setup/admin-anchor/preview`,
`/api/setup/trust-anchor`, `/api/setup/health`,
`/api/setup/auto-discovery/status`, `/api/setup/auto-discovery/start`,
`/api/setup/auto-discovery/search` and `/api/setup/save` only exist in the
first access router; `/api/setup/save` installs the root and `pom.json` before
restarting. The routes of port 8181 (`/v1/prefill`, `/v1/step`,
`/v1/step-stream`, `/v1/link`, `/v1/session/*`, `/v1/tp/reduce`, `/health`) are
explicitly out of this policy's scope.

The authority screen uses `GET /api/ui/network/admin-authority` for the
presentation contract of the principal, secondary admins, the promotion chain
and succession eligibility. The screen's isolated mutations are
`POST /admin/nodes/:id/promote-admin`, `POST /admin/authority/resign` and
`POST /admin/authority/secondaries/:id/demote`; the principal is the only
origin allowed to promote, resign and demote. The infer to node request is
`POST /api/ui/network/request-node` and the voluntary step down from node to
infer is `POST /api/ui/network/relinquish-node`. The authority offer states
share `GET /api/transfer/offers` and its answers
`POST /api/transfer/offers/{id}/accept|reject`, so the UI uses the same bell as
slice offers. For a succession offer, `accept` requires the successor's signed
acceptance; `reject` is refused because the offer only ends with a signed
acceptance or an epoch advance. These endpoints are presentation contracts and
do not define the key, signature or storage of grants.

## 2.1 Administrative authority

`connection_key` is a transport credential between private nodes and a local
credential of the administrative surface. On a public network, it is neither an
admission secret nor shared. Admin capability comes from the separate file
`config::admin_trust_path()`, protected with mode 0600 and containing the manual
anchor, the root grant, the chain of secondary grants, demotions and
succession. Consumed nonces are **not** kept in that document, which is shared
and replaced wholesale by `reconcile`: they live in the local replay ledger next
to it (`<chain>.replay.json`, also 0600), state of a single node.
`profile: admin` alone creates no capability:
without a valid chain the node stays without access to the `admin-only` routes
and emits a diagnostic at startup.

`GET /api/ui/network/admin-authority` only exposes the presentation view. The
principal promotes with `POST /admin/nodes/:id/promote-admin`; the delivery
carries the full signed chain to the target. `POST /admin/authority/resign`
opens a signed offer to the last active secondary, and
`POST /api/transfer/offers/{id}/accept` only completes when the successor
presents the signed acceptance. `POST /admin/authority/secondaries/:id/demote`
is exclusive to the principal. The infer to node path is a signed request
authorized by any admin, and node to infer is a local decision with a warning.

The promotion request is a two-leg node-to-node transaction. The `infer` sends
the signed request to `/api/internal/profile/authorize-node`; the admin
authorizes through the Ed25519 chain and sends the signed command to
`/api/internal/profile/accept-node`. Each leg tries the
`PeerInfo::gateway_candidates()` in the announced order, so `gateway_url()` is
only the first attempt and not the peer's permanent truth. An HTTP answer is
kept as an explicit refusal; only a transport failure moves to the next
address. The forward deadline is longer than one attempt of the return, since
the admin's answer must contain the second call. The same signed mutation is
not resent automatically after a timeout, so replay protection does not reject
it as if it were a new request.

Inference frames get no per-frame administrative signature; the peer is
authenticated by the libp2p transport and control stays separate.

### Anchoring the root from the UI

`pom admin-anchor` is still the operator's gesture on the machine. The UI
mirrors both sides of it at `GET/POST /api/ui/network/trust-anchor`
(`crates/node/src/backend/webui/trust.rs`).

The `GET` shows what the operator must check: `anchored`, the anchored key's
`fingerprint`, `cluster_id`, `anchor_node_id`, `is_anchor` (the root is this
node's key), `loopback` and `can_install`. All of it is public - the
fingerprint exists to be carried and checked, and knowing it grants nothing.

The `POST` receives `{ document, fingerprint }`: the pasted content of the trust
file and the fingerprint **typed by the operator**. The refusals, all on the
server, none of them bypassable by the browser:

| `reason` | HTTP | when |
|---|---|---|
| `not_loopback` | 403 | the connection did not come from loopback |
| `already_anchored` | 409 | the node already has a root, or there is already a file on disk |
| `invalid_document` | 400 | the pasted content is not a valid signed chain |
| `fingerprint_mismatch` | 400 | the typed value is not the pasted anchor's |
| `install_failed` | 409 | writing failed; nothing was changed |

Loopback is measured on the socket's real address (`serve::PeerAddr`), **not**
on `admin_exposure`, which may be deliberately open to the network, and never
on `X-Forwarded-For` or the like, which the client writes. An already anchored
node refuses because replacing the root is a manual re-anchoring
(`pom admin-anchor --replace`), not a UI gesture. And the file does not
validate itself: the signature only proves the chain's internal consistency;
what validates it is the value the operator brought through their own channel,
which is why it is never prefilled from the pasted content.

The route is `CommonOrInternal` in the scope table, on purpose: a `profile:
admin` **not yet anchored** does not pass `AdminOnly`, which is exactly the
state in which it needs to see the fingerprint, and whoever receives the anchor
is usually a `node` or an `infer`. It also does not require the
`connection_key`: the boundary is still `AdminAuth` (loopback in local mode,
human password in network mode), and adding that credential does not protect
against the threat the loopback lock contains - whoever reaches the browser
reaches the same origin's `localStorage` - while it would leave the screen dead
in a browser that has not received it yet.

### Exposure boundary

The exposure mode in Settings / Preferences only applies to the administrative
surface: `/api/ui/**`, `/admin/**` and the static mount `/admin-ui/**`
(including the root and old assets). In `localhost`, the server refuses that
surface when the connection came from outside loopback and returns a
standalone HTML page, with no bundle and no navigation, with status 403 and the
messages in English and Portuguese. In `network`, the static login screen is
public, but the administrative data requires `X-POM-Access-Token` on the first
call; the server then issues an in-memory session identified by an `HttpOnly`,
`SameSite=Strict` cookie, valid for 8 hours and only on administrative paths.
The cookie name carries the `Host` port (`<slug>_admin_session_<port>`,
`auth::admin_session_cookie_name`): cookies do not separate by port, and two
nodes on the same host (`<host>:8080` and `<host>:8081`) logged each other out on
every login. The session digest is invalidated when the password changes or
when going back to localhost. The password is compared on the server with the
persisted Argon2id hash, which is never returned to the UI, and is never
replaced by the `connection_key`. On a public network, control calls between
nodes only use the signed envelope; on a private network they also use the
internal marker together with the `connection_key`, without opening that path
to the browser.

Every administrative refusal names itself in the `x-pom-admin-error` header:
`missing-token` (401, there was no credential at all), `invalid-token` (403,
there was one and it does not match), `no-peer-address` (403) and `local-only`
(403, the standalone page). The body's sentence is for the person and changes
with the language; this value is stable and is what the UI uses. The session
only lives in the node's memory, so a restart or the end of the 8 hours drops it
with the tab open: on seeing `missing-token`/`invalid-token` on an
administrative route, the UI discards the dead credential and redoes the
surface probe, going back to the access gate.
Before that, each screen printed the server's raw 401 in place of its content,
with no way back other than reloading by hand.

The pipeline routes are not part of that boundary: `/api/generate`,
`/generate`, `/v1/**`, `/health`, `/peers` and `/handshake`, plus the inference
server routes on `inference_port`, stay reachable over IPv4 and IPv6 so the
libp2p tunnel's HTTP fallback is not interrupted. `GET /api/access` is the only
public exception of administration, since it only reports the mode, whether a
password is needed, the port, `boot_id` (random per run of the process, which
is how the interface repeats a notice once per start) and `default_password` - the password in effect
is still the first access default (`admin`, `auth::DEFAULT_ADMIN_PASSWORD`). It
is a flag written in `pom.json` (`admin_password_default`) next to the hash, so
this public route never runs Argon2; `POST /api/setup/save` sets it (the UI
leaves setup in `network` mode with that password) and `POST /api/ui/access`
updates it on every password change.

In **setup mode** (no `pom.json`), `main.rs` mounts a smaller router: the routes
`/api/setup/hardware`, `/api/setup/models`, `/api/setup/admin-anchor/preview`,
`/api/setup/trust-anchor`, `/api/setup/health`, `/api/setup/save` and the UI.
They are not in the main router and only exist before the initial
configuration.

## 2. Routes by group

### Base

- **`POST /api/generate`** - `{ model?, prompt, max_tokens?, stop? }`; answer
  `GenerateResponse` with `response`, `source` (`"pipeline:<chain>"`,
  `"local"`, `"peer:<node_id>"` or `"none"`/`"error"`), `tokens_generated`,
  `tokens_per_sec`, `duration_ms`, `timings`, `trace`. `timings` aggregates the
  whole pipeline and separates `pipeline_first_token_ms`/`server_ttft_ms`
  (queue + prefill up to the first token), `client_ttft_ms` when the client
  measures it, and `ttft_ms` for compatibility,
  `prefill_ms`/`prefill_tokens_per_sec` (tokens actually processed, after
  prefix reuse), `generation_ms`/`generation_tokens_per_sec` (output since the
  first token) and `total_ms`/`total_tokens_per_sec`. A rate without tokens or
  without a measurable interval is `null`; `tokens_per_sec` stays an alias of
  the total so clients do not break. The body is accepted as raw bytes (it does
  not require `Content-Type: application/json`). `max_tokens` is the total
  output budget, including the first token the prefill returns; without the
  field, the default is 512. The request must be greater than zero. The
  effective capacity comes from the `n_ctx` confirmed by the runtime and the
  smallest value among the live peers; the deployment's `max_new_tokens` only
  limits one leg of the continuation loop. If the prompt fills the whole
  context, the route answers 413 before opening the SSE. The route only accepts
  `Authorization: Bearer <active API key>` or `x-api-key: <active API key>`;
  the `connection_key`, a missing token and an anonymous caller are refused
  before the pipeline. `trace` also exposes, per request,
  `mtp_draft_tokens_proposed`, `mtp_draft_tokens_accepted`,
  `mtp_draft_tokens_rejected` and `mtp_acceptance_rate` (`accepted /
  proposed`, or `null` with no MTP proposal). `proposed` only counts the draft
  that reached the target's verification.
- **Catalog during warm-up** - `GET /api/ui/models` can publish
  `metadata_complete: false` while enumeration and enrichment run in the
  background. If a selection route receives a path that has not appeared in
  that partial inventory yet, it reports that discovery is in progress; it
  does not treat the file as absent from the peer. The client should wait for
  the next answer with `metadata_complete: true` and repeat the selection.
- **`GET /peers`** - `{ peers: [PeerSummary], count }`. `PeerSummary` carries
  `node_id` (first 12 chars), `addr`, `model`, `layers`, `ollama_url`,
  `inference_url`, `load` (in-flight load).
- **`GET /health`** - `status`, `ollama_available`, `peer_count`,
  `pipeline_peers` (with `inference_port`), `default_model` and
  `runtime_capacity` (`n_ctx`, `max_new_tokens`) when the local runtime has
  already answered `model_loaded`. `ollama_available` uses the last cached
  value and schedules an optional probe of at most 500 ms at most every 15 s,
  so a missing Ollama or the cold catalog enrichment does not block the
  gateway's answer. On the inference port, the answer also publishes
  `model_label`, `deploy_id`, `start_layer`, `end_layer` and `total_layers`,
  plus `n_ubatch_effective`, `mtp_ready` and `mtp_unavailable_reason`; the last
  one explains why the requested MTP context was not attached. `load_params`
  carries the llama.cpp parameters the context started with - `n_ctx`,
  `n_ctx_seq`, `n_batch`, `n_ubatch`, `n_seq_max`, `n_threads`,
  `n_threads_batch`, `flash_attn`, `flash_attn_auto`, `kv_unified`,
  `embeddings`, `n_rs_seq`, `offload_kqv`, `cache_type_k`, `cache_type_v`,
  `n_gpu_layers` and `fit_memory_applied` -, read from the live context and not
  from `EngineConfig`. With `flash_attn: null` the diagnostic bridge did not
  answer, and the UI says it does not know instead of showing "off".
- **`POST /handshake`** - a two-message challenge protocol. The first request
  carries `{ node_id, pubkey, connection_key }` without `nonce` or `signature`;
  it is refused with 403 `challenge_required` and returns a random 32-byte hex
  nonce. The second repeats the fields and adds `{ nonce, signature }`, where
  `signature` is the hex Ed25519 signature of the raw nonce by the private key
  matching `pubkey`. The nonce is valid for 30 s and consumed only once. Only
  then does the gateway answer 200 with `approved` + `peer_id`. `invalid_key`,
  `identity_mismatch` (the `node_id` is not the `SHA256` of `pubkey`, or the key
  is malformed), `blocked`, `missing_nonce`, `missing_signature`,
  `invalid_signature`, `nonce_reused` and `nonce_expired` stay 403 refusals. An
  old node, which sends the earlier body and cannot repeat with the signature,
  sees 403 and is never admitted. This route also arrives through the libp2p
  tunnel, together with `/health`, the
  `/api/cluster/{tls,quic,logs,network-chat,network-settings}` routes and the
  direct reply's return (`/v1/relay-result`, `/v1/remote-stream-result`, each
  authenticated by the frame's single-use ticket): that is the closed list of
  `infer_tunnel::gateway_path_is_allowed`, which a peer behind NAT/CGNAT uses
  when no direct address answers. The list is checked on the receiver and the
  comparison is exact, because what comes out of the tunnel reaches the gateway
  through `127.0.0.1`, and the loopback boundary is what allows installing the
  administrative anchor. See `docs/subsystems/p2p-network.md`, "NAT
  traversal".
- **`POST /v1/relay-result`** - where the **tail** of a pipeline delivers the
  direct result (`relay/v2-direct`). It does not use a client API key: it
  requires a live ticket in the body; a dead ticket = `410 Gone`.
- **`POST /v1/remote-stream-result`** - the remote head's HTTPS fallback
  callback for each opportunistic batch. It does not use a client API key: it
  requires a live ticket. The ticket is consumed by an `mpsc` bounded at 64
  batches; when the queue is full, the handler waits and applies backpressure.
  A cancelled ticket or a terminal batch ends the stream; an unknown ticket
  answers `410 Gone`. Besides `accepted`, `done`, timings and `node_timings`,
  the batch can carry `next_draft`, `speculated`, `speculation_active` and
  `speculation_disabled_reason`, trace identity and aggregated frame metrics
  (`stream_id`, index, lifetime, first delivery, latency and bytes). The
  gateway's trace summarizes those metrics as min/p50/p95/p99/max for frame
  latency, min/p50/p95/p99/max for remote compute and total/p50/p95 for wait.
  The draft travels in that same callback, but the remote head installs it
  internally in the next `step`; the gateway makes no reinjection call. Old
  peers without those fields stay valid.
- **`POST /v1/link` on the gateway** - the persistent full-duplex return of
  `remote-stream`. The body is a stream of envelopes in the same format as the
  inference channel, each frame carries `REMOTE_STREAM_RESULT_PATH` and the item
  holds the ticket. The handler rechecks the internal credential before
  dispatching each frame, delivers it to `ReplyInbox` and returns the ACK on the
  channel itself. A failure of that transport is marked as HTTPS fallback in the
  next batch.
- **`GET /api/cluster/tls`** - `{ tls_required }`. It is what the other nodes'
  ratchet queries on the principal. It requires `InternalNodeAuth`, that is,
  the connection key and `x-pom-internal-node: 1`; that stops an intruder from
  answering for the principal and downgrading the cluster to clear text. See
  `p2p-network.md`, section 5.
- **`GET /api/cluster/quic`** - `{ quic_enabled }`. The authenticated source of
  the HTTP/3 preference queried by peers when the Announce's `quic` field
  triggers the ratchet. It does not accept the announcement as policy and
  requires the same shared `connection_key`.

### OpenAI (`/v1/*`)

A pure protocol layer over `gateway::generate_stream_with_stops`. The client
inference routes require an active API key: `Authorization: Bearer <key>` or
`x-api-key: <key>`. The headers are independent candidates: when both arrive,
one valid credential authenticates even if the other is invalid; two invalid
ones are still HTTP 403. The `connection_key` identifies the node-to-node
channel, but does not authenticate client inference. An active API key keeps
its `Caller` in the audit log, and a key unknown locally is validated on the
principal over the authenticated internal channel; timeout, error or an invalid
answer fail closed before dispatch. This mapping follows the
[Claude API authentication](https://platform.claude.com/docs/en/manage-claude/authentication):
`ANTHROPIC_API_KEY` uses `x-api-key` and a Bearer token uses `Authorization`.
`render_prompt` flattens the conversation into a single text and **labels every
turn, including the first** - that is what makes one turn's prompt an exact
prefix of the next one's, without which the prefix cache would never hit on the
1->2 transition. Raw unlabeled text is what `/v1/completions` and
`POST /api/generate` deliver. `usage.prompt_tokens_details.cached_tokens`
reports how many prompt tokens the pipeline did not reprocess; it measures what
ran, not the cache choice (see `inference.md`, section 3.1).
`DEFAULT_MAX_TOKENS` 512 is only the default when the client sends no budget.
There is no artificial ceiling in the adapter: the request's **total** budget
is the smaller of the client's request and the `n_ctx` space left after the
prompt. The deployment's `max_new_tokens` does **not** enter that math: it is
the ceiling of one **leg** of the continuation loop, which the gateway crosses
without closing the SSE (see `inference.md`, section 9.1). If the total is
reduced, the gateway records the reduction and returns it in
`x_pom.output_limits` (`requested_max_tokens`, `effective_max_tokens`,
`deployment_max_tokens`, `context_available_tokens`, `leg_max_tokens`); if the
context leaves no token at all, it answers HTTP 413 before opening the SSE. The
capacity published in `/v1/models` is derived from the effective `n_ctx`, never
from a hidden absolute ceiling. `x_pom.continuation_legs` says how many legs the
answer stitched - `1` is a single generation. So an output that exhausts the
**total** budget gets `finish_reason: "length"`, even if it contains an already
recognized tool call, and the audit records that ending as `incomplete`
(`partial` is reserved for a transport failure with saved text). An exhausted
leg never produces `length`: the client has no way to tell the stitch apart,
and it was exactly that intermediate `length` that made a harness drop a tool
call halfway. The total, on the other hand, is **inviolable**: nothing goes
past it, not even a tool call open at the cut token. When the cut falls inside
a call, it comes back **structured and incomplete** -
`tool_calls[].function.arguments` with the unfinished JSON string in OpenAI, a
final `tool_use` block with incomplete `input` in Anthropic -, and
`x_pom.truncated_tool_call` is `true`, together with `x_pom.partial`. That is
both providers' shape: the client reads the terminal stop reason and repeats
the request with a higher ceiling, without executing anything. The shared
scanner recognizes the markers declared in `CALL_FORMATS`, keeps text before
the call, turns every complete call into `tool_calls` and, in SSE, into
indexed deltas. An incomplete marker the **model** left half-done comes back as
literal content so it does not disappear; one the **ceiling** cut comes back
structured, and that is the difference between `TagScanner::finish` and
`finish_truncated`. Every valid answer carries the `x_pom` extension (outside
the schema) with the proof of distributed execution and its own `timings`
object; the OpenAI `stop` field (string or list) goes through the gateway's
incremental matcher: a suffix that may still be the prefix of a stop is held
back and the stop itself is never sent to the client. CORS `*` for browser
harnesses.

The request's `model` field is validated, not just read:
`POST /v1/chat/completions` and `POST /v1/completions` refuse with HTTP 404
(`error.type: "invalid_request_error"`, `error.code: 404`) a name no peer
announces in `GET /v1/models`; `POST /v1/messages` refuses the same case with
HTTP 404 (`error.type: "not_found_error"`, in Anthropic's error format). None
of the three routes silently serves the active deployment for an unknown name.
An explicit `<model>@<deploy_id>` selector that has not appeared in anyone's
announcement yet is still accepted - refusing here would swap "this deployment
did not answer" for "I answered from another one", worse for whoever asked for
that specific configuration; the failure in that case comes later, in the
pipeline planner, and names the selector. Omitting `model` still uses this
gateway's active deployment (`GET /health` -> `default_model`), the only
implicit fallback left. `POST /api/generate` never rewrites the requested name:
a name with no matching peer fails in the planner with "Nenhum pipeline válido
para o modelo 'X'" ("no valid pipeline for model 'X'") instead of any
substitution. When more than one model is loaded in the cluster,
`GET /v1/models` is the source of truth for the servable names - including the
`<model>@<deploy_id>` variant to choose between deployments of the same model
- and a well-configured client chooses among them by sending the exact name.

`GET /v1/models` and `GET /v1/models/{id}` only announce models published by
live peers, including the local model when there is an active assignment. The
CLI's `default_model` and the persisted deployment registry do not enter this
autodetection, so an unload does not leave a ghost model in the catalog. The
list carries Anthropic's pagination envelope (`first_id`, `last_id`,
`has_more`) and fields compatible with both clients: `type: "model"`,
`created_at`, `display_name`, `capabilities`, `max_input_tokens`,
`max_tokens`, plus `context_length` and `top_provider.max_completion_tokens`
for OpenAI. The `created_at` field uses the RFC 3339 epoch when the local
release date is unknown.

The limits come from the effective runtime and the network: `context_length`,
`max_input_tokens`, `max_tokens` and `top_provider.max_completion_tokens` are
the smallest `n_ctx` among the peers of the planned pipeline that serve the
model/deployment, the capacity of the **request**, not the ceiling of a leg.
Announcing the deployment's `max_new_tokens` here would make the client ask
for less than the node delivers. The same peer minimum is used by each
request's validation. A peer only enters that capacity after its `/health`
confirms the model is loaded; during a restart, the previous announcement is
withdrawn. An idle peer chosen by the plan is woken by `reload_model` before
generation and the request waits for its `/health`; the planned idle peers are
woken in parallel, with a 15-minute ceiling for the barrier.
`POST /api/internal/reload_model` answers at once with `state: "loading"` or
`"ready"` (503 with `error` when the load cannot happen) and the gateway
repeats it until every peer is ready. A live peer on an
alternative route blocks neither the catalog nor admission. A planned non-idle
peer without that confirmation keeps the model pending, instead of falling back
to the local capacity and hiding an old stage.

### Anthropic (`/v1/messages`)

Same generation path, its own `render_prompt` (separate
system/messages/tools), `content_blocks` for `tool_use`/`tool_result`,
`stop_reason` (`end_turn`/`max_tokens`/`tool_use`/`stop_sequence`), SSE with
Anthropic's event names. It also labels every turn, for the same reason as
OpenAI, and publishes the honored reuse in `usage.cache_read_input_tokens`.
`stop_sequences` uses the same incremental matcher and `stop_sequence` returns
the string that ended the output. `tool_choice` is still parsed and ignored.
`x_pom.output_limits` reports the requested budget, the effective one and one
leg's; `x_pom.continuation_legs`, how many legs were stitched; `x_pom.timings`
uses the same convention as OpenAI; an output cut by the **total** budget uses
`stop_reason: "max_tokens"`, and an exhausted leg never does.

For Claude Code, `ANTHROPIC_BASE_URL` must point to the gateway's origin, for
example `http://<gateway-host>:8080`; the SDK appends `/v1/messages`. The
`ANTHROPIC_API_KEY` variable is sent as `x-api-key`; `ANTHROPIC_AUTH_TOKEN` is
sent as `Authorization: Bearer`. The value must be an active API key from
`pom.db`, never a `connection_key` or an external Anthropic key. If the client
sends both headers, the gateway considers both independently, without logging
any secret.

#### Incremental argument streaming

A tool call's arguments go out **in fragments, as the model generates them**.
Measured on a CUDA server before this: 663 characters in a single chunk, with the first
and last frames both at 11.01 s - the tag reader accumulated the whole body and
only flushed when the call closed, so the user stared at a frozen screen and
got the file all at once.

`tool_call::CallArgsStreamer` transcodes the model's markup into JSON as it
arrives. **The concatenation of the fragments is always exactly the arguments
string the non-streaming path would produce** - even when the ceiling cuts the
call, in which case it is the unfinished string of section 9.1 of
`inference.md`. What was already sent is never repeated on close: a normal
partial fragment and a terminal truncation do not get mixed up in the output.

The setting is `eager_input_streaming`, an **optional boolean in the tool
definition**, with no beta header - Anthropic's shape, checked in the live
documentation on 2026-08-29
(`platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming`):

| where | behavior |
|---|---|
| `/v1/messages`, `eager_input_streaming: true` on the tool | fragments as the model generates |
| `/v1/messages`, absent or `false` | held back **per parameter value** (their default): the parameter goes out whole when it closes |
| `/v1/chat/completions` | always in fragments - OpenAI has no switch for this, and inventing a field would extend its schema |

**The argument's type is never lost.** POM's markup carries no type -
`<parameter=limit>5</parameter>` only becomes a number because
`decode_parameter_value` tries to parse the full value. That is why eager mode
only streams a value that **provably cannot be JSON**: above
`EAGER_MIN_VALUE_BYTES` (64) and starting with a character that cannot open
valid JSON (`{`, `[`, `"`, `-`, `+` and digits are held back; a letter passes,
because `true`/`false`/`null` are short literals). A file that starts with `{`
is held back - it costs latency in a rare case, and is the price of never
delivering an array turned into text.

The invariant that holds it all up is **monotonicity**: the streamable prefix
only grows, never shrinks or diverges, because there is no unsending an SSE
fragment. From it come trimming whitespace at the end of the value, holding
back what may still become `</parameter>`, and the key order on close (document
order, not `serde_json::Map`'s alphabetical order). `CallArgsStreamer` also
checks this at runtime and suppresses a diverging fragment instead of
corrupting the client's JSON.

**Declared limit:** the Hermes form (`<tool_call>{"name":...}`) is not
transcoded live - the name lives inside the very JSON that would be streamed -,
so it goes out whole on close, as before.

#### Task budget

`output_config.task_budget` is the mechanism the providers offer **instead of
overflowing** the ceiling: instead of letting generation go past `max_tokens`,
it tells the model the budget so it organizes itself and finishes gracefully
within the limit. The shape is Anthropic's, checked in the live documentation
on 2026-08-29
(`platform.claude.com/docs/en/build-with-claude/task-budgets`):

```json
{
  "output_config": {
    "task_budget": {"type": "tokens", "total": 64000, "remaining": 42000}
  }
}
```

- `type` is always `"tokens"`; any other value is HTTP 400.
- `total` is the budget of the whole agentic loop. The minimum is
  `TASK_BUDGET_MIN_TOTAL = 20000`, the same as the providers', and below it the
  answer is HTTP 400 - a budget too small for the task makes the model refuse or
  stop early.
- `remaining` is optional and equals `total` when absent: it is what is left
  from a previous request.
- `output_config` fields this node does not implement (`effort`, `format`) are
  accepted and ignored; refusing a field the client sends for another model
  protects nobody.

It is **advisory and never cuts**. What cuts is `max_tokens`, an enforced and
inviolable ceiling - a `total` of a million does not extend the ceiling by one
token, and the test bench asserts that. The server injects into the generation
a countdown marker only the model sees (`messages::task_budget_marker`), at the
end of the rendered prompt, right before the model starts writing. The marker
never appears in the answer.

Since the marker is part of the prompt, the Anthropic path includes `type`,
`total` and `remaining` in the prefix cache key. Changing any of these values
invalidates the previous KV. The marker is final and injected on every request,
so it does not match between turns, as in Anthropic's documented semantics.

**The marker's form is this node's.** Anthropic documents that it injects a
server-side marker and that it is visible only to the model; the literal text
is not published, so there is nothing to mirror. What is mirrored is the
contract.

**Declared limit.** There, the marker crosses the whole agentic loop and
updates itself every turn, counting what the model sees (thinking, tool calls,
results, output) and not the resent history. Here it is injected **once per
request**, with the `remaining` the request brought: the node keeps no state
between requests, so the client stitches the loop - exactly the case Anthropic
documents for compaction, where `remaining` is passed along on each request.
`x_pom.task_budget` returns `{total, remaining_at_turn_start}` and
`x_pom.task_budget_spent_this_turn` says what the turn spent, for the client to
decrement. None of this goes into `usage`: Anthropic does not return the
remaining budget there, and copying its schema includes what it does **not**
send.

**Not verified:** whether a local model reacts to the marker. The bench proves
it reaches generation; whether the served model knows how to use it is a
cluster measurement.

### `/admin/*`

`GET /admin/nodes` (includes blocked but disconnected nodes, so the panel is
not empty), `POST /admin/nodes/:id/block`, `POST /admin/nodes/:id/unblock`,
`POST /admin/nodes/:id/promote-admin`, `POST /admin/authority/resign`,
`POST /admin/authority/secondaries/:id/demote`, `DELETE /admin/nodes/:id`,
`POST /admin/regenerate-key`, `GET+POST /admin/api-keys`,
`DELETE /admin/api-keys/{key}`, `GET /admin/transfer-offers` (the same
offers as `GET /api/transfer/offers`). `list_nodes` adds entries for blocked nodes outside the network.

`POST /admin/nodes/:id/promote-admin` is only accepted by the node whose
effective authority in the separate file is `Primary`. The principal signs a
`secondary` grant with its Ed25519 identity; the target only accepts the change
after validating the manual anchor and the full chain, while it is still a
`node`. `GET /api/ui/network/admin-authority` is the view of the chain and the
members. Resigning is a signed offer to the last active secondary and only the
acceptance signed by that successor's key changes the principal. A secondary
can neither promote nor demote another.

`DELETE /admin/api-keys/:key` deactivates the key in `pom.db` without deleting
the row and appends a revocation with the authenticated operator's `node_id`
and the time. A second attempt is idempotent and creates no other event; the
key never authenticates again.

#### KV cache snapshots (`slot_save_path`)

`GET /admin/slots` returns `{slot_persist_enabled, slot_count}` read from the
**local** inference port's `/health` (a quick view of this node), plus
`prefix_auto_cache: {enabled, index_entries, index_bytes}` when the persistent
prefix cache (`slot_persist_prefix_cache`) is on in this gateway.

`POST /api/ui/kv-cache/clear` (AdminOnly) empties the KV cache of **one**
machine. The body is `{node_id?}`: absent or the node's own id acts locally,
another id is forwarded to that machine, since only the owner of a disk deletes
from it. Locally it removes every `.bin` and `.json` of the snapshot directory,
resets this gateway's automatic prefix index and calls
`POST /v1/session/cache/clear` on the local inference server. Sessions that are
live in the model context are untouched. The answer is `{cleared, node_id,
disk_files, disk_bytes, ram_entries, ram_bytes}`. `GET /api/ui/node` publishes
the disk side as `kv_cache: {enabled, path, snapshots, bytes}`, measured on each
call, and `/health` of the inference port publishes the RAM side as `cache_ram`
(see `inference.md` 3.1.2).

`POST /admin/slots/:name?action=save|restore|erase` orchestrates a snapshot
across the whole deployment. Body: `{model, session_id?, dest_session_id?}`
(`model` is the label or the `deploy_id`). The gateway resolves the pipeline
with the same planner as generation and calls `POST /v1/session/slot` on each
hop; each peer writes/reads its slice in `slot_save_path` with the name
`"{name}__{node_id}"`. All or nothing: a hop without the
`inference/slot-persist-v1` capability, or a failure on any hop, refuses the
whole operation (hops already written are erased). `save` requires the
`session_id` of a live session (it comes from an answer's `x_pom`); `restore`
creates `dest_session_id` (or a new id) and checks that every hop restored the
same token boundary. See `docs/subsystems/inference.md`.

### `/api/ui/*`

The dashboard's control plane, every mutating route behind `ConnectionAuth`:
`node`, `models`, `models/delete`, `cluster`, `cluster/models`,
`peer/:node_id/runtime`, `paths`, `plan`, `apply`, `launch`, `stop-inference`,
`slice` + `slice/plan`, `jobs` + `jobs/:id`, `unload`, `release` +
`release/candidate`, `leave`, `rejoin`, `requests` and `requests/stats` (each
admin's local persistent audit, respectively `reqlog::list_requests` and
`reqlog::stats_requests`), `network/rtt` (authenticated GET, this node's
directional snapshot), `chat/telemetry` (authenticated POST, receives
`{ node_ids, execution_id? }` and returns transport and observed wire RTT on
the edges of the identified run; without `execution_id`, it also returns every
complete route of the active model with aggregated RTT. When the last run
contains the leg, the wire RTT (`net_rtt_median_ms`) beats the libp2p
directional probe; the gateway's and peers' probes stay as a fallback and cover
a single head with no edge in the graph. The run RTT with remote processing
stays in the trace), `chat/history` (authenticated GET/POST/DELETE for the
durable conversation history), `dev/update` + `dev/restart`,
`network/advertise-addr`, `network/tls`, `thermal`. `leave` accepts an optional
`LeaveRequest { node_id }` and **forwards** to the peer when the id is someone
else's (the panel only talks to its own node).

`GET /api/ui/plugins` is open to every profile and returns the local states of
the plugins already restored or seen by the runtime, including
`licensed_plugins`, `activation_candidates`, `plugin_usage` per `plugin_code`,
`license_expires_at` and that plugin's activation counters. That data comes
from the authenticated validation, activation and heartbeat answers; license
tokens do not appear in the status JSON. `state` describes license validity and
`active` only means the host is installed and ready. An install failure
publishes `installation_error` and keeps `active=false`, without losing the
license distinction. The browser queries the public commercial catalog directly
at `/v1/plugins`, without credentials. Activation validates the key once and
creates the runtimes for every granted `plugin_code`, activating them
separately. Buying redirects licensed plugins to the public store with an
encoded `plugin=<plugin_code>`. The commercial catalog does not register,
authorize or provide GitHub repositories for installation.

GitHub releases have an independent local registry per node. The
`Add from GitHub` card accepts a public URL and looks for `manifest.json` at the
root of the default branch; if it does not exist, it tries `ui/manifest.json`
(`pom-plugin-ui/v1`) and uses the per-platform manifests attached to the
releases. It walks every published page without calling the license server.
Reading has a metadata budget and fails without silently truncating if it is
exceeded. These actions are open to every profile (plugins are local to each
node) and use `ConnectionAuth`:

- `POST /api/ui/plugins/checkout` creates on the license server the Stripe
  checkout session for a sold plugin and returns `{url}` (see
  `enterprise-licensing.md`, "Checkout through the node");
- `GET /api/ui/plugins/github-repositories` lists the persisted sources and
  publishes `manifest_path` (`manifest.json` or `ui/manifest.json`),
  `description` per language and `screenshot_urls` already resolved to
  `raw.githubusercontent.com`;
- `POST /api/ui/plugins/github-repositories` receives `{repository_url}` and
  validates the manifest and releases directly on the public GitHub API;
- `POST /api/ui/plugins/github-repositories/:id/refresh` updates the data and
  keeps the last valid read on failure;
- `DELETE /api/ui/plugins/github-repositories/:id` removes the local record,
  without uninstalling the already installed plugin;
- `POST /api/ui/plugins/github-repositories/:id/install` receives
  `{release_id}` to download and install exactly that chosen release.
- With `POM_DESENV=1`, `GET /api/ui/plugins/dev-sources` only lists known
  repositories and valid mounted sources;
  `POST /api/ui/plugins/dev-sources/:repository/rebuild-install` builds and
  installs or rebuilds the local plugin without requiring an existing
  activation. These two routes stay AdminOnly. The server validates the paths,
  only uses fixed Cargo commands and does not return the build's stdout/stderr.
  A development-only install has no licensed features; rebuilding an existing
  licensed or GitHub install preserves that install's entitlement and never
  activates a license.

The root manifest declares ABI, `plugin_code`, features, optional preferences
and the asset name per platform. For `ui/manifest.json`, the releases attach
`pom-plugin-<os>-<arch>.json` with that metadata. The release manifest and the
binary must publish a SHA-256 digest. Installing is confirmed in the UI, checks
size/hash and neither queries `requires_license` nor grants a license, grant or
lease. The installed state lives in `plugin_public_install:<code>`.
`POST /api/ui/plugins/:code/preferences` receives `{preferences}` and only
persists to `pom.db`, under the plugin's code; it does not call the license
server. `PluginStatus.github_repository`, `public_install`,
`preferences_schema` and `preferences` expose the local state, with no tokens
and no synthetic entitlement.

`POST /api/ui/plugins/:code/rpc/:op` is a plugin's own API for its screens
(`plugin_ui::plugin_rpc`, contract in `plugins-ui.md`). The JSON body (at most
256 KiB) reaches the plugin's generic `query` as operation `rpc.<op>` with
`{params, profile}`, and the plugin's JSON reply is returned unchanged. `op` is
lowercase letters, digits, `.`, `_` and `-` (up to 64). Same-origin only, like
the proxy; 400 for a bad name or body, 413 above the ceiling, 502 when the
plugin is inactive or the operation fails. Open to every profile; the plugin
authorizes by the forwarded `profile`.

`ANY /api/ui/plugins/:code/proxy` and `/api/ui/plugins/:code/proxy/*path` are
the proxy of the web UIs served by the plugins themselves
(`crates/node/src/plugin_proxy.rs`, contract in `plugins-ui.md`). Open to every
profile on every method. They do not use `ConnectionAuth`: the browser's
scripts, WebSockets and `EventSource` do not carry the UI's `Authorization`, so
the credential is the administrative surface's (loopback in `localhost`,
session cookie in `network`), and the route refuses with 403 every request
whose `Origin` is not the node's own or whose `Sec-Fetch-Site` is not
`same-origin`/`none`, plus any call between nodes. The body has no ceiling
(uploads go through the route). The gateway's HTTP server accepts connection
upgrades because of it.

`GET /api/ui/pom-update/status` is a common read with the result of the
`pom_core` release detection (`PomCoreUpdateStatus`). The node only fills that
notice after an accepted `enterprise` plugin heartbeat and while that license
is active; installations without it do not query the core manifest. The
AdminOnly action `POST /api/ui/pom-update/check` forces that detection on
demand and returns the updated status, without downloading or restarting
anything. `POST /api/ui/pom-update/apply` receives `{ "release_id": "..." }`,
checks that it is the release_id still announced, queries and verifies the
manifest again, downloads the artifact and schedules the swap and the restart.
The action is never triggered by the daily detection. See
`enterprise-licensing.md`.

`GET /api/ui/node` includes `pom_desenv`, the decision captured at startup: it
is only true when `POM_DESENV` is exactly `1`. A missing variable, `0` and any
other text leave the flag false. The flag only controls the visibility of the
controls in the browser: the plan, deploy, apply, reconfigure and redeploy
handlers keep the fields of the `Configurações POM (DESENV)` ("POM settings
(DEV)") section received in the request, even for clients that do not see those
controls. When `inference_mode` does not exist, `RuntimeParams` uses `dense`
(`llama.cpp`) by default; `auto` is an explicit choice.

`GET /api/ui/infer/heads?model=<id>` is the dedicated read of the inference
heads, allowed for the `infer` and `admin` profiles: it returns only the live
machines announcing a range starting at L0 for the requested model, plus the
chosen head, in-flight load and the RTT/age of each candidate. The planner
requires full coverage; coverage comes first and, among equivalent ranges, a
score weighs fresh RTT and in-flight load before the `node_id` tie-break. The
measurement is valid for five minutes. `candidates[].load` comes from the
candidate's own `/api/ui/node`; if that read does not arrive, it is `null`, not
zero. Without a candidate the answer is `status: "no_candidate"`; with a head
but no full coverage it is `status: "incomplete_pipeline"`. An expired RTT shows
up for diagnosis but does not enter the choice. `POST /v1/chat/completions`
also accepts the optional POM field `inference_head`, with the `node_id` of one
of those candidates. When present, the planner checks that the node is still
alive and starts the pipeline with it; it does not recompute another head by
RTT/load. The answer's `x_pom.source` and trace show the chain that actually
started at that head. A missing, invalid or vanished choice fails closed,
without silently switching nodes.

The same endpoint accepts `thinking_enabled: boolean` for that inference's
choice. A missing field keeps the default on; `false` reaches the prefill
metadata up to the deployment's head, which combines it with the global
`no_thinking` before applying the template. The hub, ring and direct reply
relays keep the field together with the existing metadata.
`GET /api/ui/chat/capabilities` publishes `thinking: "supported"`,
`"unsupported"` or `"unavailable"` for the UI, based on the global
configuration of the active model's head. Its `asleep` flag is true while a
machine of the active pipeline has its model unloaded for idleness or is
loading it again, so the Chat can say the next answer waits for the reload.

`GET /api/ui/deployments` deduplicates the persisted copies by `deploy_id` and
marks `serving` only when a live peer announces the deployment. The record can
stay visible with `serving: false` for diagnosis and recovery, but does not
publish a model to inference clients. `?local=true` answers only the local
record and does no fan-out; `/v1/models` uses the gateway's live list, not that
persisted catalog.

`POST /api/ui/deploy/node/options` queries the catalog and configuration of the
machine given in `target_node_id`. It returns the slice and whole-model options
available on that machine, plus GPU/KV defaults and the accepted cache types.
The screen calls the route again when the operator changes the dropdown,
avoiding reusing another peer's plan.

`POST /api/ui/deploy/node` (adding a machine to a deployment),
`POST /api/ui/deploy/node/remove` (releasing one machine) and
`POST /api/ui/undeploy` (taking the whole deployment offline) sit under the
AdminOnly `/api/ui/deploy*` and `/api/ui/undeploy` rules: besides
`ConnectionAuth`, the persisted `profile` of this node must be `admin` with
effective authority. The UI hides these controls for the `node` and `infer`
profiles, but the server keeps the authority. A single machine can still free
its own memory with `POST /api/ui/unload` or leave the network with
`POST /api/ui/leave`. Adding or removing with a `deploy_id` appends a new
version with the resulting set of machines. A partial removal keeps
`serving: true`; the last one writes an empty version, which stays in the
history but is not a live deployment. The record's `kind` becomes `mixed` when
a whole-model head and a sliced head coexist in the same deployment.

The remote routes `/api/ui/model-provider/search`, `/details`, `/storage` and
`/downloads` expose the `ModelProvider` catalog and jobs without forwarding the
provider's commands or tokens. `/storage` reports the model folders resolved
from the configuration. A download picks the first eligible folder, records the
path in `Job.local_path` and shows up in `GET /api/ui/jobs`, which is the UI's
source of progress, cancellation and retry.
**`GET /api/ui/model-provider/status`** requires `ConnectionAuth` and returns
`{ cli: { installed, version, authenticated, username } }`: a missing CLI is a
structured state, while login required and an authenticated session stay
distinguishable for the UI. The model detail queries the license on the
provider before the screen offers the download.

`GET /api/ui/requests` requires `ConnectionAuth` (and is AdminOnly), since it
returns prompts and request bodies. The filters are `node_id`, `user` (the API
key's human label), `api_key` (id, label or associated node), `model`, `prompt`
(FTS5 search), `from`, `to`, `limit` and `offset`; sorting is by most recent
`received_at`. The UI uses `limit` and `offset` for infinite scroll, without
loading the whole history on the first request. The database lives in
`pom.db`, in `POM_DB_DIR` when set or in `config::project_root()` by default, is
created idempotently at startup and keeps indexes on the prompt and on the
identity, node, state and time filters. WAL and a 5-second timeout stay as a
safety net. Inside the process, `PomDb` reuses a connection configured at
startup and serializes mutations with an explicit mutex; that makes short reads
wait for the write in progress, without reopening the file or running DDL per
request. The record is born as `in_progress` before inference and the same row
gets result, telemetry, timings, tokens, peers and error when it ends. A restart
or more than 15 minutes without completion marks the row as `incomplete` with
the visible reason. Writing uses a bounded queue and a separate writer; a disk
or enqueue failure is logged on the server with the operation and the lost
request id, and is not propagated to the inference request.

`GET /api/ui/requests/stats` requires the same `ConnectionAuth` and accepts the
filters `node_id`, `user`, `api_key`, `model`, `prompt`, `from` and `to`, but
does not paginate. The server makes a single aggregation over the filtered rows
and returns totals per token, node, day and model, consumption per peer
(`bytes_sent`, `bytes_received`, `network_ms`), average inference, average
TTFT, input/output tokens, terminal states and prefix cache (`input_tokens`,
`cached_tokens`, eligible requests and hits). That split avoids a heavy scan on
the client and keeps the row list small on the initial load. Both routes run on
any admin with effective authority and query the local SQLite. The generation
flow replicates audit events to the other admins over libp2p; the secondary
query does not fall back to the principal.

The record only keeps the API key's non-secret identity: `id`, `label` and
`node_id`. The secret enters neither SQLite nor the answer. The stored body is
the JSON representation of the accepted fields, without headers. The retention
policy for full prompts still needs a product decision: `needs-decision:
define retention and possible purge by age or volume`.

The `POST /api/ui/deploy/plan` and `POST /api/ui/deploy` routes carry
`params.inference_mode` (`auto`, `dense` or `moe`) in the development section,
for the three execution modes: whole model, sliced pipeline and Tensor
Parallelism. The planner also returns `is_moe`, `inference_mode_supported` and
`inference_mode_reason`. `is_moe` comes from the GGUF's real expert metadata,
never from the file name. For a dense model the UI hides the choice; when a
combination is not supported yet, the reason stays visible and the deploy is
refused so a preference that will not be honored is not presented.

In deployments with vision, `DeployPlan.mmproj_candidates` lists the compatible
local paths on the peer serving layer 0. The UI preselects one of them and can
send `DeployRequest.mmproj_path` to choose another format;
`POST /api/ui/deploy/plan` ignores that field. `DeployRequest.vision` still
enables the projector only on the head, and `mmproj_path` is only considered
with `vision: true` - without vision, a path that was sent does not refuse the
deployment. The path is resolved against the head
(`planning::resolve_head_projector`): the same path, or the head's only
candidate with the same file name, because each machine keeps its own copy in
different folders. The plan refuses, naming the file, only when the head does
not have it. The
backend redoes the survey, checks the chosen path against the GGUF headers in
the peer's apply and does not compute the SHA-256 of the `mmproj` payload to
discover or select candidates.

`GET /api/ui/model-sidecar-links` (`NodeAdmin`) returns `{ links, stale }`:
the principal's list of model -> projector / MTP draft links, read from the
principal by a secondary (which keeps the last copy and sets `stale: true`
when the principal does not answer). `POST /api/ui/model-sidecar-links`
(`AdminOnly`) saves one `SidecarLink`; a secondary forwards it to the
principal, which stamps `updated_at`/`updated_by`. A file link needs a name and
a size; vision cannot be `embedded`. `DeployRequest` accepts `vision_choice`
and `mtp_choice` (`{ mode, file: { file_name, size_bytes } }`) and
`DeployPlan.sidecars` answers with the compatible files of the network (each
with every holder and its path), the resolved `vision` (head) and `mtp` (tail)
slots and whether each is already there or will be copied.
`/api/ui/models` and `/api/ui/cluster/models` carry `sidecars` per machine.

**`GET` and `POST /api/ui/model-provider/config`** require `ConnectionAuth` and
control the optional Hugging Face token. GET only returns
`{ configured, masked, source }`, never the secret. POST receives `{ token }` -
`null` or empty text removes the value - writes `huggingface_token` to `pom.db`
with `0600` permission on Unix and updates the provider in memory, without a
restart. The effective precedence is `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN`, the
`pom.db` token and, last, the cache (`HF_TOKEN_PATH`, `HF_HOME/token` or
`~/.cache/huggingface/token`). So existing variables keep winning, and the UI's
explicit configuration beats the cache.

**`POST /api/ui/network/tls`** - body `{ tls_required: bool }`, turns TLS
between the peers of the whole cluster on or off. It answers **403 outside the
principal node**, and that check is on the server: hiding the switch on a
regular node's screen is convenience, not control. It needs no restart and can
be flipped as often as the operator wants - the effect is immediate on this
node (client cache, announcement, refusing remote clear text) and reaches the
others through the ratchet, in one gossip cycle. `GET /api/ui/node` returns the
pair `tls_required` (state) and `tls_authority` (whether **this** node can
change it), plus `addr_candidates`, `libp2p_peer_id` and
`inference_frame_capabilities` so a deployment plan can rebuild the tunnel on
the rank that will run the call. See `p2p-network.md`, section 5.

**`POST /api/ui/network/quic`** - body `{ quic_enabled: bool }`, turns the
HTTP/3-over-QUIC preference for new inference hops on or off. Only the
principal can change it; there is no restart and peers adopt the decision
through the authenticated ratchet. `GET /api/ui/node` returns `quic_enabled`
and `quic_authority` so the screen can tell state from authority. The UDP
endpoints use the dedicated ports announced in `inference_quic_port` and
`gateway_quic_port`, without reusing the TCP port or the `inference_url`. A
negotiation failure keeps the TCP path and shows up as a fallback in
telemetry; discovery caps the cost of dead endpoints at an aggregated budget of
300 ms and avoids a new handshake on the same endpoint for 5 s.

**`GET /api/cluster/logs`** - requires `InternalNodeAuth`, that is,
`Authorization: Bearer <connection_key>` plus `x-pom-internal-node: 1`, and
returns `{ logs_enabled }`. It is the authenticated source the nodes query when
the announcement's `cluster_logs_enabled` field triggers the ratchet; it does
not accept the announcement as policy.

**`GET /api/cluster/network-chat`** - uses the same `InternalNodeAuth` and
returns `{ network_chat_enabled }`. The gateway classifies it as
common/internal, so a peer call does not go through the human token ratchet.

`pom.json` accepts `cluster_logs_min_level` (`trace`, `debug`, `info`, `warn`
or `error`) to choose the minimum level recorded; the default is `info` and it
does not change the console. The principal prunes `cluster_logs` periodically,
keeping the last seven days and at most 500,000 lines, whichever comes first.
The batched pruning and the WAL recovery do not reach `audit_requests`,
`audit_request_hops` or `audit_prompt_fts*`.

**`POST /api/ui/network/logs`** - body `{ logs_enabled: bool }`, turns the
capture and sending of the cluster's real-time process log on or off. Only the
principal accepts the operation; the effect applies without a restart and is
announced as a trigger for the other nodes' authenticated fetch.

**`GET /api/ui/cluster-logs`** - requires `ConnectionAuth` on any admin with
effective authority and returns the lines persisted in the local replica, in
arrival order, with `deploy_id`, origin timestamp, node name and a drop
counter. It accepts `deploy_id` and `limit`; it does not reorder by
`origin_at`. With no local data, the empty answer is legitimate and does not
mean the role is secondary.

**`GET/POST /api/ui/thermal`** - requires `ConnectionAuth`. GET returns the
local policy `{ limit_c, hysteresis_c, cooldown_secs }`; POST validates that the
limit is at least 70 °C, that the cooldown is not over 60 seconds, and that the
margin is finite, positive and forms a valid hysteresis, writes the policy to
`pom.db` and returns the effective value. The runtime applies the change
without a restart. `GET /api/ui/node` includes the same policy in `thermal`,
together with `system.gpus[].temperature_c`, which is `null` when the driver
does not provide the reading.

### Conditional panel reads

`GET /api/ui/jobs` and `GET /api/transfer/offers` answer with `ETag` and
`x-state-version`. When the client sends `If-None-Match` with the current
version, the gateway answers `304 Not Modified` without a body. The offer list
only carries `{ offers }`; free disk space is an internal transfer decision,
not part of the UI's read contract.

`GET /api/ui/cluster` uses the same conditional contract. Each gateway keeps
the already built snapshot for up to 2 seconds, protected by a fingerprint of
the known state of peers, runtime, configuration and deployment. A known change
swaps the fingerprint and forces a new collection before the TTL ends. The
`ETag` only ignores `system.sampled_at`, which is sampling noise; metrics and
topology still invalidate the version. `x-state-version` is monotonic for
changed snapshots.

The `/api/ui/cluster` payload carries `traversal`, the NAT traversal
diagnostic of **this** node: AutoNAT state (`unknown`/`public`/`private`), the
published multiaddresses (`addrs`, circuit included), the relays that accepted
a reservation (`reservations`), and the hole punch count with the last failure.
It enters the cache fingerprint, otherwise the snapshot would freeze - neither
the AutoNAT state nor a new reservation touches `peers`. See
`docs/subsystems/p2p-network.md`, "NAT traversal".

In the `/api/ui/cluster` payload, `PeerView.load` is the in-flight request
reading published by the peer itself. For the local peer the gateway queries
its `LoadTracker`; for remote peers it uses the `load` field of the respective
`/api/ui/node`. If the remote read does not arrive or has no valid number, the
field is `null`, not zero. The deployment planner keeps the same distinction in
`DeployMachine.load` and in the load distribution.

Reading each peer's `/api/ui/node` walks `PeerInfo::gateway_candidates()` - one
URL per known address, `addr` first - and feeds the `RouteTable` failure
memory: a transport error or timeout discredits that address, a good answer
clears it. An invalid answer and an HTTP error do **not** discredit, because
whoever answers 401 or 500 is alive there. Each peer carries `unreachable` (it
answered, the read broke) and `rediscovering` (every known address died, the
node already asked the network for the current one) - distinct states, never
both at once. `rediscovering` expires after 90 s: after that the peer goes back
to `unreachable` saying none of the known addresses answered.
`model_inventory[].unreadable_nodes` names the peers the coverage depends on
that could not be read; coverage comes from gossip, not from the read, so
without that list a model would look "complete" with half of it unreadable.
Temporary IPv6 addresses rotate on their own and the cluster has to cope with
that without `--advertise-addr`: see `docs/subsystems/p2p-network.md`,
"Address that stopped answering".

**`POST /api/ui/chat/telemetry`** - an on-demand collection made after a Chat
run. With `execution_id`, the gateway reads that request's persisted trace and
derives each edge from `pipeline_cycle.decode`, which records the observed
logical crossing and the effective transport (`http`, `https` or `quic`).
Without the id, the Dashboard shows the network, not this node's last run: it
enumerates every complete route of the active model and estimates them with the
same sources and the same math as the planner (`fastest::describe_chain`), so
the card shows the same value on any node. Each route answers in
`pipeline_routes` with `{ node_ids, legs_ms, rtt_ms }` and, when measured,
`network_ms` (network paid per token in the chosen return mode), `return_mode`
(`direct` or `ring`), `compute_ms` (one summed decode step, with the queue),
`total_ms` (network in the chosen mode plus compute, one token through the
route) and `tokens_per_sec`. `total_ms` is not the sum of the legs: with direct
reply each leg costs half an RTT, and with the ring the tail's return is not
paid - adding the full round trip of each edge doubled the network cost.
`peer_perf` carries, per peer of the active model, `prefill_tokens_per_sec`,
`decode_tokens_per_sec` and `active_generations`, read from the admission probe
(which runs right there for a peer not probed yet). The leg order is
`Gateway -> head -> ... -> tail -> Gateway`, and routes are sorted by
increasing `total_ms` (or, without it, by aggregated RTT), with `node_ids` as a
stable tie-break. The RTTs are those of the pinned routes (`route_plan`): the
viewer measures its own edges to the head, and each peer publishes its own in
`/health` (`route_rtt_ms`, `gateway_rtt_ms`, with the transport in
`route_kinds`/`gateway_route_kinds` and the peers only reached through a relay
in `relayed_peers`). A leg pinned to the tunnel toward a peer in
`relayed_peers` comes out with `relayed: true` in the measurement and marks the
whole route with `relayed: true`, with the legs in `relayed_legs` (`[from,
to]`; the first can leave from the gateway itself and then only matters for
requests that enter through it). The tail's direct return only counts when the
chosen mode uses it: if it would go through a relay and the planner chose the
ring, the route comes out with `relay_bypassed_by_ring: true` instead of
`relayed`. The Dashboard shows a compact orange line and the detail in a card
when the icon is clicked. Each peer's speed and routes come from the admission
probe; since it only runs when the node serves a request, the Dashboard probes
again the peer whose last probe is older than 15 s (`PANEL_PERF_MAX_AGE`) -
without that, a node nobody uses as a gateway showed the route the peer had
right after starting. The same measurements go into `measurements` for the
edge labels (`tunnel` comes out as `libp2p-tunnel`, the tail's return with
target `gateway`); the last run's trace only fills an edge the planned route
has not measured yet. Before, the panel depended on the trace, and a node that
was never the gateway of a run showed no RTT at all. If the tail is already the
gateway, it appears only once and does not create an artificial leg to itself.
A leg without measurement stays `null`, so that route's cost stays unknown. The
answer can also contain `pipeline_cost: { legs_ms, compute_ms }` for the
observed run. Run items prefer `net_rtt_median_ms` and fall back to the run RTT
in old traces; the probe is not renamed as inference telemetry.

**`GET/POST/DELETE /api/ui/chat/history`** - requires `ConnectionAuth` and
keeps Chat sessions in `pom.db`, not on the file system or in a separate
SQLite. POST receives a full session in camelCase (`id`, `title`, `model`,
`modelAtStart`, `messages`, `updatedAt`) and replaces it atomically; messages
keep visible content, sent content, events, errors, telemetry and trace. GET
returns the sessions by most recent update. DELETE without an id clears every
session and `DELETE /api/ui/chat/history/:id` removes one session. The content
is only returned to an authenticated operator and is never included in error
logs.

### Shared network information

`GET /api/ui/network/info` uses `ConnectionAuth` and is available to
administrator, node and infer. It returns the `network` block persisted in
SQLite, with a fallback to `pom.json`, and `can_edit`, which is only true for
an effective administrative authority. Node and infer use that snapshot as
read-only information.

`POST /api/ui/network/settings` is an administrative mutation. The server
normalizes and validates name, visibility, country and city, writes the block
to `pom.json` and SQLite, republishes the geo label in the pkarr record and
sends the same configuration to each peer. The call between nodes uses the
signed control envelope; a private network adds the `connection_key` only as a
transport guard. The receiver checks the sender's authority before persisting.

`GET /api/ui/network/trust-share` is only for the authorized administrator. It
returns the root document with the real file name for download and includes
the `connection_key` only when the network is local or private. The root can be
shared on any network; public networks distribute no secret. The document is
the local signed chain, with the `setup` metadata captured in the wizard. For
an old primary chain without that block, the route recovers the network fields
from `pom.json` and the directory ingresses from `pom.db`, signs and persists
the block before returning it. If the local authority cannot rebuild the data,
the download fails instead of delivering an incomplete root.

Setup exposes local discovery on the same loopback listener, without requiring
a session: `GET /api/setup/auto-discovery/status` reads the step's state, while
the admin uses `POST /api/setup/auto-discovery/start` to keep the initial
listening and node or infer uses `POST /api/setup/auto-discovery/search`
(optional body `{ node_name }`) to look for an admin. The `Probe` carries a
summary of the machine (`DiscoveredMachine`: name, system, architecture, CPUs,
RAM, GPUs and VRAM), informative and outside the transcript; the admin lists it
in `peers` already at the `Probe`, with the source IP and `joined: false`, and
marks `joined: true` when it delivers the root. The public datagram only
carries the trust document. The admin only answers the `Proof` that ties the
challenge to the same request's ephemeral keys; it then sends a `Secret`
encrypted with X25519 and ChaCha20-Poly1305 containing the document and the
`connection_key`. The persisted preference `local_auto_discovery` starts
`false` and the administrative version uses
`GET/POST /api/ui/network/auto-discovery`; that route requires `AdminOnly`,
never returns the key and drops the in-memory material when it receives
`enabled: false`.

## 3. Authentication (`auth.rs`)

There is an operator credential, an inference credential and a private
extractor for synchronization between nodes:

- **`ConnectionAuth`** - reads `authorization` (optional `Bearer` prefix) or a
  valid `x-pom-control-signature` envelope and compares with `connection_key`
  when the network is private. It is the operator credential: it covers
  `/admin/*`, `/api/ui/*` (mutating), `/api/transfer/*` and `/api/ui/deploy/*`.
  No header = 401 `missing Authorization header`; wrong key = 403.
- **`ApiKeyAuth(pub Caller)`** - accepts `authorization` **or** `x-api-key`
  (the client picks according to the env var it has: `OPENAI_API_KEY` sends
  `Bearer`, `ANTHROPIC_API_KEY` sends `x-api-key`). Only an active API key
  minted at `/admin/api-keys` authenticates inference, with `id`, `label` and
  `node_id`. The `Caller` built here is the only place that sees the credential
  and feeds the audit with that identity; the secret is never stored or
  returned by the audit route.
- **`InternalNodeAuth`** - requires the signed envelope and the marker on
  public control calls; on private networks it also accepts the shared key. The
  signed sender must be known and control mutations must be covered by a
  current authority. It is never accepted on the client inference routes.

`POST /admin/regenerate-key` replaces the `connection_key` (SHA256 of a UUID);
minting keys **accumulates** instead of replacing - one token per node/harness,
and the log attributes each generation to whoever paid for it.

**Routes without `ConnectionAuth`**: `/peers`, `/health`, `/handshake` (the key
goes in the body), and the read GETs of `/api/ui` (`node`, `models`, `cluster`,
`cluster/models`, `peer/:node_id/runtime`, `paths` GET, `jobs`, `jobs/:id`).
Those `/api/ui` reads are still behind the administrative surface boundary
(loopback in `localhost` mode, session in `network` mode) and the profile
table. `GET /api/ui/requests`, `/api/ui/requests/stats` and
`/api/ui/cluster-logs` are exceptions because they can contain sensitive
content and require `ConnectionAuth`. The OpenAI/Anthropic `/v1` routes
**require** `ApiKeyAuth`. A key unknown locally queries the principal over the
authenticated internal channel; each attempt has a 2 s timeout and there are at
most three attempts with backoff. There is no negative cache: timeout, error or
an invalid answer refuse the request. A valid remote key is stored in `pom.db`
and stays available for the next requests until revoked.

`POST /api/ui/chat/telemetry` stays outside the read exception: it requires
`ConnectionAuth` because it forwards internal RTT reports between peers.

The internal inference port (`inference_port`, normally 8181) has another
router. `/v1/prefill`, `/v1/step`, `/v1/step-stream`, `/v1/link`,
`/v1/session/release`, `/v1/session/rollback`, `/v1/session/slot` and
`/v1/tp/reduce` require the private credential on private networks, or TLS plus
the marker on public networks; client API keys are refused. `/health` stays
public for readiness probes.

The `x-pom-inference-session` identifier is still only opaque session context.
It is not a credential, holds no API key and does not replace the internal
channel's `connection_key`.

`/v1/link` is the persistent connection between pipeline stages: **one** POST
whose outgoing body is a stream of `frame/v1-batch` frames and whose returning
body is another stream, correlated by `batch_id`. The credential is checked at
opening by the same middleware as the other `/v1` routes **and rechecked on
every frame**, so a revocation made by another process takes effect at once
and not only when the channel drops. The body limit is turned off on this route
because the body is the whole generation, not a frame - each frame inside the
stream is still bounded by the channel's framing. See `inference.md` §2.1.

On the gateway, the same path is a second termination of the protocol, used
only for the `remote-stream` return. It runs no inference: it only dispatches
`REMOTE_STREAM_RESULT_PATH` to `ReplyInbox`. That keeps the return open and
avoids one HTTPS POST per token on the normal path.

Between peers, an inference batch goes down this transport ladder, and only
goes down a step after the previous one failed on **transport**: persistent
channel (`/v1/link`, when the hop announces it), libp2p/Noise tunnel, pinned
direct client. An HTTP refusal from the peer is final at any step and is not
retried another way.

### `relay-result` and `remote-stream-result` use a Ticket

These routes do not use `ConnectionAuth`, because the hop that answers is an
**inference peer**. The sender carries the **ticket** in the body: a UUID v4
drawn per frame, known only to the chain itself, removed from `ReplyInbox` on
`Drop` on any exit path. There is no HTTP API key on these routes. A late
answer gets `410` and can never fall into the next token. The remote callback
keeps the same rule, but delivers several batches to the bounded
`StreamTicket` instead of resolving a oneshot channel.

## 4. `x_pom.source` - the proof of distributed execution

The same string the request log stores as `source`:
`"pipeline:<id1>→<id2>"` (with `" (partial)"` when a peer failed midway but
some text was left), `"local"` or `"peer:<node_id>"`. It shows up as the
`source` field in `/api/generate`, as the `x_pom` extension in non-streaming
`/v1` and in the last SSE chunk, and as `source` in `reqlog`. `"none"` marks an
error/abandonment. It is the only way for a harness to know inference was
joint: a `source` starting with `pipeline:` and with **two** hops is the proof.

`x_pom.timings` is its own extension, both in the last SSE chunk and in
non-streaming answers. The OpenAI-compatible format stays intact: clients that
ignore `x_pom` keep reading `usage`, `choices` and the other standard fields.
`x_pom.trace` carries the same `RequestTrace`, including the local spans,
`trace_id`, cold/warm state and the optional fields `outside_node_total_ms`,
`outside_node_accounted_ms` and `outside_node_unaccounted_ms` when there is an
explicitly measured residual, connection counters, first token milestones,
streaming metrics, the four per-request MTP acceptance fields and the
distributed integrity `telemetry` summary, with expected and observed peers and
spans, duplicates, late receipts, absences and `remote-stream` aggregates.
`x_pom.timings` separates legacy TTFT, `server_ttft_ms`, `engine_ttft_ms` and
`client_ttft_ms`. A missing field means the boundary was not measured, not
zero. The audit at `GET /api/ui/requests` keeps that same object in
`telemetry`, next to the generation `timings`.

## 5. Route tests - there is a single scaffold (`gateway::testkit`)

`gateway::testkit` (`#[cfg(test)]` in `gateway.rs`) builds a whole
`GatewayState` with the peer table seeded by hand: `peer(node_id, hostname,
gateway_port)` builds a loopback `PeerInfo`, `state_with(node_id, peers)`
builds the state (with a real `Network` on an ephemeral UDP port, but
**nothing speaks UDP** - a test never announces on the LAN nor falls into the
real cluster), and `state_with_secondary_admin(node_id, peers)` builds a real
secondary administrator: another node is the principal and the local node gets
a signed `Secondary` grant. `body_of(response)` extracts the answer's JSON. It
is the only way to test a control route without broadcasting; `transfer`,
`deploy` and `webui` use it. To run a real node without announcing, the
production equivalent is `--no-discovery`.

## 6. Defects noted

None open in this file. (The `/admin/transfer-offers` stub now returns the real
offers, and the `/api/ui` read GETs are behind the administrative surface
boundary.)
