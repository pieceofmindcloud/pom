# Spec: Admin-hub routing (every prompt through an admin)

Date: 2026-11-14. Status: **proposed** - decisions listed at the end need the
operator's sign-off before any plan is written.

- Problem: any node can be a gateway today, including visitors and workers, so
  every member learns the whole topology, plans its own routes, and dials
  remote inference ports directly.
- Decision: route **all inference through an admin entry node**. Visitors see
  only their admins, direct or through a relay. Workers reach inference through
  an admin too (serving stays peer-to-peer). The admin plans centrally and pins
  a session to a route. The membership token (JWT) loses its reason to exist on
  the inference path.
- Invariants kept: mTLS between nodes, the trust chain, API keys local to the
  minting node, the pipeline's layer order, `frame-v1-batch` compatibility,
  no weight transfer, the two output budgets.

---

## 1. Today's baseline (what the code does now)

Cited from `docs/subsystems/p2p-network.md`, `inference.md`, `payments.md`,
`security-model.md` and the folder `AGENTS.md`s.

- **Any node is a gateway.** `gateway::generate_stream_with_stops` plans the
  pipeline per request (`routing/pipeline.rs`, `fastest.rs`), on **whichever
  node received the client request** - admin, worker or visitor - and dials
  every hop's inference port (QUIC -> HTTP -> p2p tunnel). The plan is rebuilt
  per request; only the head (`auto_head` session affinity) and the hop address
  (`resolve_step_url`) are sticky.
- **Everyone sees everyone.** `Announce` gossip (UDP + iroh) fills every
  member's `peers` table with every other member's range, ports, capabilities
  and addresses. A visitor today may hold worker peers in its table and dial
  their inference ports directly (the `direct` / `member_gateway` modes).
- **Roles are carried by a JWT.** A worker or visitor proves its role to *any*
  node with a 10-minute EdDSA token signed by an admin (`membership.rs`,
  `backend/webui/membership.rs`), because a node with no direct relationship to
  the principal must be able to authenticate the many callers that talk to it
  directly (`is_member_key`, `caller_may_ask`, `require_inference_auth`,
  `RelayGate`, `anon_tunnel` binding, `peer_list_for`).
- **Sessions and KV are local per hop.** Each peer keeps its own KV sessions;
  the gateway opens/reuses/rewinds/releases one per hop
  (`/v1/session/rewind`, `release`); the gateway's `PrefixCache` keys on
  *model + hop sequence*; disk prefix cache lives per node
  (`slot_save_path`, `GatewayState.prefix_disk`); `/admin/slots` snapshots a
  whole deployment fan-out across hops.
- **The seed of this design already exists**: `inference_access =
  member_gateway_only` + `entry_nodes = admins` (`backend/delegation.rs`,
  `POST /v1/gateway/delegate`, `gateway/delegate-v1` / `gateway/client-v1`,
  `Network::set_visitor_view`, `403 visitor_must_use_gateway`) - today it is an
  option that only the charging network turns on. This spec promotes it to the
  network's shape.

## 2. Goals

1. Visitors see only the admin(s): direct connection or through a relay, never
   another node. No worker addresses, routes, or topology leak to a visitor.
2. Workers route their *client queries* through the admin; the admin owns route
   planning centrally. Worker serving hops stay worker-to-worker.
3. The client (and the node the client sits on) never picks the route.
4. Sessions pin to a plan: the admin reuses a previous planning for a session,
   keeping KV/prefix caches warm and making the pin explicit instead of the
   current emergent head affinity.
5. Central tracking and balancing: one gateway sees every prompt of the
   network - quota, audit, metering, load.
6. Remove the membership token from the inference path; replace it with the
   admission roster each admin already holds.

### Non-goals

- **Not a proxy data plane.** Inference frames must not bubble through the
  admin per token: pipeline bandwidth scales with pipeline size, and the admin
  becomes the throughput bottleneck and single failure point. Hub-gateway,
  distributed frames - the existing `resposta=direta|anel|hub` modes are the
  data plane, not the client topology.
- Not changing the layer-coverage rules, the wire format, or the deploy model.
- Not removing mTLS, the trust chain, or the connection key handshake.

## 3. Target architecture

### 3.1 Three planes, three trust zones

| Plane | Path | Who authenticates |
|---|---|---|
| Client entry | client -> its own node (`/v1/*`, `/api/generate`, local API key) | node-local API key, unchanged |
| Control | member node -> admin entry node (`POST /v1/gateway/delegate`, signed peer call) | mutual TLS node key + admin's own admission roster |
| Serving | gateway -> hop 1 -> ... -> hop N -> gateway reply (QUIC/HTTP/tunnel, `frame-v1-batch`) | gateway transport identity + per-frame `Ticket` |

The only change to the *serving* plane is **who holds the plan** (the admin)
and **which members a node is allowed to reach** (a visitor: admins only).

### 3.2 The entry node (admin)

- The admin runs the existing delegate machinery for **every** member request,
  workers included: `visitor_layer` generalizes to a `member_layer` middleware
  that relays `/v1/chat/completions`, `/v1/completions`, `/v1/messages` and
  `/api/generate` to an entry node, and the entry node refuses to delegate to
  another entry node (no gateway hopping).
- The admin plans centrally with the existing planners (`plan_by_delivery_time`
  / `plan_pipeline`), over RTT and load **it measures itself** from its position
  (the current per-node directional RTT discipline, `RttTable`, keeps its
  "never transitive" rule).
- The admin is also the balancer: `LoadTracker`, admission probe quarantine,
  quota gates and payment holds all already run on the gateway - centralizing
  the gateway centralizes them.

### 3.3 Session pinning

- The admin keeps a **session table**: `session_key -> pinned plan`
  (`node_id` chain per model/deploy), keyed by the existing cache key
  (`conversation_cache_key` + model + deploy) plus the client's identity
  bucket. TTL follows the session lifetime rules already in
  `inference.md` (retention at EOS, release is explicit).
- On a new turn, the admin re-validates the pinned chain against live peers
  (`resolve_chain` already validates a chain of `node_id`s against live
  candidates). Idle peers wake through the existing `reload_model` fan-out. A
  peer gone or quarantined releases the pin and re-plans once; the rebuild
  semantics stay the current `session-lost-cold`.
- The gateway `PrefixCache` already keys on "model + hop sequence": with one
  planner and pinned chains the key stops drifting per request
  (`gateway_prefix_miss_reason: route-changed` should almost disappear), and
  the head affinity of `auto_head` becomes a property of the pin, not a guess.
- Optional per-turn re-balance: if the pinned chain is measurably worse than
  the best chain (margin like the route plan's 25% + 2 ms), release the pin,
  re-plan, and pay the cold restart. Default off.

### 3.4 Visibility

- Generalize `set_visitor_view` into the default network shape:
  - a **visitor** keeps only its admins in `peers`/`alive_peers`/`PeerList`
    (`entry_nodes` fixed to `admins` for visitors; workers are withheld from
    visitors entirely),
  - a **worker** sees admins and the workers it serves with (deploy peers), not
    the whole cluster,
  - a visitor's relay offer + anonymous relays (`relay_pass`, `anon_tunnel`)
    stay the path for a visitor behind CGNAT reaching its admin: "directly or
    through a relay" is already implemented for the visitor -> admin hop.
- A worker that serves only its own local clients (single-machine network) is a
  degenerate hub where the operator's own admin *is* the node; a standalone
  worker with no admin is unchanged (local network of its own, as today).

## 4. The membership token: what it does today, what replaces each use

| Use today | Under the hub | Verdict |
|---|---|---|
| A stranger node checks a caller's role (`is_member_key`, `caller_may_ask`, `require_inference_auth`) | Roles are only checked where the relationship exists: the admin holds the admission roster (the principal's list, mirrored to secondaries by `internal_roster` today) | **Removed on the client/control path** |
| Admission gate for gossip and the `peers` table | Admin-local roster check against the transport-proven node key | **Replaced by the roster** |
| `ServingMember` route scope (worker vs visitor between nodes) | Serving calls come from the gateway (the admin, chain-proven) or from deploy-scoped serving peers (chain membership suffices; the plan named them) | **No role needed** |
| `visitor_must_use_gateway`, `peer_list_for`, `hides` | Role is known locally: the admin knows which node keys are visitors in its roster; the visitor's view is hardcoded to "admins only" | **Roster decides** |
| Revocation taking effect "on the next request" | Admin refuses a roster-banned key on the next request directly; revocation entries stay as the replication mechanism of the ban between admins | **Kept between admins only** |
| `anon_tunnel` binding checks the caller `is_member_key` | Admin-local roster check at binding time | **Replaced** |
| GPU metering receipts naming roles (visitor gateway pays hops, never earns the gateway fee; validator) | The metering gateway is the entry node the admin designated; role facts come from the roster and `Config.validator` | **Replaced** |

**Honest assessment: "removes the need for the JWT" is true for inference, not
for all networking.** The control plane still has gossip, PeerList, chat relay,
log fan-out, chain sync, and admin -> worker control calls where a node must
decide "is this key currently a member, and what role" **without** the
principal online. Today the answer is the token checked against the chain
alone. The hub model needs a replacement: an **admin-signed admission grant /
vouch** (a signed roster entry: `{net, node_key, role, admitted_at,
expires?, revocations}`), cached locally at every node that gossip still
reaches. It is longer-lived and fewer-moving than the token, but it is still a
signed role document: what disappears is the *per-node role check on the
inference path*, not signed role proof everywhere. Calling it "removes the
JWT" in the docs without saying this would be dishonest; the accurate claim is
"the membership token no longer gates inference and no longer rides the
announce".

Middle ground (recommended as step 1): keep the token code, stop requiring it
on the serving path, and let the roster decide the visitor view. The removal
then lands as a clean, separately-testable step.

## 5. Item-by-item map of changes

Code paths per `crates/node/src/...`. "Extend" = additive, old form still
readable (compatibility policy, root `AGENTS.md`).

| Area | File(s) | Change | Kind |
|---|---|---|---|
| Visitor view | `network/network.rs` (`set_visitor_view`, `hides`, `peer_list_for`, `self_withheld_from_visitors`) | Default on (visitors learn admins only); visitors never listed workers; extend to the worker's deploy-scoped view | Extend + new default |
| Announce privacy | `network.rs::self_announce`, `discovery.rs` | A visitor's announce already goes only to admins/workers; under the hub, to admins only. Workers announce to their deploy peers + admins, not the world | Extend |
| Delegation for workers | `backend/delegation.rs` (`visitor_layer`, `entry_nodes`, `DELEGATE_PATH`) | Generalize to `member_layer`: any member's generation request goes to an entry node; entry nodes refuse delegation between themselves; `entry_nodes` value `admins_and_workers` stays readable and reads as a legacy alias of `admins` | Extend |
| Worker local loopback | `openai.rs`, `gateway/*` | A worker's own API keys keep working: the request is handed to the entry node in-process-or-over the wire exactly like the visitor's today | Behavior change |
| Inference port gate | `inference/infer_server/prefix.rs` (`require_inference_auth`), `backend/delegation.rs` (`refuse_visitor`) | Serving routes accept only a gateway or a chain member **named by the current plan**; the role question ("is it a visitor?") disappears | Extend |
| Route scopes | `backend/webui/routes/mod.rs`, `backend/gateway/state.rs` (`caller_may_ask`) | ServingMember stops consulting the membership token; roster/chain decides. Roster keeper already exists (`internal_roster`, `spawn_roster_keeper`) | Extend |
| Membership keeper | `backend/webui/membership.rs` (`spawn_token_keeper`, Announce `membership` field) | Stop putting the token in the announce; keeper retained while the role check exists between admins | Extend |
| Central planner state | `gateway/state.rs`, `routing/pipeline.rs`, `routing/fastest.rs` | Plan cache + session pin table on the gateway (`session_key -> hop chain`), validated via `resolve_chain`; `auto_head` folds into the pin | New (small) |
| Chat head choice | `openai.rs` (`inference_head`), `webui` heads route | Head pinning stays but is resolved **by the admin's plan**; the Chat's `inference_head` becomes advisory to the admin | Extend |
| Idle wake | `gateway/generation.rs` (`reload_idle_pipeline`) | Unchanged - already gateway-driven; the gateway is now always the admin | None |
| Payments/quota/metering | `payments/runtime.rs`, `quota.rs`, `metering.rs`, `delegation.rs` | Entry nodes are admins (`entry_nodes` fixed); the visitor gateway fee, leases and bundles attribute to the admin gateway; `entry_nodes = admins_and_workers` is read as admins (no behavior break for existing charging networks) | Extend |
| GPU work metering roles | `metering-core` role fields, `metering_verify.rs` | Roles resolved from the roster at the entry node | Extend |
| Anonymous relays | `relay_pass.rs`, `network/anon_tunnel.rs` | Role check at the binding becomes roster-local at the admin; passes unchanged | Extend |
| Chat visibility | `network_chat` (`chat_visitors`, `hides`) | Principal relay already central; visitors keep not seeing each other unless `chat_visitors` | None |
| Local networks | `webui::bootstrap_local_network`, `profile.rs` | A node without `pom.json` is its own principal: hub-of-one, already compliant | None |
| Frontend | `apps/frontend/src/routes/{Cluster,Chat,MyCredit}.tsx`, cluster reads | Visitor profile shows only admins; `ConnectionPhase` states reworded ("connected to the admin"); demo redaction already hides topology | Extend |
| Deprecations | `config.rs` `inference_access` enum | `direct` kept readable for one more release as a legacy mode (documented deviation, see 7.3); new default `member_gateway_only` | Extend + deprecate |

Docs to update in the same commits: `security-model.md`, `p2p-network.md`,
`payments.md`, `http-api.md`, `inference.md` (6, 3.1), deployment/web-ui
guides.

## 6. Pros

1. **Topology privacy**: a visitor (or a compromised member) learns no worker
   addresses, ranges or routes; the attack surface of every worker shrinks to
   the members its plan names.
2. **One place that knows every prompt**: quota (`quota.rs` fail-open today),
   audit, payment holds, load tracking and the `idle` wake all become network
   truth instead of per-gateway guesses; balancing decisions get a complete
   view.
3. **Better cache economics**: the admin owns the only planner, so the
   `PrefixCache` key ("model + hop sequence") stops drifting; session->plan
   pinning makes prefix reuse and KV-slot reuse deliberate, and the same pin
   makes a `restore_deployment` snapshot reliably replayable on the pinned
   route.
4. **Simpler identity story**: visitors authenticate to exactly one
   principal-ish peer; the 10-minute token dance, its keeper, its announce
   field and its clock-skew failure mode leave the inference path.
5. **Cheaper NAT/relay story for visitors**: visitor<->admin already works
   through anonymous relays and hole punching; no visitor ever needs a path to
   a stranger.
6. **Reuses shipped machinery**: `gateway/delegate-v1`, `visitor_layer`,
   `set_visitor_view`, `resolve_chain`, `auto_head`, reload fan-out - this is
   turning a tested optional mode into the shape of the network, not a new
   protocol.
7. Billing and metering become one ledger at the entry point, which is also
   what the validators design (prompts kept off untrusted machines, first/last
   layers trusted) already leans toward.

## 7. Cons and risks

1. **Admin load and bandwidth shift.** The admin already runs planning,
   admission probes, `/health` reads and quota; now it receives every prompt
   body (32 MiB cap on delegate already) and streams every answer back. The
   frames themselves still flow peer-to-peer, so bandwidth is bounded by
   fan-in/fan-out of client-facing streams, not tokens. Mitigation: multiple
   admins (secondary admins are entry nodes; the `entry_nodes` vocabulary
   stays), delegation ordered by measured RTT (already: nearest entry first).
2. **Single point of failure per request leg.** Today a worker whose peer
   table has the whole cluster can route around a dead admin. Under the hub,
   an unreachable admin stops *new* requests (an in-flight pipeline keeps
   running, and hops never depended on the gateway except for dispatch and
   reply - direct reply mode already sends the tail's answer straight back to
   the gateway, which is now the admin). Mitigation: every member keeps all
   admins, not one; delegation already moves to the next entry node when one
   gives no answer.
3. **Principal offline vs secondaries.** Known limit today (a secondary
   restarts with an empty roster mirror). The hub makes it availability
   critical: plan roster-mirror persistence on secondaries (write the mirror
   to `pom.db`) as part of this change.
4. **Added hop latency for worker-served clients.** A request on a worker now
   detours to the admin and the pipeline dispatch comes back to the worker.
   One extra control RTT per request, not per token. On LAN negligible; on WAN
   this is the main cost of the model and should be measured (telemetry
   `outside-node` breakdown already exists).
5. **Session pin vs rebalance.** A pinned chain cannot be swapped for a better
   one without paying a cold prefill. Today's per-request planning adapts to
   churn instantly; the pin trades that for cache warmth. This is the intended
   trade, but re-planning policy (when a peer dies, when idle, optional
   margin-based re-balance) is the design's sharpest edge.
6. **Prompt confidentiality shifts toward the admin.** Prompt bodies are
   already sent to every head in the pipeline; under the hub they additionally
   transit the entry node even for a local worker's client. Acceptable (admins
   already read prompts - validators design, audit), but it is a real change
   in who *must* be trusted, and it should be stated, not glossed.
7. **The control-plane role problem remains** (section 4). Removing the token
   fully is not free; the roster-vouch replacement is more code than
   deleting `membership.rs`.
8. **Compatibility.** Networks today run `inference_access = direct` and
   `member_gateway` and mix versions while upgrading. Flipping the default is
   a behavior break even if the old values stay readable; see 7.3 for the
   migration stance.

## 8. Suggested refinements

1. **Step the rollout** (recommended):
   - Phase A: flip the visitor default (delegate to admins, withhold workers
     from visitors) in all access modes; keep `direct` for worker-to-worker
     planning. Small, testable, no token change.
   - Phase B: worker delegation to the admin entry node; plan pinning; roster
     decides roles on the serving gate.
   - Phase C: retire the membership token from announce/inference paths; add
     roster vouches for the remaining control-plane checks; `membership.rs`
     shrinks to revocation entries between admins.
2. **Persist the secondary's roster mirror** in `pom.db` before phase B (fixes
   a known limit this design makes load-bearing).
3. **Keep the Chat's `inference_head` as advisory**: the admin may honor it
   (it is a warm-cache hint) but owns the final plan.
4. **Make `entry_nodes` a per-role policy, not a single enum**: visitors ->
   admins always; workers -> admins (default) with an explicit escape hatch
   setting for networks that want a worker to keep direct planning (keeps the
   `direct` semantics for those who measurably need it, instead of a global
   mode).
5. **Expose the pin on the Panel**: show "session pinned to hop chain X" beside
   `hop_chain`; it turns the abstract "central planning" into something the
   operator can see and debug.
6. **A/B before flipping the default**: the harness (`continuation_lab`) and
   the trace's `outside-node` math can measure the added control hop and the
   prefix-reuse delta on the three-machine network before the release.

## 9. Compatibility and migration

- **Contract additions** (all additive, `frame-v1-batch` untouched): delegate
  already shipped as `gateway/delegate-v1` for members - delegation from
  *workers* is the same call with a new caller class; announce keeps its
  optional fields; `inference_access` keeps `direct`/`member_gateway` as
  readable legacy values, new default `member_gateway_only`.
- **Old visitors** (no `gateway/client-v1`): already cut off only by charging
  networks; after the flip they lose remote inference too, by the same rule -
  this must be a release-notes decision, not a side effect (compatibility
  policy). The Payments screen already lists such visitors; generalize that
  warning to the Network screen.
- **Mixed-version cluster**: a node too old to delegate keeps planning
  locally; plans that include it remain valid (it is still chain-proven); no
  old node breaks. The pin lives only on the admin, invisible to old peers.
- **State**: new local table (admin session pins; secondary roster mirror);
  nothing persisted changes format.

## 10. Decisions needed from the operator

1. Confirm "workers delegate too" (vs. workers keep direct planning; the
   proposal here says delegate, with the per-role escape hatch of 8.4).
2. Confirm `entry_nodes` fixed to admins (remove `admins_and_workers` from new
   networks, keep parsing it).
3. Confirm the re-plan policy default: pin until a peer dies or the session
   ends; no proactive rebalance.
4. Confirm the token end-state: full removal with roster vouches (phase C), or
   keep the token for control-plane checks and only remove it from inference.
5. Confirm whether `direct`/`member_gateway` stay configurable or are
   deprecated with a dated removal.

### 10.1 Decisions recorded (2026-11-15)

1. Confirmed: workers delegate too (delivered in B2, with the
   `worker_delegation` escape hatch of 8.4).
2. Confirmed: `entry_nodes` fixed to admins (delivered in phase A;
   `admins_and_workers` keeps parsing).
3. Confirmed: pin until a peer dies or the session ages out; no proactive
   rebalance (delivered in B3).
4. Confirmed: **full token removal with roster vouches** (phase C). The
   execution plan recorded here:
   - **C1 - the vouch plumbing, purely additive, gates unchanged** (delivered
  2026-11-15). A
     roster vouch is an admin's signed statement of one member's admitted
     role - `{net, sub, role, admitted_at, issued_at, iss, iss_key, sig}`,
     Ed25519, the same per-entry form as a `Revocation` ("whoever sends it
     does not matter"). Any admin signs vouches from its admission list (the
     principal's own, or its mirror); they travel by a push on each approval
     and a per-tick exchange beside the revocation keeper; every node
     verifies each entry against the chain, keeps the newest per member, and
     persists them beside the chain (`<trust>.vouches.json`). A vouch is
     valid while the issuer is still an admin of the chain, while it is
     fresh (`issued_at` within the vouch window - hours, against minutes for
     a token), and when `admitted_at` is newer than the member's last ban;
     removing an approved member already signs a revocation, so the ban cut
     covers removal and freshness is the defense in depth. A vouch is never
     signed for an admin of the chain.
   - **C2 - flip the role checks** (delivered 2026-11-15). `is_member_key`,
     `Network::peer_is_visitor`, `is_visitor`, the inference session gate,
     the metering consumer label, and the gateway gates on nodes that hold
     no roster read the vouch ledger first. The token stays issued,
     announced and accepted as the legacy form, so a mixed-version cluster
     keeps working (compatibility policy): the new claim is only "the vouch
     decides when a current admin signed it".
   - **C3 - retirement (a dated release-notes decision, a separate
     release):** new nodes stop announcing and issuing tokens; `membership`
     keeps reading old announces until the old version is retired on
     purpose, and shrinks to the revocation entries between admins.
     Pending: a release decision, and it is entangled with decision 5
     recorded below (retiring the announce is the moment the old versions'
     gates stop being fed, and it is also the moment the deprecated
     `inference_access` values stop being honored anywhere, so the two
     retirements ride on the same dated release).
5. Confirmed (2026-11-15): **deprecate now**. `direct` and `member_gateway`
   are deprecated: they keep parsing and writing for older nodes and are
   never honored by a new node; the node warns on load when a file carries
   one; the new default is `member_gateway_only`, and the charging gate
   collapses together with the enum. The dated removal rides on the same
   release as C3, the moment the old versions' gates stop being fed.

## 11. Out of scope

- A data-plane where the admin proxies inference frames per token.
- Cross-node KV/prefix cache sharing or cache migration between hops (the KV
  slot save/restore model stays; only the planner becomes singular, which is
  what the pin needs - this answers the "not sync kv-slot cache" premise:
  there is no new sync, there is one plan).
- Removing mTLS, the trust chain, the connection key handshake, or per-node
  API keys.
- Changes to tensor parallelism collectives, deploy planning, slicing, or
  transfer.
- UI work beyond what the visibility and pin display require.