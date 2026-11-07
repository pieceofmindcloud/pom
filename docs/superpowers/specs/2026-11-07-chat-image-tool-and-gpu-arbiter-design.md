# Chat image tool and the GPU arbiter

Status: design, awaiting the operator's approval. Follows
`2026-11-07-image-pipelines-deploy-dashboard-chat-design.md` (delivered).

## 1. Intent

Two changes the operator asked for after using image generation in the Chat:

- **The Chat LLM decides when to make an image.** Instead of the person
  switching Generate image on, the LLM answering the conversation gets a
  `generate_image` tool. When the person asks for a picture, the LLM calls it
  with a diffusion prompt it writes from the whole conversation (a better,
  detailed prompt, in the language diffusion models read best), and the image
  appears in the reply. A second inference request (a JEV decision) was
  rejected: the tool rides on the request the Chat already makes.
- **A GPU arbiter that prefers the LLM.** When one physical GPU serves both an
  LLM and image components, the node coordinates them: an image may take the
  GPU only while the LLM is idle, and an LLM request takes it back at once.

## 2. Decisions

| Question | Decision |
|---|---|
| How the image intent is detected | A `generate_image` tool in the Chat's own request; no second request (operator). |
| Prompt | Written by the LLM from the conversation, passed in the tool's arguments (operator). |
| The Generate image menu | Three modes: **Automatic** (default: the tool is offered), **Text only** (no tool), **Always image** (today's forced mode). The menu still picks the workflow and size the tool uses (operator). |
| Image arrives, LLM holds the GPU | If the LLM has no request in flight, unload it, run the image, let it come back on its next request. With a request in flight the image waits (operator). |
| LLM request arrives during an image | The image pauses between two of its network calls, ComfyUI frees the GPU, the LLM loads and answers; the image resumes afterwards (operator). |
| Engines | llama.cpp and Strata; an engine whose reload is long (Strata) only yields when its deployment allows it, off by default (operator). |

## 3. The image tool

### 3.1 What the Chat sends

In Automatic mode, when `chat/capabilities` lists at least one workflow, the
Chat adds to its `/v1/chat/completions` request:

```json
{"type": "function", "function": {
  "name": "generate_image",
  "description": "Create an image for the user. Call it when the user asks for a picture, drawing, photo, illustration or an edit of an attached image. Write `prompt` as a detailed visual description in English that a diffusion model can follow: subject, setting, style, lighting, composition. Use the conversation for context.",
  "parameters": {"type": "object", "properties": {
    "prompt": {"type": "string"},
    "negative_prompt": {"type": "string"},
    "aspect": {"type": "string", "enum": ["square", "landscape", "portrait", "wide"]},
    "use_attached_image": {"type": "boolean"}
  }, "required": ["prompt"]}}}
```

POM's `/v1` routes already parse tools and stream tool calls in fragments
(`tool_call.rs`, `openai.rs`); nothing changes on the node for the tool.

### 3.2 What the Chat does with the call

- `lib/chatStream.ts` keeps `tool_calls` from the stream (today it drops
  them). When the answer ends with a `generate_image` call, the Chat runs the
  chosen workflow (`/api/ui/chat/image`) with the call's `prompt`, `negative`
  and a size from `aspect` (the menu's size wins when one is set), and the
  attached image as the base when `use_attached_image` is true and the
  workflow takes one.
- The reply bubble keeps the LLM's text (often a short sentence before the
  call), then the pieces animation with the step, then the thumbnails. A small
  "Prompt" disclosure under the images shows the prompt the LLM wrote.
- An attached image goes to the LLM only if it has vision; otherwise the
  message tells the LLM in text that an image is attached, so it can still
  call the tool with `use_attached_image`.
- The tool result is not sent back to the LLM in this phase (no second
  round): the conversation history records the call and a short text
  ("image generated") so the next turn has context.
- Text only: no tool. Always image: today's path (the message goes straight to
  the workflow), and the LLM is not asked.

### 3.3 Limits

- A model that does not call tools well simply answers in text; Always image
  remains for that case.
- At most one image call per answer is run; more are ignored with a note.

## 4. The GPU arbiter

### 4.1 Who shares a GPU

The arbiter acts only between an LLM runtime and image components that use the
**same physical GPU**. On the operator's machines several nodes (containers)
share one card, so "same GPU" is decided by the card's identity (NVIDIA UUID,
or PCI bus id for AMD), which each node reads for its visible GPUs and
publishes in `/api/ui/node` (`system.gpus[].uuid`, optional). Two nodes on the
same host that report the same identity are one GPU group. A node whose GPUs
report no identity is never arbitrated (today's behaviour).

### 4.2 States of a GPU group

- **llm**: the LLM is loaded; image ops wait until the LLM is idle.
- **image**: the LLM yielded (unloaded); image ops run.
- **reclaiming**: an LLM request arrived; new image ops are held, ComfyUI is
  asked to free the GPU (`component.yield`: `unload_all_models` +
  `soft_empty_cache`), then the LLM reloads through its existing idle wake.

### 4.3 Image op arrives (on the node serving the component)

1. The node finds the GPU group of the component's GPU and every LLM runtime in
   it (its own and peers' on the same host, through a signed peer call).
2. If no LLM shares the GPU, or the free VRAM already fits the component (its
   file size plus a margin), the op runs (no swap).
3. Else, if every LLM in the group has no request in flight and its
   deployment allows yielding, the node asks each to yield
   (`unload_for_idle` without the idle timer: the same unload, the peer keeps
   announcing `idle`, the next request reloads it) and runs the op.
4. Else the op waits (bounded by the component call timeout) and retries step 2
   when an LLM's in-flight count drops to zero.

### 4.4 LLM request arrives (on a node whose LLM yielded)

The existing idle wake already reloads the model on the first request. Before
reloading, the runtime asks the arbiter to **reclaim**: image ops on that GPU
are held at their next call (the diffusion stage calls once per sampling step,
so an image pauses within a step), the serving node asks its plugin
`component.yield`, and the LLM loads. When the LLM's in-flight count returns to
zero and stays there for a grace period (default 10 s), held image ops resume
(ComfyUI reloads its weights on the next op).

### 4.5 Deployment settings

`RuntimeParams.yield_gpu_to_images: bool`, default true for llama.cpp
deployments and false for a plugin inference service (Strata), editable in the
deploy inspector's POM settings. A deployment that does not yield is never
unloaded for an image; images then wait for its idle unload or run in the VRAM
left.

### 4.6 Observability

The Dashboard machine card shows "GPU lent to images" while an LLM yielded;
the request log's image telemetry records the wait (`gpu_wait_ms`) and whether
a swap happened.

## 5. Compatibility

- Tools: the Chat only adds a tool when capabilities list workflows; older
  nodes list none and nothing changes.
- New optional fields: `system.gpus[].uuid`, `RuntimeParams.yield_gpu_to_images`
  (absent = the defaults above), image telemetry `gpu_wait_ms`.
- New plugin op `component.yield`; an older plugin answers unknown, and the
  arbiter then only waits for its 60 s idle unload.

## 6. Tests

- Chat: tool offered only in Automatic with workflows; a streamed
  `generate_image` call runs the workflow with the LLM's prompt and aspect;
  menu size wins; attached image passed only with `use_attached_image`; Text
  only sends no tool; Always image unchanged; prompt disclosure.
- Arbiter: group by GPU identity; no swap when VRAM fits; swap when the LLM is
  idle and allows it; wait while a request is in flight; reclaim holds image
  ops between calls and resumes after the grace; a non-yielding deployment is
  never unloaded; nodes without GPU identity untouched.
- Live: Z-Image and a llama.cpp model on one 16 GB card; an image while the
  chat is idle swaps; a chat message during an image pauses it and answers
  first.

## 7. Out of scope

Text encoders taken from a served LLM, diffusion slicing, sending the
generated image back to the LLM for a second round.
