# AGENTS.md - raiz

Repository index. **This file is a map, not a manual**: the detail of each
subject lives in the folder's `AGENTS.md` and in the subsystem READMEs under
`docs/subsystems/`. If you need more than one paragraph about something, its
place is there, not here.

`CLAUDE.md` is a symlink to this file.

## Where everything is

**By subsystem** (`docs/subsystems/`) - the "how it works and why":

| File | Subject |
|---|---|
| `build-and-run.md` | crate workspace, `cargo build`/features, Docker, release, environment variables, scripts |
| `operations.md` | bringing up a cluster, verifying the inference is joint, field gotchas, two peers on the same machine, harness, load monitor |
| `p2p-network.md` | UDP + iroh discovery, announced addresses, dynamic routes, pkarr, relays and hole punching, identity |
| `inference.md` | prefill, decode, hidden-state wire, ring vs direct reply, speculative/MTP, prompt limit |
| `parallelism.md` | pipeline parallelism, tensor parallelism, choice by topology and telemetry |
| `slicing.md` | GGUF split/merge, ggml type table, size-based planning |
| `deployment.md` | putting slices in service, merge, pipeline order, slice lifecycle |
| `freetoken-moe.md` | FreeToken goal measurements, KV budget and the in-house MoE path |
| `file-transfer.md` | pulling/offering files between machines, resumption, MD5, concessions |
| `http-api.md` | every route, authentication, `x_pom.source`, `gateway::testkit` |
| `web-ui.md` | how the UI comes up, menu, modals, Panel, Chat, slicing/deploy screens |
| `plugins-ui.md` | menus, screens, routes, profiles and i18n provided by plugins (`pom-plugin-ui/v1` manifest) |
| `enterprise-licensing.md` | independent plugins, licenses, lease, process host, GitHub repositories |

**By folder** (`AGENTS.md`) - what each file does, function by function, and the
links to other folders: `crates/`, `crates/node/`, and inside
`crates/node/src/`: `models/`, `network/`, `backend/`, `inference/`, `routing/`,
`utils/`; `crates/routing-contract/`, `crates/routing-base/`, `crates/types/`;
`apps/frontend/` and `src/{components,lib,routes,i18n}/`; `docs/`; `scripts/`.

`docs/AGENTS.md` explains the specs and plans (`docs/superpowers/`);
`docs/architecture/routing-plugin-contract.md` is the open-core boundary;
`docs/reports/` keeps dated technical reports (investigations, benchmarks).

**`brand.json` (root)** is the product identity - name, slogan and logo - and
the single source of every derived name: config file, environment variables,
identity keys, GGUF metadata and the pkarr record. The binary embeds it at
compile time (`crates/brand/src/lib.rs`, `include_str!`) and the interface copies it into the bundle
(`apps/frontend/scripts/build-brand.mjs`). The images live in `assets/brand/`.

`assets/geo/` holds the offline city database the binary embeds
(`crates/node/src/utils/geo.rs`); its `README.md` has the source, license and
update procedure.

**Not a source of truth for architecture:** the root `README.md` (it is a
presentation).

## Documentation duty

**Whoever changes a folder updates its `AGENTS.md` and the affected subsystem
READMEs in the same commit.** The task is not done while the documentation does
not reflect the delivered code. Outdated documentation is a bug: a future agent
reads the doc and understands the system wrong.

Writing rules: all documentation is written in English (the folder `AGENTS.md`
files were the last Portuguese holdouts and are translated as they are edited);
no absolute local machine path (`/Users/...`, `/home/...`) - use paths relative
to the repository or placeholders like `<repo>`/`<reports-dir>`; no personal
information: people's names, usernames or the operator's machines (hosts,
cluster hostnames) do not enter the documentation, use generic roles ("machine
A", "the CUDA server", "the operator"); never an em dash, use a hyphen "-";
document what the code does today, verified; where code and doc diverge, the
code wins and the doc is corrected. A real defect is noted in
`## Defects noted` and reported, not fixed on the side. A fixed defect leaves
the annotation **in the same commit** - a fixed defect that stays annotated is
worse than one not annotated.

The folder `crates/node/models/` is in the `.gitignore` (pattern `models*`), so
its `AGENTS.md` lives in `crates/node/src/inference/AGENTS.md`.

The copy in `apps/frontend/src/docs/` is **generated** by
`apps/frontend/scripts/build-docs.mjs` (it runs in `npm run build`/`dev`/`test`).
Never edit it.

## Commands

```bash
cargo build --release                 # ./target/release/pom
cargo build --release --features metal   # Apple Silicon
cargo build --release --features cuda    # NVIDIA (toolkit >= 12.8 for sm_120)
cargo test                            # crates/node
cargo test --workspace                # all public crates
RUST_LOG=debug ./target/release/pom ...

cd apps/frontend && npm run build     # REQUIRED after editing the UI
```

The Docker image also compiles `pom-plugin-host` and places it beside `pom`;
the entrypoint keeps the path explicit and the node starts an isolated host when
a plugin is installed. The local Compose uses `dev-runtime`, mounts the
allowlisted sources in `/opt/pom/dev-plugins` and enables `POM_DEV_MODE=1` for
the local rebuild; Cargo writes into `.pom-dev-plugin-builds/` at the POM
project root, outside the sources that may be mounted read-only. The cluster's
plugin profile uses the same target and keeps the Cargo cache in
`.pom-dev-cargo`, inside the project volume and writable by the node's user. The
release `runtime` image does not include a toolchain. The development Docker
builds use libraries from the exact fork release by default; to compile local
sources, use `POM_LLAMA_CPP_MODE=source` and, optionally,
`POM_LLAMA_CPP_SOURCE_DIR=/path/to/llama.cpp` with the build scripts.

Preparing the build

The llama.cpp engine (`llama-engine` and the `llama-cpp-sys-2` binding) lives in
the private repository `pom-plugin-llama.cpp`, pinned by `rev` in `Cargo.toml`
(`[patch.crates-io]`) and in `crates/node/Cargo.toml`. Cargo fetches it over SSH
with the `git` client (`[net] git-fetch-with-cli` in `.cargo/config.toml`).

The Cargo builds and the Docker `source` mode need the `llama.cpp` fork on the
`pom-tensor-parallel` branch. `.cargo/config.toml` sets `LLAMA_SOURCE=llama.cpp`
relative to this root: leave a checkout or a symlink of the fork there (gitignored).
An `LLAMA_SOURCE` already set in the environment (CI, Docker) prevails. The
binding's build accepts a checkout with `ggml`; with none, the preparation error
shows the manual clone command.

If `llama_moe_ffn_descriptor`, `llama_set_moe_ffn_callback` or the
`moe_external_executor*` fields of `llama_model_params` are missing, the fork's
checkout is out of date - it is not a preexisting defect of the project. In a new
copy, the first `cargo build` prepares the ignored content; do not create an
additional symlink nor change files inside an existing checkout of the fork. See
`docs/subsystems/build-and-run.md`, section "Preparing a local worktree".

**Compiling with `--features cuda` without `CUDAARCHS` is almost always waste**:
llama.cpp compiles each CUDA object for ten GPU architectures. Measure the cost
and each machine's architecture in `docs/subsystems/build-and-run.md`
("GPU architectures"). In development, pin only the machine's architecture (for
example `CUDAARCHS=120` on an RTX 5060 Ti; a machine with two architectures needs
both together, like `CUDAARCHS="120;86"`). The wide list is only for release.
Changing the value invalidates the previous native build and cargo does not notice
the change.

The interface is **read from disk** at `apps/frontend/dist` at runtime
(`config::ui_dist_dir()`, `$POM_UI_DIST` overrides), not embedded in the binary:
`cargo build` does not rebuild the screen and needs no Node. In Docker, the
frontend is compiled in an independent stage and its copy into the runtime is the
last layer; an edit only of the UI does not invalidate the backend.

```bash
./target/release/pom                       # no args: uses pom.json
./target/release/pom --port 42778 --gateway-port 8091   # second local node
```

The model never comes from the command line: it is deployed through the interface.

Every node serves the UI and the API on the `--gateway-port` (8080): open
`http://<peer>:8080/`. See `docs/subsystems/operations.md` for the full bring-up
and `build-and-run.md` for the scripts and variables.

## Architecture in one paragraph

A single Rust binary (`crates/node`, package `node`) that coordinates LLM layers
across machines. Each node discovers peers over UDP and an iroh endpoint, announces the layer
range it serves, and any node can be the gateway: `POST /api/generate` and the
`/v1` routes run **only** the distributed pipeline - with no plan covering
`0..total_layers` the request fails with a message, there is no fallback to
Ollama. Each peer's inference server (`/v1/prefill`, `/v1/step`) loads the model
in-process via `llama-cpp-sys-2` and exchanges hidden state in f16 by default;
f32 is a lossless option that doubles the payload, and f8/i4 groupwise are
optional lossy formats negotiated per hop.
Concurrency: Tokio, shared state in `Arc<RwLock<>>`.

Modules in `crates/node/src/`: `main.rs` (CLI and orchestration), `models/`
(catalog, GGUF, profiles, splitter), `network/` (discovery, identity, transport,
tunnel), `routing/` (pipeline planner), `backend/` (gateway, APIs, admin,
transfer, deploy, UI), `inference/` (embedded server, llm, wire, relay, direct,
speculative), `utils/` (config, sysmon).

## Invariants that hold everywhere

Each one is detailed in the subsystem named; they are here because breaking one
breaks the whole product.

- **Project root.** `models/`, `pom.json`, keys and the UI come from
  `config::project_root()` (`$POM_HOME` → a cwd that looks like the project →
  the executable's directory → the default home: `~/.pom`, or the executable's folder on Windows). Launching the binary from another cwd reads another
  config. Two nodes on the same machine need distinct `POM_HOME`s, because the
  identity (`.pom-node.key`) lives there. → `operations.md`
- **`node_id` is the machine's name in the cluster** and the key of
  `blocked_nodes`; deleting `.pom-node.key` creates a new machine. It is the
  node's **only** key:
  the iroh endpoint id (the public key itself), the pkarr `pk:` key and the
  authority key all derive from it. **An IP is never an identity**: signed
  documents and screens carry keys (and relay URLs with host names), and an
  address is only resolved at dial time (direct, or through a relay - n0's, a
  dedicated one or a member - with iroh punching the NAT underneath). → `p2p-network.md`, section 0
- **The connection key is the same across the whole cluster** and is the operator
  credential: it authenticates `/admin/*`, `/api/ui/{plan,apply,launch}` and the
  node-to-node synchronization. `POST /api/generate` and every `/v1/` route accept
  only an active API key. The UI's administrative surface uses an in-memory session
  with an `HttpOnly` cookie; the human password lives only in the tab's memory and
  never goes to storage or a URL. The node persists only its Argon2id hash. In
  local mode, the same boundary covers the static files, but never the pipeline
  routes. Without `pom.json` the node creates a **local network of its own** and
  goes straight to the interface: profile `admin`, named after the host, discovery
  off (`webui::bootstrap_local_network`). On that first start the interface opens
  on the first run wizard, which only asks the machine's name and the network's
  (`web-ui.md`). The older setup wizard, which ran before any configuration
  existed, is deprecated and unreachable: its code is kept, but no flag, route or
  screen leads to it. Joining another network is asked from the Networks screen and
  approved by that network's admin, who is the only one that hands over its
  connection key. **That own network is never dropped**: joining puts it in standby
  and leaving brings it back as it was (`webui/standby.rs`), and a member that
  leaves makes the principal replace the connection key. The persisted profile and
  its policies live in `crates/node/src/profile.rs`; the key remains the
  authentication mechanism. → `http-api.md`, `web-ui.md`
- **A node's role is enforced by the nodes, not by the screens.** An
  inference node refuses everything that would make it hold or serve a model
  (transfers, slicing, `apply`, being a deploy target), whoever asks, an admin
  included. What one node may ask of another depends on who signed the request:
  routes that govern the network take the principal, deploy routes take any admin
  of the trust chain, routes of a serving node take a worker, and the
  connection key alone opens none of them - every member holds it. A machine's
  own operator routes (API keys, plugins, its name, networks, logs) are reached
  by no other node at all. **The trust chain names the admins only**: a worker
  or a visitor carries a short lived membership token an admin signed
  (`src/membership.rs`), which every node checks against the chain alone, so
  admitting a member changes no trust document. **A secondary admin deploys
  models, decides admissions and removes workers and visitors, and nothing
  else**: settings, keys, admins and every change to the trust chain come from
  the principal, on the screen and between nodes. The role is granted at
  admission and a machine admitted as a visitor stays one whatever it
  announces. → `http-api.md`
- **Plugins are independent of each other and are a process boundary.** POM no
  longer has a single product-wide "Enterprise license": `PluginManager`
  (`crates/node/src/plugins.rs`) keeps an activation, lease, heartbeat, process
  host and version-update state PER `plugin_code` returned by the license server
  or restored from local state, with no static catalog in the binary and no state
  shared between plugins. Any profile (`admin`, `worker`, `visitor`) activates and
  installs plugins on its own node, including the development rebuild (which
  requires `POM_DEV_MODE`). Each plugin can only be loaded by `pom-plugin-host`
  through the C ABI and the versioned IPC in `crates/enterprise-protocol` - the
  host already supports several concurrent instances, one per active plugin. The
  Request Logs and Statistics screens stay discoverable in the menu without a
  license, but the backend refuses the data until that plugin's entitlement is
  active. Updating the `pom` binary itself is NOT a plugin and lives apart, in
  `crates/node/src/pom_core_update.rs` - it will become a public GitHub Releases
  without a license (another task); until then it borrows the `enterprise`
  plugin's lease, but its code and its screen (`/config/about`) never mix with
  those of the Plugins screen (`/plugins`). →
  `docs/subsystems/enterprise-licensing.md`
- **First-version policy.** The product has not shipped yet. Therefore, do not
  keep in the runtime compatibility code, adapters or fallbacks for old protocol
  versions - especially the inference v1/v2 formats. The `v1` of
  `frame/v1-batch` identifies the first current official contract, not an
  obligation to support a legacy protocol; future changes must evolve that
  contract explicitly. → `inference.md`
- **The deployment's `max_new_tokens` bounds one generation, not the request.**
  It was never a backend cap - the `infer_server` does not know it. It caps one
  **leg** of the gateway's continuation loop, which crosses the leg in the same KV
  session and the same SSE, with no re-prefill and nothing visible to the client.
  What bounds the request is the client's `max_tokens` and the session's context
  window (`n_ctx_per_session`, counted with the real prompt), and only they produce
  `finish_reason: "length"`. Leaking a leg's `length` made the harness drop the
  tool call halfway. → `inference.md` (9, 9.1)
- **The ceiling the client asks for is enforced and inviolable, and reaching it
  is a terminal stop.** Nothing goes past it, not even a tool call open on the
  cut token - that is what OpenAI and Anthropic do, and what stands in place of
  overflow is the **task budget** (`output_config.task_budget`), which warns the
  model instead of stretching the ceiling. A cut inside a call returns the call
  **structured and incomplete**, like the two providers: raw markup in `content`
  is never an answer, and discarding it is not either.
  The output capacity comes from the effective `n_ctx` announced in `/health` and
  aggregated across the deployment's live peers. It is published in `/v1/models`;
  `max_new_tokens` stays only the size of one leg of the loop, and `max_tokens`
  stays the budget the client asked for.
  In streaming, a call's arguments come out in **fragments** as the model generates
  them, and concatenating them is always the string the non-streaming path would
  produce. → `inference.md` (9.1), `http-api.md`
- **Generation never happens in the prompt's KV sequence.** Each session of a
  peer has two sequences: the prefill writes into the prompt's, generation into a
  copy of it, and the end of the turn erases the copy entirely. Only **total**
  sequence removal is used, because partial removal is refused by recurrent and
  hybrid engines - and the prefix cache across turns is product functionality,
  which applies to every model served, not an eligibility criterion. Retention is
  the peer's local act at EOS; `/v1/session/rewind` only confirms. → `inference.md`
- **The pipeline's order is the layers' order**, and one node may serve several
  ranges. `merge` only joins adjacent ranges on the same node. Overlap and a
  global hole are refusal. Whoever holds layer 0 processes the whole prompt of
  every request. → `deployment.md`
- **How many layers the model has is an `Option`, not a number.** A slice without
  `pom.*` metadata does not know the total; inventing zero was a real bug. →
  `slicing.md`
- **`<config>.last-slice.json` only becomes `serving` through the `/health`
  watcher.** Annotating in the `apply` what only the `/health` knows is an infinite
  restart loop. → `deployment.md`
- **`idle` is an active state per peer.** With `unload_if_idle`, the local
  activity recorded in `backend/reqlog.rs` is evaluated off the hot path by the
  existing monitor: the model is unloaded, the peer keeps announcing and enters
  `idle`; the next request reloads locally and sends the authenticated
  `reload_model` event to the plan's idle peers for parallel loading, repeating the
  event until all answer `ready` and replanning afterwards. There is no new timer
  or `sleep` in the inference: the wait only exists while an idle pipeline wakes
  up. → `deployment.md`, `p2p-network.md`
- **No weight travels the network because of slicing**: each peer extracts
  its own range from its own copy. A file transfer is an explicit act of the
  operator, and **a file never crosses a public relay** (n0's, one in
  `relay_peers`, or one that is not a member of the cluster): it goes direct, over a hole
  punched connection, or through a relay of the cluster itself. A node the
  internet reaches relays for its own cluster by default; relaying for everyone
  is an option. → `slicing.md`, `file-transfer.md`, `p2p-network.md`
- **The product's name is not literal anywhere.** Config, env, keys, GGUF
  metadata and the pkarr record derive from `brand.json`'s `slug`
  (`brand::config_filename()`, `brand::env_var()`, `brand::gguf_key()`,
  `brand::dns_record()`). The only exception cargo forces is the `[[bin]]` in
  `crates/node/Cargo.toml`. → `build-and-run.md`
- **Between nodes it is always mutual TLS, and the key alone opens nothing.**
  There is no TLS switch; plain HTTP is served only to loopback, which keeps
  `http://localhost` working for the operator. Each node's certificate is
  derived from its `.pom-node.key`, verified by `SHA256(pubkey) == node_id`, and
  presented on both ends, so a call authorized by the connection key also needs
  the caller to be a member today: an admin of the chain, or a node with a valid
  membership token no admin revoked. Every UDP announce is signed by the node
  key. A ban is a revocation any admin signs, so it holds while the principal is
  away. → `p2p-network.md` (2, 5), `http-api.md`
- **Route tests use `gateway::testkit`**, never real UDP - the suite transmits on
  the LAN and could inject messages into a live cluster. → `http-api.md`

The engineering pending items that accompany these invariants live in `TODO.md`;
this file does not duplicate the work list.

## Maintaining this file

It must fit in one reading. New knowledge enters the folder's `AGENTS.md` or the
corresponding subsystem; only what changes the map enters here (a new folder, a
new subsystem, an invariant that cuts across everything). Prefer rewriting and
pruning over appending.

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.