# routing/ - the inference pipeline planner

Single file for now. It decides an ordered chain of peers that covers
`0..total_layers` through executable sub-ranges, measures the load in flight and
the input RTT of each peer, and picks the address per hop. It is the highest
level entry consumed by `backend/gateway/` (`generate_stream_with_stops`).

## Files

- `pipeline.rs` - `LoadTracker`/`LoadGuard` (RAII count of requests in flight per
  peer), `PipelineStep`/`PipelinePlan`/`PipelineEdge`, `plan_pipeline`
  (greedy) and `resolve_step_url` (address per hop, per session). Every
  `PipelineStep` keeps the HTTP candidates and the `host:port` destinations of
  the hop's announced UDP QUIC port; the transports try that list with expiring
  failure memory instead of treating `addr` as permanent truth. The QUIC
  destination is never derived from `inference_url`.

- `fastest.rs` - planner by delivery time (`plan_fastest`), used by the gateway
  before the greedy one: dynamic programming over the ranges that sums the cost
  of each stage (measured speed x queue) and of each edge (RTT of the fixed
  routes). `PeerPerf::from_health` reads `perf`, `active_generations`,
  `route_rtt_ms` and `gateway_rtt_ms` from `/health`; `estimate_chain` estimates
  a given path (the one of a cached prefix, discounting the tokens already
  retained); `describe_chain` describes a path for the Panel from the same
  sources, with no guessing (an unmeasured leg stays `None`), returning legs,
  return mode, network per token and compute; `resolve_chain` validates a chain
  of `node_id`s against the live candidates. The return mode (`ReturnMode`:
  direct reply or ring) is chosen together with the path - the dynamic
  programming runs per mode -, and `return_path_prefers_ring` is the fallback
  rule for the greedy planner. See `docs/subsystems/parallelism.md`, "Choosing
  the pipeline by delivery time".

## Key functions

- `step_for`/`plan_from_steps` - assembly of one step and of the plan with its
  edges, shared by the greedy planner and `fastest.rs`.

- `plan_pipeline_with_rtt` - sweeps from 0 to `total_layers`; at each position it
  chooses only a peer whose loaded range starts exactly at that position. That
  guarantees `exec_start` never asks for input in the middle of a slice. The
  score is `executed_layers * 100_000 - rtt_penalty - load * 10`, so the RTT
  chooses between equivalent branches while coverage stays dominant. Final ties
  use `node_id` to keep the plan deterministic. **Excludes tensor parallelism
  workers** (`PeerInfo::is_tp_worker`): only a group's primary enters the chain.
  Returns `None` at the first executable hole.
- `plan_pipeline_with_rtt_from_head` - the same coverage and the same choice for
  the following stages, but it honors an explicit `node_id` for the L0 step when
  that node is a live head of the model. An invalid choice fails closed, without
  falling back to the score, and `PipelinePlan::hop_chain` then proves that head
  in the telemetry.
- `plan_pipeline` - calls the variant above with no RTT measurements.
- `model_pipeline_candidates` and `model_head_candidates` - the first applies the
  model, deployment and liveness filter, and leaves out a replica still loading
  (`PeerInfo::is_warming`: not idle, no `runtime_capacity` yet) when a ready peer
  serves the same range, so a request is not held by it; the second keeps only the ranges that
  start at L0, for the limited view of the infer profile. A mixed deployment can
  therefore expose the head of the whole GGUF and the head of slices at the same
  time. Neither one publishes the full topology.
- `PipelineStep::exec_start`/`exec_end` - the range the hop executes, distinct
  from the range it loaded; a `PipelineEdge` exists only when those sub-ranges
  are contiguous.
- `LoadTracker::acquire` - increments the counter and returns a `LoadGuard`,
  which decrements on drop (RAII): the load balance never gets stuck on an error.
- `PipelineStep::frame_encoding` - returns the `F16` marker; every byte passes
  through the initial frame `inference/frame-v1-batch`.
- `CAP_FRAME_BATCH` - capability `inference/frame-v1-batch`; the relay sends only
  after the peer announces the loaded server. There is no rolling upgrade for
  earlier formats in this first version of the product.
- `CAP_LINK_STREAM` - capability `transport/link-v1`, checked **per hop** like the
  tunnel's, not all-or-nothing: the caller opens the persistent channel when the
  next hop announces it, and any transport failure falls back to the usual ladder
  without ending the generation. See `inference/link.rs`.
- `CAP_FRAME_DTYPE_F8` and `PipelinePlan::validate_wire_dtype` - capability
  `inference/frame-v1-dtype-f8` and an all-or-nothing validation of f8 precision
  across the whole plan; a missing one blocks the deploy or the request with the
  peer's reason, instead of letting bytes be read as another dtype.
  `CAP_FRAME_DTYPE_I4G64` and `CAP_FRAME_DTYPE_I4G128` apply the same rule to the
  groupwise int4 formats. f16 stays the default, and f32 is the lossless option
  of four bytes per value; both are base formats that need no capability.
- `CAP_REMOTE_TOKEN_STREAM` - capability announced by the inference server so a
  remote head keeps the decode loop on the peer and delivers tokens to the
  gateway in callbacks bounded by opportunistic batches. The gateway also requires
  a callback address; the local shortcut never uses this transport.
- `CAP_RANGE_LOAD` - capability `model/range-load-v1`, announced (node and gateway
  state) unless `POM_VIRTUAL_SLICES=0`: the node serves a range of a whole model it holds
  through a header-only virtual slice, so the deploy planner charges it no disk for
  the range. Not an inference frame capability; the planner only reads it.
- `PipelinePlan::supports_relay_chain` / `supports_direct_reply` /
  `supports_speculation` - all-or-nothing: one step without the capability drops
  the feature for the whole plan (`CAP_RELAY_CHAIN`, `CAP_RELAY_DIRECT`,
  `CAP_SPECULATE_LOOKUP`), rather than mixing modes in the middle of the chain.
- `PipelinePlan::supports_mtp` - the exception to the all-or-nothing rule, on
  purpose: the **tail** needs `CAP_SPECULATE_MTP` (it is the one holding the
  `nextn` block, at the model's last layer, and the one drafting - the draft
  leaves in the direct reply itself, `next_draft`), **all** of them need
  `CAP_SPECULATE_LOOKUP` and `CAP_SPECULATE_MTP_SLICED` (the new protocol -
  `tokens`/`mtp_k`/`next_draft` on the wire - only exists in a new build; an old
  peer in the ring would receive a `speculate` it does not understand). See
  the engine's `mtp` module.
- `supports_direct_reply` - requires **every** step to have `CAP_RELAY_DIRECT`
  (`relay/v2-direct`, strictly stronger than `relay/v1-chain`: whoever announces
  one announces the other) **and** at least 2 hops - a single-hop plan is already
  the minimum, and diverting the reply would only add a callback to the same two
  legs. It is what lets the gateway dispatch each frame **once** (the direct mode
  of `inference/direct.rs`) instead of bubbling it around the ring.
- `resolve_step_url` - order: 1) a route measured in the `RouteTable` (if it is
  still a candidate), 2) `inference_url` (the observed address, the only one
  proven before the first probe), 3) the first candidate. It pins the choice per
  session - the KV cache on the other side is per session, and switching address
  halfway is worse than the slow route. The opening uses
  `PipelineStep.inference_candidates` as a fallback when the chosen address fails
  before the session starts.
- `routing_request_from_peers` - translates `PeerInfo` into the `RoutingRequest`
  of `routing-contract` (the future planner route; production still uses
  `plan_pipeline` until the equivalence coverage exists).
- `PeerInfo::runtime_capacity` does not take part in the greedy layer choice: the
  gateway aggregates that capacity before validating the request, keeping the per
  leg `max_new_tokens` separate from the total budget derived from the context.
- `PipelinePlan::hop_chain` - "abc123→def456", the proof of distributed execution
  the gateway puts in `source`.

## Links

- **Called by**: `gateway/budget.rs::generate_stream_with_stops` (a plan per request) and
  `backend/webui/`.
- **Calls**: `network::PeerInfo` and `network::route::RouteTable`
  (`resolve_step_url`).
- **Consumed by**: `inference/relay.rs` and `inference/infer_server/` (the
  capabilities `CAP_REMOTE_TOKEN_STREAM`/`CAP_RELAY_CHAIN`/`CAP_RELAY_DIRECT`/
  `CAP_SPECULATE_LOOKUP`/
  `CAP_SPECULATE_MTP`/`CAP_SPECULATE_MTP_SLICED` travel in `next_hops` and in the
  wire format); `inference/direct.rs` (the direct mode depends on
  `supports_direct_reply`); the engine's `wire` module (tensor dtype/encoding).
- `network.rs::self_announce` asks for sync with the capability constants here.

## Defects noted

None.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.