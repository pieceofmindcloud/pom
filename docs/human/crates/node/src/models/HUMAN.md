---
id: human-models
label: Models
summary: How a GGUF is found, read and cut into slices
title: Inside the models
reference: crates/node/src/inference/AGENTS.md
order: 40
---

:::lead
This folder knows the model files: it finds the GGUF files in the configured folders, reads their headers without loading them, plans how to cut a model by size and writes the slices. It also remembers which slice was in service, so it can come back after a restart.
:::

:::diagram slicing
A 32-layer GGUF cut by size target, not into equal parts.
:::

:::cards
### Catalog in two passes
First a fast listing that only reads the file system, so the screens show something at once; then the headers are read in the background and the catalog is marked complete. A file that cannot be read is marked, without hiding the others.
### Slices carry their origin
Each slice gets `pom.*` metadata: which range of which model it is. A slice without that metadata does not know the model's total of layers - the total is "unknown", never zero.
### Planning by size
A target in GB per slice becomes a whole number of layers. Intermediate slices repeat the tensors the architecture needs at every stage, and the plan counts that cost.
### Vision and MTP sidecars
Vision projectors and MTP drafts are recognized from their headers and linked to the model they fit, wherever they are in the network.
:::

:::diagram lifecycle
The life of a slice: only the state the node itself confirmed lets it come back after a restart.
:::

:::note Pending until the node confirms
After an apply, the slice is annotated as Pending. Only the `/health` watcher, seeing the model loaded, turns it into Serving - so a node never restarts into a slice it never managed to serve.
:::

:::deep crates/node/src/inference/AGENTS.md
The models folder's notes live here, because models/ is gitignored
:::

:::deep docs/subsystems/slicing.md
GGUF reading, required tensors and size planning
:::
