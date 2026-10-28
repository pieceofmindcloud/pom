---
id: beta-settings
label: Beta settings
summary: MoE experts in RAM, the GPU expert cache and automatic prefill
title: Beta settings
order: 130
---

:::lead
These three settings exist to run a MoE model that does not fit whole in VRAM. They work, but they are still being measured: the best value changes from model to model and machine to machine.
:::

In a MoE (mixture of experts) model, each layer has many experts and each token uses only a few. The experts are most of the weights. That is why they can stay in RAM while VRAM keeps the rest of the model and the experts used recently.

| Property | What it does |
|---|---|
| MoE experts in RAM | Keeps every routed expert in RAM, computed by the CPU (llama.cpp's --cpu-moe), and gives VRAM to the rest of the model. Turns the layer fit off. |
| Expert cache on GPU (MiB) | VRAM that keeps the recently used experts held in RAM: only misses cross PCIe, for batches of up to 32 tokens (generation; prefill keeps the regular offload). 0 turns it off. Per GPU: with two cards, each caches the experts of its own layers. |
| Automatic prefill chunk | The gateway picks each prompt's chunk size, between 32 tokens and the Prefill chunk, from what it measured on the route; at least 64 while the expert cache is on. Off, every prompt goes in chunks of exactly the Prefill chunk. |

:::stats
- **14.95 tok/s** Qwen3.8-Flash-Next UD-IQ3_XXS (78 GB), one 16 GB GPU, all experts in RAM
- **19.26 tok/s** The same, with an 8000 MiB expert cache
:::

## Combining them

| Combination | Experts in RAM | Cache (MiB) | Fit to memory | When |
|---|---|---|---|---|
| Everything in VRAM | off | 0 | on | The model fits in VRAM. The fastest. |
| The fit decides, with a cache | off | 4000 to 8000 | on | The model almost fits. The fit sends to RAM only what does not fit, and the cache serves the recently used experts. If everything fits, the node turns the cache off itself. |
| All experts in RAM, with a cache | on | the largest that fits | (off because of experts in RAM) | The model is much larger than VRAM. VRAM goes to attention, KV, MTP and the cache. |
| All experts in RAM, no cache | on | 0 | (off because of experts in RAM) | Little VRAM. The CPU computes every expert; the slowest. |

:::note How much cache?
Give the cache the VRAM left after the weights, the KV cache, the compute buffers and MTP. If the context does not fit, the node halves the cache and retries, turning it off below 512 MiB. In the load log, the line MoE cache size = X for Y of host experts tells how much of the experts in RAM the cache covers; on unload, the llama_moe_cache lines show the hit rate.
:::

:::warn The cache only holds experts that are in RAM
With no experts in RAM (neither from the fit nor from MoE experts in RAM), the cache has nothing to hold and changes nothing. And with the experts in RAM, MTP can make generation slower: measure before keeping it.
:::

## Step by step

:::steps
### Start from the default
Fit to memory on, MoE experts in RAM off and cache 0. Deploy and check in the preview and the log whether the fit sent experts to RAM.
### Turn the cache on
If it did, set the Expert cache on GPU (somewhere between 4000 and 8000 MiB per GPU) and redeploy. Dropping the Micro-batch to 256 frees VRAM for it.
### Model much larger than VRAM
Turn on MoE experts in RAM and give the cache the largest value that fits.
### Compare
Use the logs telemetry to compare generation and prefill tok/s across versions. History keeps every version with the parameters it loaded with.
:::

:::faq
### Does it work with two GPUs in one machine?
Yes. The cache is per GPU: both cards keep their share of the layers, and each caches the experts of its own layers.
### Why is prefill still slow with the cache?
The cache serves batches of up to 32 tokens, which is generation. Prefill goes through the regular offload of the experts in RAM.
### When should I turn the Automatic prefill chunk off?
To compare measurements with a fixed chunk, or on a route where the automatic choice does not help. Off, the Prefill chunk always applies.
:::

:::deep docs/subsystems/deployment.md
MoE experts in RAM and the GPU expert cache
:::
