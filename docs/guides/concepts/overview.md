---
id: overview
label: What this platform is
summary: Ordinary machines serving one large model, together
title: What this platform is
order: 10
---

:::lead
A language model that does not fit on one machine is cut into slices of layers, and each machine serves its own. The machines find each other on the network, agree on who takes which range, and start answering as if they were one.
:::

Every node runs the same binary, every node serves this interface and the API, and any of them can take the incoming request. What differs between them is the range of layers they hold in memory and the role each one has in the network.

:::diagram topology
Three machines covering a 32-layer model. Whoever takes the question need not be the one holding layer 0.
:::

## The four pieces

:::cards
### Node
One machine in the cluster. It announces who it is, how much memory it has and which layer range it serves. It finds the others over UDP on the local network and through its iroh endpoint across the internet.
### Slice
A GGUF file holding a range of layers of the original model. Slicing is local: each machine extracts its own range from its own copy, and no weights travel the network because of it.
### Pipeline
The ordered chain of nodes covering layer 0 through the last one. Without full coverage there is no answer - the request fails with the reason instead of answering worse.
### Gateway
The HTTP front door, served by every node: this interface, the OpenAI-compatible API and the generation route.
:::

## Why slice by layers

Transformer layers run in sequence: each one's output is the next one's input. That allows cutting the model at clean points and spreading the pieces, as long as the pieces run in the right order. What crosses the network is neither weights nor text, but the hidden state - one vector per token.

:::note Slice size is the machine's decision, not the model's
Slices are not equal. What decides is the memory of the machine that will hold it, and the machines in a real cluster do not look alike. The slicing screen works from a size target in GB per slice and converts that into a layer range.
:::

If this is your first time here, read the guides in order: getting started, the profiles, the path of a question and, last, from model to serving cluster.

:::deep docs/subsystems/p2p-network.md
Discovery, advertised addresses and node identity
:::

:::deep AGENTS.md
Repository map and project invariants
:::
