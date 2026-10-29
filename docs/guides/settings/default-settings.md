---
id: default-settings
label: Default settings
summary: Context, batch, MoE experts, sampling, KV cache and speculation
title: Default deployment settings
order: 110
---

:::lead
These are the settings llama.cpp already has. POM only carries them to every machine of the deployment. They decide how much memory each slice reserves, how fast the prompt goes in and how the model picks each token.
:::

## Memory and capacity

| Property | What it does | When to change it |
|---|---|---|
| Context | Attention window of each session, in tokens. After the weights, it is the largest share of KV-cache VRAM. | Raise it for long chats and prompts. Lower it only when the deployment cannot fit any other way. |
| Sessions | Generations served at once. Each one reserves the whole context, so the KV cache costs Context × Sessions. | Raise it to serve more people at once. Each extra session costs a whole context. |
| Max. tokens | Ceiling of one leg of the gateway's continuation loop, not of the request. The client's max_tokens bounds the response. | Rarely. |
| Batch | Prompt tokens llama.cpp scores in one prefill pass. Larger fills the GPU better on long prompts and raises the compute buffer. It does not change generation speed. | Lower it when VRAM is short. Raise it when there is room and prompts are long. |
| Micro-batch | How many tokens the context decodes at once while the memory fit is on. The compute buffers in VRAM are sized for it. Never more than the Batch; with the fit off, the Batch applies. | Drop it from 512 to 256 to free VRAM for weights, MTP or the expert cache. Prefill gets a little slower. |
| Prefill chunk | Prompt tokens sent through the pipeline per round trip during prefill. | Smaller starts streaming sooner on a slow link. Larger cuts round trips. |
| K cache / V cache | KV cache type llama.cpp allocates (f16, q8_0, q4_0...). Quantized takes less VRAM. It applies to every context the node loads next. | q8_0 or q4_0 for large contexts. |

:::note The memory math
Each machine needs: the slice's weights, the KV cache (Context × Sessions, in the K/V cache type) and the compute buffers (which grow with the Micro-batch). Two 32k sessions cost the same as one 64k session.
:::

## MoE models larger than VRAM

In a MoE (mixture of experts) model, each layer has many experts and each token uses only a few. The experts are most of the weights, so they can stay in RAM while VRAM keeps the rest of the model and the experts used recently. Both settings are llama.cpp's own.

| Property | What it does |
|---|---|
| MoE experts in RAM | Keeps every routed expert in RAM, computed by the CPU (llama.cpp's --cpu-moe), and gives VRAM to the rest of the model. Turns the layer fit off. |
| Expert cache on GPU (MiB) | VRAM that keeps the recently used experts held in RAM (llama.cpp's --moe-cache-mib): only misses cross PCIe, for batches of up to 32 tokens (generation; prefill keeps the regular offload). 0 turns it off. It is the node's total: with two GPUs it is split between them like the layers, and llama.cpp then turns its pipeline parallelism between them off. |

:::stats
- **14.95 tok/s** Qwen3.8-Flash-Next UD-IQ3_XXS (78 GB), one 16 GB GPU, all experts in RAM
- **19.26 tok/s** The same, with an 8000 MiB expert cache
:::

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
With no experts in RAM (neither from the fit nor from MoE experts in RAM), the cache has nothing to hold and changes nothing. Start from the default (fit on, experts in RAM off, cache 0), check in the preview and the log whether the fit sent experts to RAM, and only then set the cache. Dropping the Micro-batch to 256 frees VRAM for it.
:::

## Sampling

Only the last peer of the pipeline samples. The order is llama.cpp's: first the candidate cuts, then the repetition penalties, then DRY. The seed comes last.

| Property | What it does | Starting point |
|---|---|---|
| Temperature | Softens or sharpens the distribution before sampling. 0 always takes the most likely token; above 1 spreads probability. | 0.6 to 1.0, per model. |
| Top-k | Keeps only the K most likely tokens. Applied before top-p. | 0 disables it. |
| Top-p | Keeps the smallest set whose probabilities sum to this, and renormalizes. | 1 disables it. |
| Min-p | Drops a candidate below this fraction of the most likely one, after top-k and top-p. | 0 is off; 0.05 is common. |
| Penalty | Divides the logit of tokens seen in the recent window, against loops. | 1 is no penalty; above ~1.3 can distort the text. |
| Repeat window | How many recent tokens the Penalty looks at. | 0 disables the window. |
| Presence | Subtracted once from the logit of every token already seen in the window, pushing toward new words. | 0 is off; Qwen recommends up to 1.5 against endless repetition. |
| Frequency | Subtracted once per appearance: the more a word repeats, the less likely it gets. | 0 is off. |
| DRY multiplier | Turns DRY on, which punishes continuing a sequence already seen (not single words). The other DRY numbers appear only while it is on. | 0 is off; 0.8 is usual. |
| DRY base | How fast the penalty grows with each extra token of the repeated sequence. | 1.75. |
| DRY allowed length | Repeated sequences shorter than this are not punished. | 2. |
| DRY window | How many recent tokens DRY searches. | 0 searches the whole history, up to 8192 tokens. |
| Seed | Where the random generator starts when the deployment loads. The same seed, model and requests in the same order give the same answers. | POM's default. |
| Reasoning budget | How many tokens a thinking block may take; at the limit, the end of thinking is forced. | Empty is no limit; 0 closes the block at once. |
| Disable thinking | The model answers directly, without the preamble between tags. | Turn on when only the answer matters. |

## Speculation

Speculation drafts the next tokens and checks them all in one batch. When the draft is right, one round through the pipeline yields several tokens instead of one. Only one mode runs at a time.

| Property | What it does |
|---|---|
| Speculation | None; MTP (draft), which drafts with the model's own trained head (nextn); or Speculative (lookup), which repeats text already in the context and only pays off on repetitive output. |
| MTP draft tokens | How many tokens the nextn head guesses per round. More widens the batch on every hop; fewer wastes less when a guess is rejected. |
| MTP minimum probability | Confidence floor for continuing the draft. Lower accepts more guesses, weak ones included; 0 accepts all. |
| Where MTP runs | Automatic: VRAM only if the whole model still fits with it. VRAM: MTP takes its share first and the model uses the rest. RAM: MTP runs on the CPU and spends no VRAM. |
| MTP draft (tail) | Shown when the model has no nextn head: pick the external draft (mtp-*.gguf) the last machine loads. |
| Draft width / Lookup window | Lookup only: how many guesses per batch, and the longest n-gram searched for a prior repeat. |

:::warn Measure MTP on a MoE model with experts in RAM
The verification round processes several tokens at once. With the experts in RAM it fetches many more experts than a single token does, and generation can end up slower than without speculation. Compare tok/s in the telemetry before keeping it.
:::

## Changing a property

:::steps
### Open the inspector
In Deploy, pick the model and the machines. The inspector shows the values the deployment will load with. The [i] icon left of each name explains the field.
### Change the value
Type the number or tick the box. A field that depends on another (the DRY numbers, the MTP ones) appears once the other turns it on.
### Check the allocation preview
It redoes each machine's memory math with the new values. If a machine goes over, adjust before deploying.
### Deploy or redeploy
For a deployment already running, use Redeploy in History: the new version loads with the changed parameters and History keeps the previous one.
:::

:::faq
### The deployment does not start for lack of VRAM. What do I change first?
Turn on Fit to memory, under POM settings. Then drop the Micro-batch to 256 and use a quantized K/V cache. Lower Context or Sessions only as a last resort.
### The client's max_tokens or Max. tokens: which wins?
The client's max_tokens bounds the response. Max. tokens only splits generation into legs inside the gateway.
### Does changing sampling require reloading the model?
The values travel with the deployment, so the change goes into a new version (Redeploy).
:::

:::deep docs/subsystems/deployment.md
Deployment, memory fit and slice lifecycle
:::

:::deep docs/subsystems/deployment.md
MoE experts in RAM and the GPU expert cache
:::

:::deep docs/subsystems/inference.md
Inference pipeline, prefill and speculation
:::
