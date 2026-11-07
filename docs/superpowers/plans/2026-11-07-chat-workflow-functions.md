# ComfyUI workflows as Chat functions - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Published ComfyUI graphs become Chat functions with typed parameters, callable by the LLM (one tool each) and from a form, with image, video and audio outputs.

**Spec:** `docs/superpowers/specs/2026-11-07-chat-workflow-functions-design.md`

## Global Constraints
- Parameter types: `text|number|integer|boolean|choice|image`; at most 24 per workflow; `key` lowercase slug.
- Media kinds by magic bytes: image (png, jpg, webp, gif), video (mp4, webm), audio (wav, mp3, flac, ogg); 32 MiB each, 16 per run.
- Earlier forms keep working: `map` read as parameters, `images` line still read and still answered.

## Review Focus
- A choice value outside its options is refused (plugin test).
- A number outside min/max is clamped (plugin test).
- A video or audio output is stored with the right extension and served with its type (node test).
- An old published workflow (prompt map only) still runs (plugin test).
- Tool names never collide with `generate_image` (UI test).
- A tool call through the API with an unknown name, or arguments that are not an object, is a 400, never a run (node test).

### Task 1: Plugin - parameters and media outputs
**Files (pom-plugin-comfyui):** `runtime/pom_workflows.py` (`publish(home, name, graph, params, description)`, `params_of(record)`, `apply_params(graph, params, values, save_image)`, media collection from history outputs), `src/components.rs` (service lists `description`, `params`), `runtime/web/pom.js` (publish dialog with per-input checkboxes), tests.
- [ ] Tests: publish with typed params; old map read as params; coercion (clamp, choice refusal, boolean); media collection (images, gifs/videos, audio). Implement; suites PASS; commit, push.

### Task 2: Node - params and media
**Files:** `plugin_services.rs` (`ImageWorkflow.description`, `params`), `chat_image.rs` (`RunRequest.params`, `Workflow.params/description`, `store_media`, media relay, typed GET), tests.
- [ ] Tests: params parsed and bounded; media stored by magic with kind; GET serves video/audio types; old `images` line still stored. Implement; PASS; commit.

### Task 3: Chat - tools per workflow, form, players
**Files:** `lib/chatImageTool.ts` (`workflowTool(w)`, `argsToParams`), `lib/chatImage.ts` (`params`, `media`), `lib/types.ts`, `routes/Chat.tsx` (tools list, call dispatch, Run dialog), `components/WorkflowForm.tsx`, `components/ChatImageGrid.tsx` (video/audio), i18n, guides.
- [ ] Tests: tool schema from params; dispatch of `wf_<slug>` calls; form fields per type and submit values; video/audio players. Implement; PASS; commit; docs; push.

### Task 4: API - tools, calls and OpenAI images
**Files:** `crates/node/src/chat_tools.rs` (new: `tool_definitions(workflows)`, `call_to_request(workflows, name, args)`, routes `GET /v1/pom/tools`, `POST /v1/pom/tools/call`, `GET /v1/pom/media/:file`, `POST /v1/images/generations`), route wiring, `lib/chatImageTool.ts` reads the same definitions, `docs/subsystems/http-api.md`, guides.
- [ ] Tests: definitions from params (names `generate_image`, `wf_<slug>`, schema types, required prompt); a call maps arguments to a `RunRequest` (choice/number passed as given, image data URL); unknown name / non-object refused; images API maps `size` and `n`, answers `b64_json` and `url`. Implement; PASS; commit; docs; push.
