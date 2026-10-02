# Pipeline and tensor parallelism

POM distributes inference in two different ways. The choice should follow the
network topology and the reason a single node cannot load the model.

## Pipeline parallelism

In a pipeline, each node holds a complete range of transformer layers. The
planner sorts the ranges by start layer and only accepts a plan that covers the
required layers without overlap. The node serving layer 0 receives the whole
prompt, processes its own range and hands the next node the hidden state, in
f16 by default. A deployment can also pick f32, lossless on the wire, or lossy
groupwise f8/int4 with capability negotiation. The process repeats up to the
output head, which samples the next token.

Weights are never sent during inference. Each machine needs its own copy or its
own slices of the GGUF. What travels is the hidden state, about
`hidden_dim * bytes_per_value` bytes per token per hop, plus the frame headers:
two bytes per value in f16 and four in f32. In decode, the direct reply sends
the tail's token straight to the gateway; in the older ring mode, the result
bubbles back through the chain.

The pipeline is the recommended option between machines on the internet. It
makes one crossing per layer boundary for each token and needs no global
barrier at every internal operation. Node order matters, and the layer 0
machine must have enough memory for the prompt and the KV cache.

### Choosing the pipeline by delivery time

With replicas, the chosen pipeline is the one that **delivers the request
earliest** (`routing/fastest.rs`, `plan_fastest`), not the one with the lowest
network latency. Per request, the gateway estimates for each path that covers
`0..total_layers`:

- **prefill:** new tokens x ms/token of each stage x (1 + queue);
- **decode:** expected tokens (the requested `max_tokens`, between 16 and 512)
  x ms/step of each stage x (1 + queue);
- **network:** (1 + expected tokens) x RTT of each edge, including requester ->
  head and tail -> requester.

The terms are sums per stage and per edge, so the best path comes from dynamic
programming over the ranges. A distant but fast head beats a near, slow one
when it finishes first; with little output requested, the network weighs more
again.

- **Speed** of each peer comes from `/health` (`perf`): a local test of the
  slice right after the model loads (64-token prefill and 8 decode steps,
  synthetic input, no network - `infer_server/perf.rs`) and the moving average
  of real traffic. A peer with no measurement uses the average per-layer cost
  of the measured ones.
- **Network** comes from the pinned routes (`route_plan`): the requester uses
  its own; edges between two other peers come from the source peer's
  `/health` (`route_rtt_ms`, `gateway_rtt_ms`). An unmeasured edge counts as
  100 ms.
- **Balancing:** the stage queue is the larger of the peer's
  `active_generations` (generations in flight from any gateway) and this
  gateway's in-flight load. It multiplies the stage cost: the fastest peer
  with a queue becomes slower than an idle average one, and load spreads
  across the network with no round robin.
- **Prefix:** each path that already holds this prompt's prefix in the
  gateway's cache enters as a candidate, estimated with only the new tokens.
  Reuse wins when it pays off and loses when the cache peer is saturated.
- **Return mode:** chosen together with the path, not afterwards. With direct
  reply, the forward leg pays half an RTT per edge and the return pays half
  the RTT of the tail's pinned route to the gateway (`gateway_rtt_ms` in its
  `/health`); with the ring, the answer goes back over the forward
  connections, the full RTT of each edge. The dynamic programming runs once
  per mode and the lowest total wins. A tail that has measured routes to other
  gateways but none to this one has no way back: only the ring works. So a
  forward leg over QUIC with the return only through the tunnel picks the ring
  from its own measurement. The gateway uses the plan's mode (`ReturnMode`);
  with the fallback greedy planner, the rule `return_path_prefers_ring`
  (missing return, or tunnel-only return with a direct forward leg) decides.
  `POM_RETURN_MODE=direct|ring` forces a mode, for testing: the ring always
  works behind NAT (the answer comes back over connections the node itself
  opened), and so it hides a broken traversal; `direct` exposes it.
- The head chosen in the UI (`inference_head`) is still the only one
  accepted. With no viable path, the old greedy planner decides.

The gateway log states the choice (the line is in Portuguese):
`pipeline por tempo de entrega: A→B ~N ms, resposta anel|direta` ("pipeline by
delivery time ..., reply ring|direct").

### Replicas and scheduler

Two machines announcing the same range are replicas of one stage, not two
consecutive hops of the same request. Each peer keeps its own local scheduler,
`llama.cpp` context and KV cache. The gateway picks one replica per session and
keeps that affinity in the following rounds; the two replicas share no queue
and no global scheduler. Independent requests that reach the same replica can
form a local microbatch.

This local batching does not turn replicas into a TP group. For distributed
pipeline batching, peers would still have to negotiate a multi-item frame and
correlate each hidden state per session. Today, frame negotiation and fallback
are still individual between hops. `RouteAffinity` records the node IDs chosen
per stage, and `BatchRoute::group` only allows grouping items that share the
same next hop and execution contract.

## Tensor parallelism

In TP, every rank loads every layer, but some tensors of each layer are
partitioned. Column-parallel projections produce partial widths; row-parallel
projections consume those widths and must sum the contributions of all ranks.
The sum is an all-reduce, and every rank gets the same result before
continuing the layer.

POM stamps each TP shard with size and rank. The HTTP collective at
`POST /v1/tp/reduce` exchanges the workers' partials during the forward pass;
the primary contributes at the local barrier. The system records bytes, wait
time and percentiles in telemetry. In a 32-layer model with two row-parallel
points per layer, one decode does 64 reduces per token. These reduces cannot
be postponed to the end: the next operation of the layer depends on the summed
result. A single final reduce would change the transformer's math.

The output head stays replicated in the current implementation. Reducing it
would need a partition by vocabulary and an all-gather or logit merge, plus
tokenizer handling. The output norm delivers the full width, so a partition by
input would also be wrong.

## How to choose

| Situation | Recommendation | Reason |
|---|---|---|
| Two machines in different regions or ISPs | Pipeline | Fewer synchronous barriers and less sensitivity to RTT |
| GPUs in the same machine or the same low-latency LAN | TP | Collectives can use fast links and the model stays replicated per layer |
| A model does not fit one machine, but each range fits one node | Pipeline | Distributes weights by layers and keeps traffic to hidden states |
| A single layer does not fit any node | TP or lower quantization | The pipeline does not split a layer; TP reduces tensor width |
| WAN with mandatory TP | Avoid if possible | The cost is about `reduces * RTT` per token, plus transferring the partials |

TP can reduce the weight memory per rank, but it does not turn the network
into a single hidden-state stream. The partial of the row-parallel projections
must cross the network before the next layer can run. Pipeline frame
compression does not remove that dependency; they are different problems.

The scheduler of a TP group is also not a queue shared between processes. The
primary rank must coordinate the same list of items, positions and shapes on
every rank before a collective. Until that batch key is tied to the barrier,
the runtime keeps TP on the one-sequence-per-decode path.

## Telemetry

Pipeline and TP show up in separate blocks. Pipeline shows hops, phase,
backend, transport, bytes and the reply mode. TP shows expected and observed
reduces, total wait, p50/p95, request and response bytes and whether the path
was local, direct or tunnel. A TP wait close to `reduces * RTT` means the
topology, not just the CUDA kernel, is dominating the token rate.

For an operational analysis, compare:

1. `backend` and `engine wait`, which show queueing and local compute;
2. `transport` and bytes, which show the cost of pipeline frames;
3. `tp.wait_ms`, `tp.reduce_count` and the percentiles, which show TP
   barriers;
4. the `pipeline` or `tp` mode recorded for the run, which confirms the path
   actually used.
