---
id: slice-and-deploy
label: From model to serving cluster
summary: Choose the ranges, get the model onto each machine, put into service
title: From model to serving cluster
order: 50
---

:::lead
These are three separate acts, and it helps to keep them apart: choosing who serves which layers, getting the model onto the right machines, and telling those machines to load their range of it.
:::

:::note First of all, a model
Full models come from the machine's model folders. To download one, use Search Models, the Hugging Face plugin's screen: it shows the license before the download and the progress on its Downloads tab. A model that fits whole on one machine is deployed on that machine alone.
:::

## 1. Choose the ranges (no cutting)

Each machine serves its range of layers straight from its own copy of the whole file, so nothing is cut before a deploy. The deploy screen proposes the cuts on the boundaries the model allows, and a change of cuts later is only a reload. Slicing the file survives only for tensor parallel shards, a development feature (Tensor parallel shards).

## 2. Get the model onto each machine

Files are no longer sent between machines. Each machine that serves a range keeps its own copy of the whole model: download it there (from Search Models, or **Download here** on its card in the deploy screen, which uses the same repository and file another machine downloaded), or copy the file into one of its model folders and **Rescan**. A machine serves its range from that copy; nothing crosses the network before the model starts.

## 3. Deploy (Deploy to network)

Here you choose which machine serves which range and tell the cluster to load. The node suggests a distribution; your choice travels with the request and the cluster is re-scanned at apply time rather than trusting the preview. The map cards show how much memory each machine will use after the deploy.

| Rule | Why |
|---|---|
| Pipeline order is layer order | whoever holds layer 0 receives the full prompt of every request; choosing the order and choosing the ranges are the same act |
| Several ranges on one machine are valid | as long as they do not overlap and global coverage is complete; merge only joins neighboring ranges |
| Overlap is a refusal | the same layer running twice corrupts the answer |
| A gap is a refusal | every required layer must be covered before a pipeline forms |

:::note Context and batch are cluster-wide; GPU is per machine
The context window and batch size govern the whole chain. How many layers go to the GPU and where the attention cache lives are properties of the hardware - a machine with 16 GB of VRAM and one without a GPU cannot both be right about that, so each has its own field.
:::

:::checklist Before clicking deploy
- Every layer is covered, with no gap.
- No range overlaps another.
- The machine holding layer 0 has memory to spare for the prompt and the cache.
- The map cards show no memory warning.
:::

To check, open Deployments: the active version shows the parameters used, and Redeploy this version goes back to any earlier version.

:::faq
### Do I have to slice the model?
No. Each machine serves its range from the whole file. A model that fits whole on one machine is deployed as a whole model, on that one machine. More machines can join the map, as long as their ranges cover every layer.
### Pipeline or tensor parallelism?
Pipeline is the recommended option between machines across the internet: one crossing per layer boundary per token. In tensor parallelism each layer is split and the machines sum results several times per token, so it needs a fast network.
:::

:::deep docs/subsystems/slicing.md
Slicing (tensor parallel shards), type table
:::

:::deep docs/subsystems/deployment.md
Deployment, merge and slice lifecycle
:::

:::deep docs/subsystems/parallelism.md
Pipeline and tensor parallelism
:::

:::deep docs/subsystems/file-transfer.md
The retired transfers between machines
:::
