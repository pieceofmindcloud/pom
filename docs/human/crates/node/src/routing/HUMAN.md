---
id: human-routing
label: Routing
summary: How the gateway picks the chain of machines for each request
title: Inside the routing
reference: crates/node/src/routing/AGENTS.md
order: 30
---

:::lead
For every request, the gateway has to pick an ordered chain of machines that together cover every layer of the model, from 0 to the last. This folder is the planner that makes that choice - and it picks the chain that delivers the answer earliest, not merely the closest one.
:::

:::diagram topology
Three machines covering a 32-layer model. Whoever takes the question need not be the one holding layer 0.
:::

:::steps
### Collect the candidates
Every live machine announces the layer ranges it serves. A machine may serve several ranges, and several machines may serve the same range (replicas).
### Estimate each path
For each chain that covers `0..total_layers`, the planner adds the prefill cost of each stage, the decode cost of the expected tokens, the queue already waiting at each machine and the network time of each edge.
### Choose with the return mode
The same estimate runs for direct reply and for the ring, and the cheapest pair wins.
### Or refuse
With no chain that covers every layer, the request fails with the reason. There is no fallback to a smaller model or to a local engine.
:::

:::note Order is the layers' order
A pipeline always runs in layer order. An overlap or a global gap between ranges is a refusal, never a guess.
:::

:::deep crates/node/src/routing/AGENTS.md
The planner, file by file
:::

:::deep docs/subsystems/parallelism.md
Choosing the pipeline by delivery time
:::
