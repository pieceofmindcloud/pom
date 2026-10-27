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
| Merge neighboring shards on the same machine | Two adjacent ranges on one machine become one file before loading. | Turn on when a machine got neighboring ranges: it saves a hop. |
| Hidden-state precision | Bytes sent between peers per token. f16 is the default; f32 is lossless and doubles the volume; f8 and groupwise int4 are lossy and need every peer to support them. | Shown only when the pipeline crosses machines. Try f8 on a slow link. |

:::warn Without the memory fit
With Fit to memory off, llama.cpp tries to put the whole slice, the KV cache and the compute buffers in VRAM. When that does not fit, the node fails to start and the supervisor retries in a loop. Off, the Micro-batch no longer applies either: the Batch does.
:::

## Recipes

:::steps
### Fit a large slice
Turn on Fit to memory, drop the Micro-batch to 256 and check the allocation preview. If the slice still does not fit and the model is MoE, see the Beta group right below.
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
### What is the hidden state?
The activation vector one slice hands to the next for every token. It is what crosses the network in the pipeline.
:::

:::deep docs/subsystems/deployment.md
Memory fit, merge and the deployment plan
:::

:::deep docs/subsystems/inference.md
Inference pipeline and hidden-state precision
:::
