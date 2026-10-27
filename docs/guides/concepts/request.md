---
id: request
label: The path of a question
summary: From prompt to token, machine by machine
title: The path of a question
order: 40
---

:::lead
Understanding this page is understanding where the cluster's latency comes from, and why the answer arrives at the pace it does.
:::

:::diagram request
The prompt enters through any node; the last machine in the chain answers the asker directly.
:::

:::steps
### The question arrives
Any node accepts it. It picks the path that delivers the answer earliest among the machines covering the model. If a range is missing the request fails with the reason - there is no degraded mode that quietly answers worse.
### The head reads the whole prompt
The machine holding layer 0 tokenizes and processes the prompt in chunks. It is the most expensive step of the request, which is why layer 0 belongs on a machine with memory to spare.
### The hidden state crosses the chain
Each machine runs its layers and hands the result to the next. On a 4096-dimension model that is 8,192 bytes per token per hop, because the vector travels as f16 rather than text.
### The tail picks the token
The last machine in the chain is the only one that samples: it turns the hidden state into the next token.
### The answer goes straight back
The tail delivers the token directly to whoever took the question instead of retracing the chain. When that is not possible, the answer goes back through the ring.
:::

## What it costs

:::stats
- **8 KB** hidden state per token, per hop (f16, 4096 dimensions)
- **16 KB** the same state in f32, lossless
- **~200 B** per sampled token coming back
- **+1 RTT** per extra machine in the chain
:::

:::warn The prompt has a budget, and exceeding it is a refusal
Each session gets the deployment's context window. Increasing sessions multiplies reserved memory; it does not divide that window. The gateway measures the prompt before generating and answers HTTP 413 when there is no room left, instead of taking the process down.
:::

:::note Speculative decoding
When the model allows it, a draft of several tokens is proposed at once and the tail confirms or discards the batch. On a hit, several tokens come out for the price of one crossing; on a miss, the cache rolls back and nothing wrong escapes.
:::

:::faq
### Can fewer bytes travel per token?
Yes. f16 is the default; f32 is lossless and doubles the volume; f8 and i4 are optional lossy formats, negotiated per hop in the deployment.
### What if two machines serve the same range?
They are replicas of the same stage, not two hops. The gateway picks one replica per session and keeps that choice in the following rounds.
### Who decides the path?
For each path that covers the model, the gateway estimates prefill, decode and network time, counting each machine's queue, and picks the one that delivers earliest.
:::

:::deep docs/subsystems/inference.md
Prefill, decode, hidden-state format and speculation
:::

:::deep docs/subsystems/parallelism.md
Pipeline and tensor parallelism: when to use each
:::

:::deep docs/subsystems/http-api.md
Routes, authentication and the proof of distributed execution
:::
