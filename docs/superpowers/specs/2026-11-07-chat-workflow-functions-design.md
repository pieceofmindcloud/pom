# ComfyUI workflows as Chat functions

Status: approved in conversation (operator: admins publish; video and audio
outputs too; both the LLM tool and the form in the first version).

## Intent

Any ComfyUI graph an admin publishes becomes a function of the Chat with the
parameters it needs: the LLM can call it as a tool, filling the parameters
from the conversation, and a person can run it from a form in the Chat. Its
outputs (images, video, audio) appear in the conversation and are kept on the
requesting node.

## Publishing (ComfyUI, admins only)

"POM > Publish to Chat" lists every editable input of the graph (widget values
of `graphToPrompt().output`: text, number, boolean, a combo's options, a
LoadImage `image`) with a checkbox. Each checked input becomes a parameter:

```text
{ key, label, description?, node, input, type: text|number|integer|boolean|choice|image,
  default?, min?, max?, options? }
```

`key` is a slug made from the label (unique). The workflow also has a name and
a description (what it does; the LLM reads it). The prompt/negative/image
choices of the earlier dialog become parameters of type text and image, and a
workflow published before keeps working (its `map` is read as parameters).

## The service and the node

The plugin lists, per published workflow, `{id, name, description, params,
outputs}` (never the graph). The node passes `description` and `params` to
`chat/capabilities` (`image_workflows`), validating the shape (at most 24
parameters, slugs, known types, short texts).

`POST /api/ui/chat/image` takes `params: {key: value}` beside the earlier
fields. The plugin coerces each value to its type (clamped to min/max; a
choice must be one of the options; an image is a PNG/JPEG data URL saved under
the input folder), sets it on its node input, runs the graph and streams
progress. The last line is `{"media": [{kind, mime, data, width?, height?}]}`
with every output the graph saved: images (`images`), video (`gifs`, `videos`)
and audio (`audio`). The node stores each file as `<sha256>.<ext>` (png, jpg,
webp, gif, mp4, webm, wav, mp3, flac, ogg, by magic bytes; 32 MiB each, 16
files) and answers `{"media": [{hash, ext, kind, width?, height?}]}` plus the
earlier `images` list for older screens. `GET /api/ui/chat/image/:hash` serves
any of those types.

## The Chat

- **Tools:** in Automatic mode the Chat offers `generate_image` (the built-in
  workflow, as now) and one tool per published workflow with parameters:
  name `wf_<slug>`, its description, and a JSON schema from its parameters
  (an image parameter becomes `use_attached_image: boolean`). A call runs that
  workflow with the arguments.
- **Form:** the Generate image menu lists the published workflows; choosing
  "Run..." on one opens a dialog with a field per parameter (text, number with
  bounds, switch, select, image picker) prefilled with the defaults. Running it
  adds a user turn naming the workflow and its values and a reply with the
  outputs.
- **Outputs:** images as thumbnails (as now), video with a player, audio with a
  player; all open full size with Download.

## The API (operator request: tool calls through the API too)

The same functions serve any OpenAI-compatible client with a key this node
minted (every `/v1` route's rule), so an agent outside the Chat runs the
standard tool-call loop against POM:

- `GET /v1/pom/tools` lists the tool definitions the Chat uses
  (`generate_image` while a built-in workflow serves, `wf_<slug>` per
  published workflow) in the `tools` format of `/v1/chat/completions`; an
  image parameter becomes `image_url: string` (a data URL or an http(s) URL
  is not fetched: data URLs only).
- `POST /v1/pom/tools/call {name, arguments}` runs one call and answers
  `{content, media: [{url, kind, mime, width?, height?}]}` once the run ends:
  `content` is the text to send back as the `tool` message, `url` is
  `/v1/pom/media/<hash>.<ext>` on this node (same key).
- `POST /v1/images/generations` (OpenAI images API: `prompt`, `n`, `size`,
  `response_format` `b64_json` or `url`) runs the built-in workflow.

The gateway does not run tools by itself inside `/v1/chat/completions`: the
client keeps the loop, as with any OpenAI tool.

## Compatibility

Only optional fields and new values: `params`, `description`, `outputs`,
`media`. Older plugins list workflows without params (the form shows only the
prompt) and answer `images`, which the node still reads.

## Out of scope

Converting saved UI-format workflows without opening them in ComfyUI; per-user
publishing; streaming partial outputs.
