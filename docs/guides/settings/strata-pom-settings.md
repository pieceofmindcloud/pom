---
id: strata-pom-settings
label: POM settings for Strata
summary: How the Strata plugin shares a model between machines
title: POM settings for Strata
order: 130
---

:::lead
When a deployment runs on the Strata plugin, the deploy inspector shows a POM settings card with the settings Strata declares. They decide how POM spreads the model and the conversations between machines. The engine's own settings (sampling, reasoning, MTP) are in the engine card above it.
:::

| Property | What it does | When to change it |
|---|---|---|
| Inference role | Shown when the whole model runs on more than one machine, one row per machine. **Prefill and decode** (the default): the machine answers its own conversations and reads its own prompts. **Prefill first**: it does the same, and also reads the long prompts of the other machines while it is free, then hands them over as a session. | Set **Prefill first** on the machine that reads prompts fastest. Leave the others at Prefill and decode. |
| Prefill machine from (tokens) | A turn goes through the prefill machine only when the part of the prompt the answering machine does not already hold has at least this many tokens. Shorter turns are read by the answering machine itself, because sending the session costs about 118 MB whatever the prompt. Empty: 4096. 0: every turn with anything new. | Lower it on a fast link between machines. Raise it when the link is slow. |
| Save the KV cache | Saves the KV cache of each conversation of 8,000 prompt tokens or more to disk, so another machine can restore it instead of reading the prompt again. The folder holds at most 5 GB per machine, and conversations unused for 24 hours are removed. | Turn it on when conversations move between machines. The whole model on several machines keeps sessions on anyway. |
| KV cache format | How Strata stores the KV cache: int8 (the default), q4_0 (half the memory, slightly less precise) or k8v4. Every machine of the deployment uses the same format, so a session saved on one machine restores on another. | Use q4_0 when memory is short. |
| Prefill chunks in flight | Shown when the model is split into slices across machines. Sets how many prefill chunks travel between the stages at once (1 to 8). | Raise it on a deep pipeline. Set 1 to turn the overlap off. |
| Hidden-state precision | Shown when the model is split into slices across machines. Sets the bytes sent between stages per token: f16 (the default), f32 (lossless, twice the volume), f8 or int4 (lossy). | Try f8 on a slow link. |

:::warn You do not choose the shared role
When the whole model goes on two or more machines, POM gives every machine left at Prefill and decode a third role, shared, on its own. That role keeps Strata's sessions on, which is how the machines hand conversations to each other. On the Dashboard those machines show the **Prefill/Decode** badge, and the prefill machine shows **Prefill first**.
:::

## Recipes

:::steps
### Two equal machines
Put the whole model on both and leave both at Prefill and decode. Each machine answers the conversations the gateway sends it, and a conversation goes back to the machine that holds it.
### One machine reads prompts faster
Set Prefill first on that machine. While it is free, it reads the long prompts of the others. The other machines then only decode.
### A model in slices plus a prefill machine
Deploy the slices and add the whole model on another machine with Prefill first. That machine answers requests on its own beside the pipeline.
:::

:::faq
### What happened to Decode first?
It did the same as Prefill and decode: every machine that is not a prefill machine already takes the prompts a prefill machine reads for it. It was removed.
### Does a Prefill first machine also answer?
Yes. Every role answers requests. Prefill first only adds reading prompts for the other machines.
### Does Qwen3.6-35B-A3B run on several machines?
Yes. Each machine reads its own prompts, because that engine has no batch slots to hand a prompt over. A prefill machine needs Qwen3.8-Flash-Next.
### Why does a short turn skip the prefill machine?
Strata's session carries about 118 MB of recurrent state, whatever the prompt size. For a short prompt, reading it again is faster than sending the session.
:::

:::deep docs/subsystems/inference.md
Plugin inference services and the prefill/decode split
:::
