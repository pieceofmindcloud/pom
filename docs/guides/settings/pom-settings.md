---
id: pom-settings
label: POM settings
summary: What exists because inference is split across machines
title: POM settings
order: 120
---

:::lead
llama.cpp runs a model on one machine. These settings exist because POM splits the model into slices on different machines, and has to decide how each slice fits in memory and how state passes from one to the next.
:::

| Property | What it does | When to change it |
|---|---|---|
| Disable embeddings | Normally automatic: only the last peer skips hidden-state extraction. Turned on by hand, it stops the first peer from extracting it, and the pipeline stops working. | Almost never. |
| Fit to memory | Before loading, the node reads each GPU's free memory, keeps a safety margin (256 MiB by default) and runs llama.cpp's fit for Context × Sessions. When it does not fit, it lowers the GPU layers or sends part to RAM instead of failing out of VRAM. | Leave it on. GPU layers other than 99, or MoE experts in RAM, turn the layer fit off. |
| Chunks in flight | How many prefill chunks travel the pipeline at once (1 to 8). | Higher hides per-hop latency on a deep pipeline; 1 turns the overlap off. |
| Automatic prefill chunk | The gateway picks each prompt's chunk size, between 32 tokens and the Prefill chunk, from what it measured on the route; at least 64 while the MoE expert cache is on. Off, every prompt goes in chunks of exactly the Prefill chunk. | Leave it on. Turn it off to compare measurements with a fixed chunk, or on a route where the automatic choice does not help. |
| Merge neighboring shards on the same machine | Two adjacent ranges on one machine become one file before loading. | Turn on when a machine got neighboring ranges: it saves a hop. |
| Hidden-state precision | Bytes sent between peers per token. f16 is the default; f32 is lossless and doubles the volume; f8 and groupwise int4 are lossy and need every peer to support them. | Shown only when the pipeline crosses machines. Try f8 on a slow link. |
| Yield the GPU to images | When an image model (ComfyUI) on the same physical GPU needs room and this model has no request in flight (for 10 s), POM unloads it; the next request reloads it, and a running image pauses until this model is quiet again. Off by default. | Turn on for a model that reloads fast and shares a card with an image model. Leave off for a large model (slow to reload) or one in constant use. |
| Visitor price, input / output per 1k | What a visitor of a charging network pays for this model, in credits per 1000 tokens. Saved with the deployment version, so changing it makes a new version and redeploying an old version brings its price back. Empty keeps the network's price; zero and zero makes the model free. | Set it when the network charges visitors (Payments). |

:::warn Without the memory fit
With Fit to memory off, llama.cpp tries to put the whole slice, the KV cache and the compute buffers in VRAM. When that does not fit, the node fails to start and the supervisor retries in a loop. Off, the Micro-batch no longer applies either: the Batch does.
:::

## Recipes

:::steps
### Fit a large slice
Turn on Fit to memory, drop the Micro-batch to 256 and check the allocation preview. If the slice still does not fit and the model is MoE, see MoE models larger than VRAM in Default settings.
### Pipeline over a slow link
Raise Chunks in flight (up to 8) to overlap prefill across hops. If every peer advertises support, switch Hidden-state precision to f8.
### Two neighboring ranges on one machine
Turn on Merge neighboring shards: the machine serves the union from one file, and the token stops crossing the network between the two ranges.
:::

:::faq
### Does Fit to memory make inference slower?
Only when it has to send layers or experts to RAM, because the alternative is a slice that does not start. When everything fits, it changes nothing.
### Where do I see what the fit decided?
In the allocation preview, before deploying, and in the node's load log, on the lines starting with llama.cpp fit.
### Why at least 64 tokens with the expert cache on?
The expert cache serves batches of up to 32 tokens as generation. A 32-token prefill chunk went through it, crossed PCIe for the experts of every chunk and evicted the ones generation keeps hot.
### What is the hidden state?
The activation vector one slice hands to the next for every token. It is what crosses the network in the pipeline.
:::

:::deep docs/subsystems/deployment.md
Memory fit, merge and the deployment plan
:::

:::deep docs/subsystems/inference.md
Inference pipeline and hidden-state precision
:::
