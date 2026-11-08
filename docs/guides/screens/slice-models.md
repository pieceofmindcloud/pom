---
id: screen-slice-models
label: Tensor parallel shards
summary: Cut one tensor shard per rank from a whole model (development mode)
title: Tensor parallel shards
route: /models/slice
order: 70
---

:::lead
A pipeline deploy no longer needs slices: each machine serves its range straight from its own copy of the whole model. This screen is left for one job only, cutting the shards of a tensor parallel deploy, and it works only on a node started in development mode (`POM_DEV_MODE=1`). It is reached from the **Slice** button of a model on the Models screen, which shows only in that mode.
:::

:::steps
### Choose the source model
Only whole models can be cut. A model split into several physical parts must have every part present; a missing part is named and the start stays disabled.
### Choose the rank count
One shard per rank, from 2 to 64. Every rank covers all the layers and gets a different part of each tensor.
### Read the preview
One row per rank, labeled `1of2`, `2of2` and so on, with any warning the node raises.
### Slice now
The job shows its phase and the bytes read while it runs, and stays in the list when it ends or fails.
:::

:::note Shards made earlier
Pipeline slices already on a machine keep serving until they are undeployed, and the deploy screen shows them as a legacy deployment. There is no screen that lists them any more.
:::

:::deep docs/subsystems/slicing.md
Required tensors and tensor parallel shards
:::
