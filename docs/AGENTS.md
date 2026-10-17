# AGENTS.md - docs/

Design, plan and reference documentation. Everything in this folder is written
in English. The operator manual is the root `AGENTS.md` (maintained
separately). Layout:

- `subsystems/` - the permanent runtime guides ("how it works and why").
- `architecture/` - permanent notes about repository boundaries.
- `reports/` - dated technical reports (investigations, benchmarks, delivery
  reports). They describe the code at the time they were written.
- `superpowers/` - historical specs and plans. Naming rule: **spec is design,
  plan is implementation**, both dated (`YYYY-MM-DD-<slug>`). These files are
  kept as written (some in Portuguese) and are not part of the living docs.

## architecture/

- `routing-plugin-contract.md` - boundary between the public crates
  (`crates/routing-contract`, `crates/routing-base`) and a future private
  enterprise provider: the provider only receives a `RoutingRequest` and
  returns a `RoutingPlan`, never sockets, model files, HTTP credentials or the
  inference engine; the node keeps using the internal planner
  (`crates/node/src/routing/pipeline.rs`) while equivalence coverage grows.
  The root README.md points to it.
- `kv-cache-device-placement.md` - investigation of separating the weights
  device from the KV cache device, including the pending decision that needs a
  new API in the llama.cpp fork.

## reports/

- `hf-provider-reuse.md` - pre-implementation survey and reuse boundaries of
  the `ModelProvider` integration with the Hugging Face CLI.
- `hf-provider.md` - delivery report of that integration: authenticated CLI
  status, license lookup and the fallback to model details.
- `hf-provider-http.md` - the switch to native HTTP: the factual answer about
  Python packaging, the own cache, resume, cancellation, integrity,
  authentication and the removal of the CLI path.
- `hf-provider-ui-reuse.md` / `hf-provider-ui.md` - reuse survey and delivery
  report of the UI phase: status panel, gated models and the license shown
  before download.
- `bench-llamacpp-qwen36-unified.md` - versioned record of the comparison of
  plain llama.cpp with unified cache, MTP and the POM geometry.
- `moe-nan-heterogeneous.md` - handoff report on the NaN in the MoE path for
  the heterogeneous Q4_K + Q6_K Ornith-1.5-35B GGUF.
- `three-machine-network.md` - the three-machine network investigation: the
  deploy button that would not enable, the silent gossipsub and the untested
  tunnel.
- `pipeline-telemetry.md` - distributed pipeline telemetry: the
  `outside-node` math, the missing second peer in decode and the
  `remote-stream` instrumentation.

## subsystems/

The permanent runtime guides. Beyond what their titles say:

- `p2p-network.md` - discovery, identity, TLS, the iroh endpoint's protocols for process
  logs and audit replicated between admins, and the network Chat (endpoint
  protocol, validation and relay by the principal, authenticated switch
  ratchet, inference tunnel priority over chat traffic). Tests use
  `Network::testkit` on loopback with an injectable announce target and no
  global broadcast; authenticated cluster policy probes use real listeners in
  the testkit and walk gateway candidates when a ghost address answers
  401/403.
- `slicing.md` - required tensor selection, the real cost of repeated heads,
  rebasing layer metadata to local numbering and header-only catalog reads,
  including for the selected model.
- `parallelism.md` - pipeline versus tensor parallelism, WAN costs and how to
  read the matching telemetry.
- `build-and-run.md` - also the compile-time generation of the MTP list from
  the local `llama.cpp` fork, the conditional sccache helper, the shared
  target measurement and the release/source modes of the development Docker.
- `inference.md` and `web-ui.md` - the Chat's observed RTT, directional RTT per
  edge, effective placement and configuration per peer, the second MTP context
  reservation and its fallback to RAM. In `inference.md` the main model fit is
  authoritative: the MTP reservation only joins a combined attempt that cannot
  reduce its layers. `inference.md` also records the known causes of session
  state loss, MTP availability in `/health`, the Kimi-K3 snapshot rollback and
  the observability task recommended to tell them apart in the field; section
  2.1.1 records the advertised address regression of 2026-09-04.
- `deployment.md` - `tp_parts` selection, `1ofN` ranks, sending the `.pom`
  before the weights file, several ranges per machine with full global
  coverage, and the analytical capacity preview without calling the native
  fit. Together with `http-api.md` and `web-ui.md` it details the incremental
  deployment topology flow, mixed mode and the versions created when machines
  are added or removed. `deployment.md` and `inference.md` keep the contract
  that `idle` deployments stay selectable by name, even with no advertised
  capacity, so the first request can wake them.
- `web-ui.md` - also the administrative exposure choice, the human token
  separate from the `connection_key`, the `admin` profile authority over adding
  machines and undeploying, the TLS guidance in Preferences, how validation
  shows in the Deploy screen, how the map shows hops and directional RTTs and
  how the Inference profile narrows the view without creating authorization in
  the backend.
- `http-api.md` - also that the advertised address is a local mutation open to
  every profile, that the federated public directory only accepts signed
  records, that the UI fallback does not intercept methods of concrete
  administrative routes, and the HTTP contract of the chat history.
- `freetoken-moe.md` - method, measurements and invariants of the goal that
  adds the own MoE path beside the dense path: the four-model milestone on
  the CUDA test server, the dtype safeguard, the KV reservation multiplied by `max_sessions`,
  the later CUDA goal and, with `build-and-run.md`, the NCCL investigation in
  the final CUDA link.
- `enterprise-licensing.md` - the license contract per independent
  `plugin_code` (no static catalog of installable plugins), entitlement of the
  audit screens, the `pom`/`pom-plugin-host` boundary, the `enterprise.db`
  plugin and the remaining P2P/release limits. The `/plugins` showcase crosses
  the public catalog with the node's local plugin state (open to every
  profile), local development installs without license activation, and sends
  purchases to the public store by `plugin_code`. With `http-api.md` and
  `build-and-run.md` it documents the daily `pom_core`
  release notice, applied only after explicit authorization in the UI; that
  notice is not a plugin and lives in `crates/node/src/pom_core_update.rs`
  with its own routes and screen (`/api/ui/pom-update/*`, `/config/about`).

## superpowers/specs/ and superpowers/plans/

Specs are the "why" and "what" before any code: context and measurements that
shape the design, architecture decisions, explicit invariants and a final "Out
of scope" section. Plans are task-by-task TDD implementations (failing test ->
minimal implementation -> passing test -> commit) with the code embedded in the
file; each plan cites its spec in the header and `- [x]` marks a finished step.
Files and lines cited are from when the plan was written - check `file:line`
before relying on them, the code moves.

Notable pairs:

- `2026-05-24-pkarr-discovery-design.md` and
  `2026-05-25-dcutr-nat-traversal-design.md` - internet discovery under libp2p,
  the basis of `pkarr_node.rs`; the transport has since moved to iroh (see
  `subsystems/p2p-network.md`, section 3).
- `2026-08-07-principal-node-architecture-design.md` - setup mode and the
  connection key.
- `2026-08-13-rota-dinamica-e-implantacao-incremental-design.md` and its plan -
  dynamic route and incremental deployment; carries the laptop/CUDA server p50/p95
  table (9.6 vs 7.2 ms) behind the 25% / 2 ms switch margin.
- `2026-08-20-batching-continuo-inferencia-distribuida-design.md` - local
  scheduler, multi-item frame and batch sync in Tensor Parallelism.
- `2026-08-24-chat-telemetria-design.md` - wire inventory and the decision to
  keep the data in `x_pom`, `/health` and an authenticated RTT route.
- `2026-08-26-auditoria-de-requisicoes-design.md` - persistent prompt audit in
  SQLite, the non-secret key identity, FTS5 and the two-step record cycle.
- `2026-08-26-protecao-termica-gpu-design.md` - deployment thermal protection:
  temperature policy and hysteresis machine.
- `2026-09-02-pom-db-unificado-design.md` - the unified local store: one shared
  SQLite connection per process, explicitly serialized mutations, timeout only
  as a safety net.
- `2026-09-02-rdma-wizard-primeiro-acesso-design.md` - the first access wizard.
- `2026-09-08-inferencia-trace-outside-node-design.md` - per-inference trace,
  local clocks, parallel span math and the `outside-node`/TTFT breakdown.
- `2026-09-23-docker-prebuilt-llama-design.md` - Docker mode with prebuilt
  libraries from the fork.

Plans worth knowing: `2026-09-12-windows-crash-root-cause.md` (`model_loaded`
tracing and the native `/health` boundaries),
`2026-09-14-reload-idle-runtime-v2.md` (reload after idle-unload),
`2026-09-15-hybrid-memory-decode-segv.md` (recurring Kimi-K3 rollback),
`2026-09-15-moe-freetoken-performance.md` (heterogeneous CUDA hot path),
`2026-09-16-enterprise-heartbeat-schema.md` (rich heartbeat contract),
`2026-09-21-docker-backend-variants.md` (CUDA, Vulkan and CPU image variants).

Naming: `specs/YYYY-MM-DD-<slug>-design.md` and `plans/YYYY-MM-DD-<slug>.md`.
One exception, `2026-08-12-inferencia-pela-internet-mudancas.md`, is a
proposal written before the convention ("no decision taken").

## Defects noted

None open. The root `README.md` is presentation: for architecture, the root
`AGENTS.md` and `docs/subsystems/*` are authoritative.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
