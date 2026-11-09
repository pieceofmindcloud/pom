---
id: screen-models
label: Models
summary: The whole GGUF files found on this machine
title: Models
route: /models
order: 60
---

:::lead
Models lists every whole GGUF file this machine found in its model folders: an Ollama store, an LM Studio library or a plain folder of GGUF files. From here a model is deleted (or, in development mode, cut into tensor parallel shards). To download one, use Search Models, the Hugging Face plugin's screen. The **Image** tab lists the image model files of the same folders (diffusion models, text encoders, VAEs and the rest of what ComfyUI loads).
:::

![Models: one row per model, with its source, layers, architecture, vision projector, size and actions.](../images/en/models.webp)

:::video https://www.youtube.com/watch?v=lCYW3TJ70M8
Video: the Models screens - full models, slices and transfers - explained.
:::

:::cards
### Layers and architecture
Read from the GGUF header, without loading the model. The number of layers is what a deploy divides into ranges.
### Vision (mmproj)
The vision projector that belongs to the model, when there is one. **Vision and MTP** links a projector or an MTP draft found anywhere in the network to this model.
### Slice
Only in development mode: opens Tensor parallel shards with this file chosen. A pipeline deploy needs no slicing.
### Image models
The **Image** tab lists the `.safetensors` files of the same folders with what each one is: diffusion model, text encoder, VAE, checkpoint, LoRA, ControlNet, CLIP Vision or upscaler, read from the file's own header, never from its name. Family and data type come from the same header. These files are deleted here; they are not sliced.
:::

:::note Where these files come from
The folders scanned are set in Settings > Storage. **Scan again** reads them once more after you copy a file by hand.
:::

:::note Slicing is local
Each machine extracts its own range from its own copy of the GGUF. No weights travel over the network because of slicing.
:::

:::deep docs/subsystems/slicing.md
How a GGUF is read and cut
:::
