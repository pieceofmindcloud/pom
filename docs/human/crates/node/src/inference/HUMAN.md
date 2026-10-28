---
id: human-inference
label: Inference
summary: How a question crosses the machines and comes back as tokens
title: Inside the inference
reference: crates/node/src/inference/AGENTS.md
order: 10
---

:::lead
Every node runs a small inference server on port 8181. It loads only its own slice of the model, receives the work of the previous machine, runs its layers and hands the result to the next one. This folder is that server, and the way the pieces talk to each other.
:::

:::diagram request
The prompt enters through any node; the last machine in the chain answers the asker directly.
:::

## Two phases of one answer

:::steps
### Prefill
The machine with layer 0 reads the whole prompt at once and fills its KV cache. The result of its layers - the hidden state, not text - goes to the next machine, which does the same with its own layers.
### Decode
Then one token at a time: each new token crosses every machine of the chain again, but now it is a single row of numbers per hop, so it is fast.
### The answer comes back
The last machine samples the next token. In direct reply it sends the token straight to the gateway that asked; in the older ring mode, the result bubbles back through the chain.
:::

## What travels between machines

:::cards
### Hidden state, never weights
About `hidden_dim × 2` bytes per token per hop in f16, the default. f32 is lossless and doubles the size; f8 and int4 are lossy and negotiated per hop.
### One session, two sequences
Each conversation keeps the prompt in one KV sequence and generates in a copy of it. The end of the turn erases the copy whole, so the prompt stays ready for the next turn - that is the prefix cache.
### Idle and wake up
With "unload if idle", a machine that has not worked for a while unloads its model but stays in the network as `idle`. The next request reloads it, and wakes the other idle machines of the plan in parallel.
:::

:::note Why it is built this way
Machines over the internet are far apart in time. A pipeline crosses each layer boundary once per token and needs no barrier inside each operation, which is why it is the default between homes - and why the closer the machines, the faster the answer.
:::

:::deep crates/node/src/inference/AGENTS.md
Every file of the inference server, function by function
:::

:::deep docs/subsystems/inference.md
The full inference path: prefill, step, wire, caches
:::
