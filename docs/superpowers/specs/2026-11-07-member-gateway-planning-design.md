# Planning by a network member, never by a visitor

Date: 2026-11-07 - Status: proposal for review

## Problem

Any node can be the gateway of a request, and the gateway plans the pipeline
(`routing/pipeline.rs`) and drives it. A visitor that receives a request from
its client therefore plans over its own view of the network and talks to every
hop:

- the hidden state crosses visitor and head (ring, remote token stream), or
  every hop (hub);
- in direct reply mode the tail delivers the result to the visitor, usually
  through the iroh tunnel;
- the session lifecycle (`release`, `rewind`, `rollback`) is posted to **every**
  step of the plan (`rollback_sessions` and its siblings in
  `backend/gateway/generation.rs`), whatever the mode.

Consequences seen or expected in the field:

- A visitor needs a working route to every worker of the pipeline. Reaching
  the head but not the tail leaves sessions held on the tail (failed release)
  and stalls later requests.
- The visitor plans from a partial view: what discovery handed it, measured
  from outside the network, with no first-hand queue or health data.
- Worker addresses and the network's layout reach every visitor.
- Quotas, audit and access rules are enforced on the workers' inference port
  and in the visitor's own records, never in one place that sees the request.
- Visiting several networks
  (`2026-11-06-multi-network-visitor-design.md`) multiplies all of it.

## Goals

- A visitor never plans. It hands each request to a **member entry node**
  (an admin or, when allowed, a worker), which plans and runs the pipeline as
  the gateway and streams the answer back.
- Workers can refuse visitors on their inference port, and visitors stop
  learning workers.
- Clients see no difference: same routes, same API keys on their own node,
  same streaming.
- Released visitors keep working until the network chooses otherwise.

## Non-goals

- Changing how admins and workers serve requests they receive directly; they
  already plan as the gateway.
- Splitting one request across several entry nodes.
- Load balancing beyond picking an entry node by measured RTT and health.

## Decisions

### 1. Network policy `inference_access`

`NetworkSettings` (`utils/config.rs`) gains `inference_access`:

| Value | Visitor behavior | Workers accept visitors on the inference port |
|---|---|---|
| `direct` | plans itself (today) | yes (today) |
| `member_gateway` | forwards to an entry node | yes, for older visitors |
| `member_gateway_only` | forwards to an entry node | no (`visitor_must_use_gateway`) |

and `entry_nodes`: `admins` (default) or `admins_and_workers`.

The principal sets both; they travel like the other network policies. Missing
fields read as `direct` / `admins`, so every released `pom.json` and every
existing network behaves as today. New networks created by
`bootstrap_local_network` start at `member_gateway`.

### 2. The delegate route

New peer route `POST /v1/gateway/delegate`, scope AnyMember with the handler
checking the caller is a visitor holding a valid membership token (a member
that is not a visitor plans itself and gets `409 plan_locally`). The body is
the client's request as received (`/v1/chat/completions`, `/v1/completions`,
`/v1/messages`, `/api/generate`), the route it came in on, and the visitor's
request id. The entry node runs the same handler it runs for a local client,
with itself as gateway, and answers with the same body or SSE stream.

- It travels over the streamed tunnel (`/pom/infer-tunnel-stream/1.0.0`) or
  any direct route the route plan pins, so a visitor behind NAT works.
- Authentication is the visitor's membership, as today for inference between
  nodes; the client's API key never leaves the visitor.
- An entry node announces `gateway/delegate-v1` and accepts delegation only
  when the policy names its role in `entry_nodes`.
- Cancellation: closing the delegated stream cancels the request on the entry
  node, which releases the sessions through its usual path.

### 3. Choosing the entry node

The visitor keeps a short list of live entry nodes (members announcing
`gateway/delegate-v1` whose role is allowed), ordered by measured RTT and
recent failures. A request goes to the first; it moves to the next only when
the request was **never delivered**. A request that may have been delivered is
not resent (the same rule as signed commands in `peer_client`). With no
reachable entry node the request fails with `503 no_entry_node` and the list of
what was tried; it never falls back to planning locally under
`member_gateway_only`, and does so under `member_gateway` only when no entry
node announces the capability (a network not upgraded yet).

### 4. What a visitor learns

Under `member_gateway_only`:

- Workers do not send their announce to visitors, and a `PeerList` sent to a
  visitor lists only entry nodes.
- The visitor's `/v1/models` is the entry node's catalog for the network
  (aggregated as for its own `/v1/models`), cached for a short time.
- Visitor screens show the entry nodes, not the workers.

Under `member_gateway` workers stay visible, so older visitors keep planning.

### 5. Records, quotas and telemetry

- The entry node records the request with `entry_node = visitor` and
  `gateway = itself` (new optional field), so the Business Pack request log,
  statistics and quotas see one row per request, keyed by the visitor node.
- The visitor records its side with `delegated_to = <entry node>`.
- `x_pom` in the answer carries `gateway` and a pipeline summary (hop count,
  TTFT, tok/s, mode). Under `member_gateway_only` hop identities are left out.

## Impact

### Gains

- A visitor needs one route, to an entry node. The head-reachable,
  tail-unreachable failure and the leaked sessions it causes are gone.
- Plans come from a node with first-hand discovery, measured in-network edges
  and live load.
- Topology and worker addresses stay inside the network.
- Policy (quota, audit, access) is enforced where the plan is made.
- A visitor sheds route plans, pipeline execution and session lifecycle for
  remote networks; multi-network visiting needs one entry route per network.

### Costs

- One more crossing, visitor to entry node, at the start of the request and
  per streamed batch; not per layer or per hidden state. The entry node is
  usually near the workers, which offsets part of it.
- Entry nodes carry every visitor's HTTP/SSE and planning. Small against
  running layers, but concentrated; `admins_and_workers` spreads it.
- Availability depends on entry nodes. Two or more give failover.
- Gateway-side tuning (speculation, prefix cache decisions, remote stream)
  follows the entry node's configuration, not the visitor's.
- Visitor chat telemetry loses per-hop detail under `member_gateway_only`.

### Compatibility

- Contract additions only: a route, a capability, optional policy and record
  fields.
- A released visitor in a `member_gateway` network keeps planning directly.
- In a `member_gateway_only` network a released visitor is refused by the
  workers. The settings screen lists visitors that do not announce
  `gateway/delegate-v1` before the principal switches, and the release notes
  say so.
- Making `member_gateway` the default applies to new networks only.

## Changes by area

### Backend

- `utils/config.rs`: `inference_access`, `entry_nodes`, defaults, tests.
- Policy propagation: the two fields join the principal's network policy.
- `backend/gateway/`: the visitor path (pick entry node, forward, relay the
  stream, cancellation), the delegate handler, `gateway`/`delegated_to`
  record fields, `x_pom.gateway`.
- `backend/openai.rs`: `/v1/models` from the entry node's catalog for a
  visitor under `member_gateway_only`.
- `backend/webui/routes/mod.rs`: scope of `/v1/gateway/delegate`.
- `inference/infer_server/`: refusal `visitor_must_use_gateway` on the
  inference port and tunnel under `member_gateway_only`.
- `network/` discovery: announce and `PeerList` filtering for visitors.
- `pipeline.rs`: the `gateway/delegate-v1` capability constant.

### UI

- **Network settings** (principal): "Inference access" (Direct / Through
  network members / Through network members only), "Entry nodes" (Admins /
  Admins and workers), and the list of visitors that would be cut off.
- **Dashboard and map on a visitor**: entry nodes only, with a note that the
  network hides its workers.
- **Chat on a visitor**: "via <entry node>" and the pipeline summary.
- **Dashboard on an entry node**: delegated requests per visitor.
- Guides: network settings, dashboard, chat, and the concepts guide on roles.

### Documentation

`security-model.md` (new refusal, delegation scope), `http-api.md` (route,
fields), `inference.md` (who plans), `p2p-network.md` (what visitors learn),
`web-ui.md`, and the root `AGENTS.md` invariant on roles.

## Testing

- `gateway::testkit`: visitor forwards and streams; entry node records the
  request; cancellation releases sessions; failover only before delivery;
  `503 no_entry_node`; `409 plan_locally` for a non-visitor caller.
- Policy matrix: `direct`, `member_gateway` with an old visitor (plans
  locally), `member_gateway_only` (worker refuses the visitor).
- Discovery: a visitor under `member_gateway_only` never receives a worker in
  an announce or `PeerList`.
- Live: visitor, admin and two workers in isolated containers, the visitor
  with no route to the workers.

## Open questions

- Should the entry node expose a slim per-hop timing summary to visitors under
  `member_gateway`, or only under `direct`?
- Rate limiting of delegated requests per visitor at the entry node, or leave
  it to the Business Pack quotas?
- Should `admins_and_workers` exclude workers that are hops of the plan, to
  keep the gateway work off busy GPUs?

## Phases

1. Policy fields, delegate route and the visitor forward path under
   `member_gateway`.
2. Entry node selection and failover; records and `x_pom.gateway`.
3. `member_gateway_only`: worker refusal and discovery filtering; the
   visitors-cut-off list.
4. UI and guides.
5. Later, once released visitors are gone: remove remote planning from
   visitors.

## Out of scope

- Entry nodes outside the network (a third-party proxy).
- Cross-network pipelines.
- Per-model entry node pinning.
