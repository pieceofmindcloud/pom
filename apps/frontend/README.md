# POM - web interface

Administrative interface of `pom`. **It is the product's interface**: the node
reads this directory's production build from disk and serves it on the gateway
port (8080 by default), including the initial setup screen. The old `web/` was
removed.

The brand is UI-only. Crate, binary, config (`pom.json`) and routes stay `pom`.

**React 18 + TypeScript + Vite + Tailwind**, with `react-router`, `framer-motion`,
`@tanstack/react-query`, `react-markdown` + `remark-gfm` + `rehype-highlight`,
`lucide-react` and `recharts`.

## How the binary serves the interface

The node reads the files of `apps/frontend/dist` **from disk, on every request**
(`crates/node/src/backend/webapp.rs`), instead of embedding them in the binary.
Nothing in `cargo build` touches this directory - editing the interface does not
recompile Rust. To have something in `dist/`:

```bash
cd apps/frontend
npm install
npm run build
```

The production bundle is split by route. The shell loads the dashboard and the
common chrome; screens come in through `import()` when the address is opened.
`vite.config.ts` also separates React, animations, icons, React Query and each
reference document into their own chunks. This keeps the initial JavaScript small
without hiding Vite's warning with `chunkSizeWarningLimit`.

`config::ui_dist_dir()` decides where to look: by default
`<project_root>/apps/frontend/dist` (the same `project_root()` used by
`pom.json` and `models/`); `POM_UI_DIST` overrides it, for a container or a
different disk layout - that is what the Docker image uses, pointing at where the
`dist` generated in the build was copied.

Without this directory (or without `index.html` inside it) the node comes up
normally and answers 500 on the interface routes, with the message saying where
it looked.

`dist/` and `dist-release/` are local artifacts and ignored by Git.

### Routing

The bundle is mounted **last** in the axum router, and its catch-all is narrow on
purpose: a real file wins; `/api`, `/v1`, `/admin`, `/peers`, `/health` and
`/generate` never fall into the app (an unknown API route answers 404 in JSON,
not a page with status 200); a path with an extension that does not exist is 404;
what is left receives the `index.html`, so reloading `/config/keys/api` works.

## Development with hot reload

To iterate on the interface without recompiling Rust on every change, the Vite
dev server remains the path - it talks to a real node through a proxy.

```bash
# 1. the backend, at the repo root
cargo build --release
./target/release/pom --no-discovery

# 2. the interface, pointed at it
cd apps/frontend
npm install
VITE_API_TARGET=http://127.0.0.1:8099 npm run dev
```

The interface comes up at <http://localhost:5199/>. `VITE_API_TARGET` is where
Vite forwards `/api`, `/v1`, `/admin`, `/peers`, `/health` and `/generate`
(see `vite.config.ts`); without it the default is `http://127.0.0.1:8080`.

In production none of this is needed: one node serves API and interface on the
same port, so the app is same-origin and every path is relative.

### `--no-discovery`, and why it exists

The LAN discovery broadcast goes to `255.255.255.255:42777` **fixed**, ignoring
`--port`. A node brought up to try the interface joins by itself any cluster that
is up on the same network. `--no-discovery` (added on this branch) brings the node
up detached: it does not announce, does not broadcast, does not look for
bootstrap. It is the only safe way to run a test node beside a cluster that is in
use. There is a test of it in `src/network.rs`
(`a_detached_node_neither_announces_nor_looks_for_bootstrap`).

A detached node shows "Disconnected from network" in Settings › Network, with a
"Rejoin network" button that calls `/api/ui/rejoin`.

The main navigation separates Network from Settings. Network holds Network
information, Network participants, Administration, Sharing and API keys.
Settings keeps Preferences, Storage and the administrative preferences. The
Sharing screen gathers the trust root, this machine's data and the connection
key. In Public networks, each published address shows the result of a short probe
on `/health`, with a 30 second cache and manual refresh.

The local preferences live in Settings › Preferences: exposure of the
administrative interface, gateway port, TLS between nodes, GPU thermal protection
and the optional Hugging Face credential. The section also ends with About, which
lists the direct dependencies and their licenses and highlights the POM's
provisional license. The exposure starts at localhost; when it moves to the
network, the screen shows the generated human token and the API requires
`X-POM-Access-Token`. The `connection_key` stays separate, to authenticate the
nodes and the pipeline's clients.

### The screen opened without styles? Restart the dev server

Symptom: the page appears raw, serif fonts, no layout - and together, `/api/*`
returns `<!doctype html>` instead of JSON, so the screens show
"Unexpected token '<'". Both at the same time, with no error in the console and
the server answering 200 on everything.

It is not Tailwind, it is not PostCSS, it is not the cache. It is the dev server
**losing the project context** and running with no config at all. Restarting
fixes it:

```bash
# in the apps/frontend directory
npm run dev
```

How it is confirmed in ten seconds, without opening the browser:

```bash
curl -s http://localhost:5199/src/index.css | grep -c '@tailwind'   # healthy: 0
curl -s http://localhost:5199/api/ui/node | head -c 20              # healthy: {"node_id"...
```

Raw `@tailwind` in the output means PostCSS did not run at all - different from
a badly configured `content`, which would generate the preflight and consume the
directive.

**How one reaches this state without editing anything:** a `git rebase`, a
`git checkout` of another branch, anything that rewrites `vite.config.ts` with
the dev server up. Vite restarts itself when it sees the file change, and the
restart can land on the instant when the tree is mid-rewrite; it comes up without
finding the `package.json` and, from there, with no config: no PostCSS and no
`server.proxy`. Nothing fails loudly, the server keeps serving.

The practical rule: **you touched git with the dev server running, restart the
dev server.**

### The port and the configuration used in the validation

`pom.json` is not versioned. What was brought up during this delivery:

```json
{
  "role": "principal",
  "connection_key": "<connection-key>",
  "gateway_port": 8099,
  "p2p_port": 42799,
  "inference_port": 8199,
  "bootstrap_nodes": [],
  "shard_dir": "<models-dir>"
}
```

Ports outside the usual ones (8080/42777/8181) on purpose, so as not to collide
with a cluster already running on the machine.

---

## What is really wired to the backend

Everything below reads or writes on a real node. There is no sample data on any
screen: the previous version had a `mock-server.mjs` and it was removed.

| Screen | Routes |
|---|---|
| Dashboard (flow and details) | `GET /api/ui/node`, `/api/ui/cluster`, `/peers` |
| Chat | `POST /v1/chat/completions` (SSE, token by token) |
| Whole models | `GET /api/ui/models` |
| Slice models | `POST /api/ui/slice/plan`, `POST /api/ui/slice` |
| Slices / Slicing rounds | `GET /api/ui/cluster/models`, `/api/ui/jobs`, `/api/ui/jobs/:id` |
| Transfers | `GET /api/ui/jobs`, routes of `src/transfer.rs` |
| Send a file | transfer by concession (`grant`) |
| Logs per token and per node | `GET /api/ui/requests` |
| Statistics | `GET /api/ui/requests` |
| Live logs | `GET /api/ui/requests` in polling |
| Network (block node) | `GET /admin/nodes`, `POST /admin/nodes/:id/block`/`unblock` |
| API keys | `GET`/`POST /admin/api-keys`, `DELETE /admin/api-keys/:key` |
| Connection keys | `GET /api/ui/node`, `POST /admin/regenerate-key` |
| Administration exposure | `GET /api/access`, `GET`/`POST /api/ui/access` |
| Model folders | `GET`/`POST /api/ui/paths` |
| Initial setup | `GET /api/setup/key`, `POST /api/setup/save` |
| Deploy a sliced model | `POST /api/ui/deploy/plan`, `POST /api/ui/deploy`, `GET /api/ui/jobs` |
| Release a slice | `GET /api/ui/release/candidate`, `POST /api/ui/release` |

### Blocked nodes and availability for inference

Blocking a `node_id` immediately removes that remote peer from the cluster's
operational view: it does not appear on the Dashboard, does not take part in the
catalog and the deployment plan, and cannot receive inference traffic. UDP
announcements already arriving do not undo that block. The Network screen still
lists it as blocked, so the operator can unblock or remove it.

Before assembling a pipeline, each request also queries `GET /health` of each
candidate inference endpoint, in parallel, with a ten second limit. Only peers
that answer successfully enter that request's plan. This way an old UDP
announcement or a stopped inference server does not artificially close the layer
coverage nor make the request wait for an unavailable hop.

**The per-node API token is really wired**, not a mockup: the tokens are issued,
listed and revoked in `/admin/api-keys`, they live in `api_keys` in the
`pom.json`, they count as a credential on the `/v1` routes (`ApiKeyAuth`,
`src/auth.rs`), and every served request is attributed to the token that paid for
it - that is what the Logs screen shows. The record lives **in the process
memory** (`src/reqlog.rs`): restarting the node zeroes the history, and only the
most recent ones remain.

The notifications (the bell) survive a reload because the progress comes from the
node, via `/api/ui/jobs`, and not from browser state.

## What is not wired

- **Chat conversation history** - sessions live in `localStorage`
  (`src/lib/chatStore.ts`). It disappears when the browser is cleared and does not
  follow the user between machines. It was the accepted choice for this delivery.
- **`GET /admin/transfer-offers`** returns a fixed empty list in the backend
  (`src/admin.rs`). The interface consumes the route; the backend does not yet
  keep the received offers to answer it.
- **Text generation** depends on there being a model loaded on the node. The
  validated node came up without a model (see below), so the Chat connects, calls
  `/v1/chat/completions` and shows the error the node returns. The SSE path itself
  was exercised.

### Why the validated node came up without a model

The validation machine is the operator's work machine: 16 GB in total, with ~5 GB
recoverable at the moment of the measurement (`vm_stat`, free + inactive +
speculative + purgeable pages). The smallest combination of slices that covers a
whole model among the available ones is `qwen3-8b_0_31` (3.9 GB) +
`qwen3-8b_32_35` (1.3 GB) = 5.2 GB in two processes - above the headroom, that is,
swap. No model small enough was on the disk. By calculation, then, and not by
attempt: the node came up in gateway mode, with every route up and no weights in
memory.

To exercise generation, bring the node up with a slice and `--inference-port`, or
point `VITE_API_TARGET` at a node that already has a model loaded.

---

## New routes created in the backend

All in `src/webui.rs`, in the file's style, plus one new module.

| Route | What it does |
|---|---|
| `GET /api/ui/jobs` | Lists the active jobs and the ones finished less than ten minutes ago. `/api/ui/jobs/:id` already existed, and it only serves to follow a job whose id you already have - without the listing, the bell would have no way to rebuild itself after a reload. |
| `POST /api/ui/unload` | Unloads the node's model from memory. |
| `POST /api/ui/leave` | Leaves the network: stops announcing and forgets the peers. |
| `POST /api/ui/rejoin` | Returns to the network. |
| `GET /api/ui/requests` | The request record, per token and per node, that feeds Logs and Statistics. |
| `POST /api/ui/release` | The node gives back the memory of the slice it serves and stays in the network; if there is an idle peer with the slice on disk, it is passed on. It is never refused. |
| `GET /api/ui/release/candidate` | Who would take the slice, so the confirmation on the screen can really warn when the range will be left uncovered. |
| `POST /api/ui/deploy/plan` | Read only: who would serve which range of a ready set of slices, and what would have to be copied first. |
| `POST /api/ui/deploy` | Executes. Redoes the plan on the server, makes each target pull what is missing through the existing transport, waits, and then calls the `launch` with `skip_split`. It is a job, so the bell follows it and reloading loses nothing. |

New module `src/reqlog.rs`: in-memory record of the served requests, with the
`Caller` (operator, token with a hint of its value, or anonymous) and the hops
traveled. `src/gateway.rs` and `src/openai.rs` started feeding it, and
`src/auth.rs` gained `ApiKeyAuth`, which resolves the request's credential into a
`Caller`.

New CLI flag: `--no-discovery` (`src/main.rs`, `src/network.rs`).

`cargo build --release` passes and `cargo test` stays green (145 tests).

---

## Deploying an already sliced model

`src/deploy.rs` + `src/routes/DeployModel.tsx`. The pieces already existed - the
catalog knows who holds which slice, `transfer` moves a file between machines, the
`launch` ties a range to a peer. What was missing was the decision in the middle:
**which machine serves which slice**, and what needs to be copied for that to be
possible. The module is only that decision plus the sequence; it does not invent a
second transfer or launch mechanism.

### The automatic plan remains the few-clicks path

Without `selection` in the body, the routes behave as before: the node decides and
the screen shows what it decided. The rules, in the order they matter:

1. The sweep starts at layer 0 and always takes the **longest** range that
   continues from where the previous one stopped. This is not aesthetics: fewer
   slices means fewer machines needed, and taking only what continues the chain
   makes overlap impossible by construction - which started to matter when the
   same model can be sliced twice with different cuts.
2. A machine may serve several valid ranges, including separate ranges. The set
   must cover each required layer exactly once; a global gap is refusal, not a
   half deployment.
3. A machine that already holds the slice is preferred for it - each copy avoided
   is ~17 minutes less on the real link.
4. Layer 0 goes to the machine with the most free memory, because whoever holds it
   processes the whole prompt of every request.

After the copies, the path of each file is asked of the machine that received it
instead of being deduced - only it knows where its own `received/` is.

### What the operator chooses on top of the plan

The plan is a suggestion. `selection` in the body of both routes carries the
operator's own answer, and the server **validates** that answer instead of
recomputing its own and ignoring it - `POST /api/ui/deploy` redoes the cluster
sweep (a machine may have left between looking and deciding) but does not redo the
choice.

- **Which layers enter.** The screen lists *every* range that exists in the
  cluster, from every slicing, each with a checkbox. Mixing different cuts is
  legitimate and the coverage ribbon shows at once what ended covered, uncovered
  or doubled.
- **Which machine gets each range.** A `select` per row. Choosing a machine that
  already holds another range **swaps** the two, which is what the operator meant.
- **The pipeline's order.** The ↑ ↓ buttons swap the machine with the neighboring
  row. There is no second "order" button because there is no second order: the
  `pipeline.rs` sweeps from layer 0 upward, so whoever holds layer 0 receives every
  prompt, and reordering the pipeline **is** reassigning the ranges. The ribbon at
  the top shows the resulting order, with "processes the prompt" on the first.

Feasibility, and what is refusal and what is a warning:

| Situation | Answer |
|---|---|
| Chosen ranges overlap | **Refusal.** The layers would run twice and the answer would come out wrong with nothing failing. |
| Two or more ranges on the same machine | **Accepted** when they do not overlap and the global coverage is complete. If "join neighboring slices" is on, only **adjacent** ranges of the same machine are merged into one file (`splitter::merge`); separate ranges stay independent steps. |
| Range without a machine, or a machine that left the cluster | **Refusal.** |
| A range nobody in the cluster holds | **Refusal.** |
| Insufficient disk on a copy's destination | **Refusal** - `transfer.rs` would refuse anyway, better before moving a byte. |
| Tight memory on the destination | **Warning**, with the number. It is an estimate (free VRAM where there is a card, available RAM elsewhere), and a wrong block would be worse than a yellow line. |
| Hole in the coverage | **Refusal.** The deployment must cover all required layers before forming the pipeline. |

### Advanced adjustments (execution)

The fourteen knobs of `RuntimeParams` (`src/webui.rs`) came back to the screen, with
the same labels and defaults of the old interface, and now they really go to each
slice's `launch`: context (`n_ctx`), batch (`n_batch`), max new tokens, concurrent
requests, tokens per chunk, temperature, `top_p`, `top_k`, repetition penalty and
its window, the safety reserve of the automatic fit's VRAM, turning the reasoning
block off, and forcing without embeddings.

They are **of the cluster**, not of each peer, because the pipeline needs them to
be: the context and the batch decide the prompt budget the gateway charges on every
request, and the sampling only runs on the last peer. The exception is **layers on
the GPU**, which is a property of the machine - a box with a 5060 Ti and one without
a card want different numbers. It is edited in the machines table, travels in
`LaunchTarget::n_gpu_layers` and overrides only that peer (`webui::params_for`).

The panel's header quotes the number that decides whether a harness fits - "Each
request accepts up to {prompt} prompt tokens, in chunks of {chunk}" - computed on
the server by `gateway::prompt_budget_of` / `prompt_chunk_of`, the same functions
the gateway uses to refuse a prompt that is too large. Measured on the screen:
`n_ctx 32768`, 2 sessions, 256 new → 16,128 tokens; chunk `min(4096, 3072)` =
3,072.

`no_embeddings` stays in the panel for parity with the old interface and **with the
warning on the screen**: it is normally automatic (only the last peer skips it), and
turning it on prevents the first one from extracting the hidden state.

### What is really wired to the backend on this screen

Everything. There is no sample data: the set of slices comes from
`GET /api/ui/cluster/models`, the plan and validation from `POST /api/ui/deploy/plan`
(which asks each peer its `/api/ui/models` and its `/api/ui/node`), the execution
from `POST /api/ui/deploy`, and the progress from `GET /api/ui/jobs` - the same job
that appears in the bell, now also with a bar on the screen itself. The copies keep
going out through `POST /api/transfer/pull` on each destination: no parallel
mechanism was created.

What the screen does **not** compute on its own: none of the validation. The
plan is surveyed once per model (machines, valid cuts, defaults); map edits stay
local, and pressing Deploy sends the map to the node once - a refusal opens a
window with every reason, and nothing is deployed.

### Routes changed

| Route | Change |
|---|---|
| `POST /api/ui/deploy/plan` | Accepts `selection` (range + machine + optional `n_gpu_layers`) and `params`. Returns, besides the plan: `available` (every range of the cluster, with who holds it), `machines` (free memory and disk, what each one holds and what it takes on), `missing`, `overlap`, `warnings`, `params`, `prompt_tokens`, `chunk_tokens`, and per step `position`, `primary`, `options`, `problem`, `note`. |
| `POST /api/ui/deploy` | Accepts and validates the same `selection`/`params` instead of replanning on its own. |
| `POST /api/ui/launch` | `LaunchTarget` gained `n_gpu_layers: Option<u32>`. |

Nothing was removed from those routes' old body: a client that only sends `group`
keeps receiving exactly the automatic plan of before.

### How this was reviewed, and what was left out of the review

**Nothing was deployed to the operator's cluster.** The order was clear: plan,
validate and review, without pressing "Deploy" against the operator's machines, and
without touching the CUDA server. So:

- **Real backend, one machine.** An isolated node (`--no-discovery`, with
  `POM_HOME` in a temporary directory) with four header-only GGUF slices,
  overlapping on purpose (0-4 / 5-31 and 0-10 / 11-31). It is the origin of the
  refusal states: "there are 2 slices for 1 machine", a range without a machine,
  and the automatic choice taking 0-10 + 11-31 among the four cuts.
- **Three machines, by stand-in.** A local server of ~250 lines (scratchpad, not
  versioned) serves the same routes with three machines, to review what one machine
  alone cannot draw: destination `select`, the swap, the copy row, the memory
  warning, the per-machine GPU column. It is a **port** of Rust's `plan()`, not the
  source of truth - the logic is Rust's, covered by the tests.
- The `POST /api/ui/deploy` was exercised **only against the stand-in** (which
  returns 400 on purpose), to check the error toast and the confirmation dialog.

Fixed in the pixel review: native checkbox (the system's unchecked state arrives as
a light gray square and reads as *disabled*), range alignment between the ribbon
cards, the destination `select`'s width (a width class on it loses to the
component's `w-full` - the width lives in the cell), a footer promising "1 copy" for
a range without a machine, and the order of the slices in the machines table, which
came in directory order.

Tests: `cargo test` (145 + 2 green), with the planner and the validation covered -
full coverage, gap, overlap, two ranges on one machine, range without a machine,
machine out of the cluster, range nobody holds, insufficient disk, tight memory,
VRAM vs RAM reading, parameter and budget echo, and per-machine GPU.

## What is still missing

- Persisting the chat history outside the browser.
- Keeping the transfer offers in the backend, so `/admin/transfer-offers` stops
  being an empty list.
- A request record on disk, to survive a node restart.
- The logo. `src/components/Logo.tsx` is a placeholder with fixed dimensions,
  isolated on purpose: replacing it with an SVG touches only that file.

## Details that save time later

- **Editing `apps/frontend/` only requires `npm run build`** to take effect on the
  node - the bundle is read from disk on every request, not embedded. During
  development use the dev server, which has hot reload.
- **`Footnote`** (`src/components/ui.tsx`) exists because the obvious markup is a
  trap: a `<p className="flex">` with a `<span>` in the middle of the sentence makes
  the span its own flex item and the sentence breaks into columns. Use the
  component, do not repeat the markup.
- The dark theme is the default; the light one is defined on bare `:root`, and the
  dark one redefines tokens under `[data-theme="dark"]` (`src/index.css`).
- Nothing scrolls horizontally: wide content scrolls inside its own panel
  (`ScrollX`), never the page.