# AGENTS.md - crates/node/src/backend/

## What this folder is

The HTTP backend of the `pom` node: every axum router and handler the
binary serves. It owns the single generation path (`gateway::generate_stream_with_stops`,
which routes to the **distributed pipeline** - there is no fallback to a local
or peer Ollama, a missing plan fails the request), the gateway state all
handlers share, the two protocol adapters (OpenAI and
Anthropic), the operator control plane (`/admin/*`, `/api/ui/*`, `/api/ui/deploy/*`,
`/api/ui/undeploy`),
slice transfer over HTTP `Range`, and the persistent request audit. No P2P, no
inference, no model loading lives here - those are `network/`, `inference/`,
`models/`. All state is `crate::gateway::GatewayState` (or `crate::auth::AuthState`
where a handler only needs the config); handlers are axum functions, most
behind an auth extractor.

The gateway keeps the pipeline transport (`/api/generate`, `/v1/*`, the relay
callbacks) separate from the administrative surface (`/api/ui/**`, `/admin/**`, the
static UI), and the two never share an authorization rule. The rules that decide who
may call what - the profile table, the in-memory browser session and its single
`ADMIN_SESSION_TTL`, the signed control envelope between nodes, the per-route
`RouteScope` (`PrincipalOnly`, `AnyAdmin`, `ServingMember`, `AnyMember`,
`LocalOnly`, `HandlerAuth`) and the `x-pom-admin-error` header the browser reads to know its
session died instead of printing a raw 401 inside whatever screen was open - are
`docs/subsystems/http-api.md`. The deployment lifecycle, the idle wake
(`reload_model`) and the role gate (`deployment_authority` /
`ProfileAction::ManageDeployment`) are `docs/subsystems/deployment.md`. The
generation pipeline, the two output budgets and the KV session rules are
`docs/subsystems/inference.md`.

The `webui` route registry is organized under `webui/routes/`: `common.rs` holds the
routes shared by every profile, `infer.rs` the inference-profile actions, `node.rs`
the local node/model operations and `admin.rs` the administrative and lifecycle
routes; `routes/mod.rs` is the only composition point. That organization is for
legibility - the gateway's central profile policy remains the effective authorization
boundary.

## Files

- `auth.rs` - auth extractors. `AdminAuth` protects the administrative surface
  with the real peer address, an Argon2id-verified human password, a
  server-side in-memory session cookie, or the explicit internal-node channel.
  Passwords are never persisted in plaintext; sessions use a cryptographically
  random identifier, bind to the current password hash, expire after the single
  `ADMIN_SESSION_TTL` value and never
  persist in the browser or on disk. `ConnectionAuth` (connection-key bearer
  only), `InternalNodeAuth` (the signed envelope, or that key plus an internal
  marker from a caller the transport proves is a member, `caller_is_member`
  against `Network::is_member_key`, set once by `set_member_oracle`; plain HTTP
  from loopback needs no proof, and a test bench without the oracle keeps the key
  alone), `ApiKeyAuth`
  (active inference API key only, from `authorization: Bearer` or `x-api-key`),
  `AuthState = Arc<RwLock<Config>>`.
  When both headers are present they are checked independently, so an invalid
  one cannot hide a valid one; a valid API key still supplies its non-secret
  `Caller`. Only a key this node minted is valid: keys never leave the node,
  no registry exists elsewhere and no other node is asked about an unknown
  key. The inference
  port does not use the shared `connection_key` alone on private networks; on public
  networks it demands TLS transport, the internal marker and a member caller, while the
  result callbacks validate the per-frame `Ticket`.
- `/handshake` is the admission proof of the private discovery. The key never travels: the
  node answers a challenge with a signed HMAC of it (`session.rs`), and a node without a key
  answers nothing, so an empty key never turns a setup process into a member of the swarm.
- `gateway/` - the module split into state/selection, budgets and generation types,
  generation, snapshots/planning, HTTP and tests: `GatewayState`,
  `start_gateway_with_ready`, `generate_stream_with_stops` /
  `generate_blocking_with_stops` / `spawn_logged_generation_with_stops`, o `StopMatcher` incremental,
  prompt-budget math, the
  direct-reply relay route (`POST /v1/relay-result`), the bounded remote-token
  callback route (`POST /v1/remote-stream-result`), the deployment-wide KV
  snapshot fan-out (`resolve_plan_for_model`, `snapshot_deployment`,
  `restore_deployment`, `erase_snapshot` - all-or-nothing across hops, mirroring
  `rewind_sessions`/`release_sessions`; used by `/admin/slots`), the persistent
  prefix cache (Phase 3, `slot_persist_prefix_cache`): `build_prefix_disk_index`
  builds `GatewayState.prefix_disk`, `generate_stream_with_stops` spills each
  retained turn to disk in a detached task and, on a memory miss, restores the
  longest strict-prefix snapshot before prefill (`prefix_reuse_status = "disk"`),
  the adaptive chunk tuner fits each hop and link as fixed + per-token cost
  from completed prefill traces for each model/route and simulates the chunked
  pipeline for the next prompt's size to pick the chunk (see
  `docs/subsystems/inference.md`, "Adaptive chunk"), capped by
  `prefill_chunk_tokens` and `n_batch`, and the `testkit`
  test scaffold (peer/state_with/body_of).
- `db.rs` - the only local SQLite store, internal to the crate: schema created at startup,
  WAL/timeout, deployments, API keys and revocations, thermal preferences,
  the Hugging Face secret, the catalog's durable history and the conversation history. Each
  effective admin keeps its own replica of the audit. The
  cluster process log is not here: the logs plugin keeps it in its own
  database, and the schema drops the old `cluster_logs` tables. The history identifies
  whole models by the local path, without
  hashing the weights during the catalog registration; content hashes live
  on the explicit boundaries of deployment, slicing and transfer.
  `PomDb` keeps one connection configured at startup, reused
  by every operation; an explicit mutex serializes writes and the same
  connection also does short reads in order, eliminating the opening and the DDL per
  operation. The path comes from `config::database_path()`; `POM_DB_DIR` points
  to a volume and, without it, the database sits at the project root.
  `known_public_networks` is the table of every public network this node has
  heard of (`remember_public_networks`, `known_public_networks`): one row per
  fingerprint with the listing's JSON view, folded search text and first/last
  seen times, pruned by age and capped on every write.
  `local_setting_keys_with_prefix` lets the runtime dynamically restore the
  persisted plugin codes without keeping a compiled list in the binary.
- `webapp.rs` - serves the frontend bundle from disk at `/admin-ui/`; `/`
  redirects there and the static surface is covered by the same local-only
  boundary, while nothing is compiled into the binary.
- `model_provider.rs` - an API-agnostic wrapper for the CLI's status, authentication, search,
  license, details, space and remote downloads. It injects `DynModelProvider`, uses
  `webui::Job` as persistent state, a `CancellationToken` per download and
  a two-process semaphore; it assembles no commands and exposes no token or stderr.
  Each download picks the first non-empty models folder resolved from the
  configuration, passes that folder to the provider and records the destination in the job. The
  storage response exposes the eligible folders. The structured status in
  `GET /api/ui/model-provider/status` lets the UI tell an absent CLI, a required login and an
  authenticated session apart.
  `GET` and `POST /api/ui/model-provider/config` expose only the mask and the
  effective source of the token, and the POST saves the secret in `pom.db` with a restricted
  mode and updates the `HfTokenStore` without a restart. The secret never appears
  in a log, an error or a response.
  The handler clones the `ModelFile` before creating the `'static` task, it does not keep
  provider-response borrows in `tokio::spawn`, and the local file searches borrow the name on
  each iteration.
- `webui/` - the `/api/ui/*` control API behind the dashboard, split into
  `mod.rs`, `routes/` (common, infer, node and admin), `catalog.rs`, `cluster.rs`, `params.rs`, `jobs.rs`,
  `jobs_apply.rs`, `slicing.rs`, `lifecycle.rs`, `setup.rs`, `trust.rs` and test fragments:
  catalog, cluster view, slicing, apply/launch, merge, jobs, release, leave,
  storage paths, thermal policy (`GET/POST /api/ui/thermal`), local auto-discovery
  preference (`GET/POST /api/ui/network/auto-discovery`), cluster process
  logs (`GET /api/ui/cluster-logs` answered by the logs plugin, 403
  `logs_plugin_required` without it; `POST /api/ui/network/logs`, primary
  only, refuses turning collection on unless the logs plugin is active on that
  node, while turning it off always works), slice retention
  and the authenticated CRUD of the conversation history (`/api/ui/chat/history`).
  `GET /api/ui/plugins` (all profiles) lists local state for independent
  plugins restored or seen by the runtime (`crate::plugins`), each with its
  own features, validity, authenticated usage and updates.
  `POST /api/ui/plugins/activate` activates every `plugin_code` granted by the
  server in one batch; `POST /api/ui/plugins/:code/refresh|deactivate|update/check|update/apply`
  acts on one plugin. GitHub repositories are local to the node and use the
  PrincipalOnly routes `GET/POST /api/ui/plugins/github-repositories`,
  `POST /api/ui/plugins/github-repositories/:id/refresh`,
  `DELETE /api/ui/plugins/github-repositories/:id` and
  `POST /api/ui/plugins/github-repositories/:id/install` with
  `{release_id}`. POM queries GitHub directly and does not ask the license
  server whether a plugin requires a license. Sources are stored in
  `plugin_github_repositories`; installations use
  `plugin_public_install:<code>` without a lease or grant. Typed preferences
  stay local at `/api/ui/plugins/:code/preferences`; these routes do not
  activate a license. With `POM_DEV_MODE=1`, `GET
  /api/ui/plugins/dev-sources` lists mounted sources and
  `POST /api/ui/plugins/dev-sources/:repository/rebuild-install` builds and
  installs or rebuilds a local plugin without prior activation. Both routes
  are PrincipalOnly; the handler accepts mounted direct-child `pom-plugin-*`
  sources only after validating their manifests and plugin codes, uses fixed
  Cargo commands and never exposes compiler output. A development-only installation persists in
  `plugin_dev_install:<code>` and starts the host without granting licensed
  features. If a license or GitHub installation already exists, its
  entitlement is preserved. The browser reads the commercial catalog through
  `GET /api/ui/plugin-catalog[/:code]` (`plugins::plugin_catalog`), which
  passes on only `q`, `page` and `per_page`, sends the store no credential and
  returns its JSON as it is. Purchases redirect the operator to the public store with the
  plugin code. Updating the `pom_core` release is a separate boundary in
  `crate::pom_core_update`: `GET /api/ui/pom-update/status`,
  `POST /api/ui/pom-update/check` forces an on-demand check, and
  `POST /api/ui/pom-update/apply` requires explicit confirmation before
  downloading the release and restarting. The cluster view transports
  `SystemLoad.gpus[].power_watts` as optional telemetry and keeps it `null`
  when the system does not report board power. The
  `/api/ui/network/geo/{countries,cities,nearest}` routes read the offline
  city database embedded in the binary (`utils/geo.rs`) for the location
  editor and use `LocalOnly`; they do not create or change network data
  and look nothing up on the internet. `GET /api/ui/deployments` is `ServingMember` so a
  `worker` profile can see the read-only inspector, while mutations and deployment actions stay
  under the existing administrative policy.
  The authority surface exposes the presentation contract in
  `/api/ui/network/admin-authority`. There is no route that changes a
  node's role in place (infer-to-node and node-to-infer were removed): a role
  is set at admission. Authority offers share the offers feed so the browser's bell does not
  choose the granting or signing mechanism.
  **The trust anchor predates all of that** and lives in `webui/trust.rs`:
  `GET/POST /api/ui/network/trust-anchor` shows the anchor's fingerprint and the anchored/not
  yet anchored state, and installs the chain the operator
  carried over. The route is `HandlerAuth` on purpose - an unanchored `profile: admin`
  does not pass `PrincipalOnly`, which is exactly the state where the operator needs it - and it
  does not demand the cluster's connection key, for the reason recorded in the file's header.
  The installation's three locks are the server's: loopback measured in `serve::PeerAddr` (never
  `admin_exposure`, which may be open to the network, and never `X-Forwarded-For`), a node still
  without an anchor, and the fingerprint typed by the operator checked against the chain pasted.
  The validation and the check are `admin_trust`'s
  (`parse_transported`, `fingerprint_matches`, `adopt_anchor`), the same ones
  `pom admin-anchor` uses: there is no second policy.
  `GET /api/ui/network/trust-share` serializes the local chain, including the
  `setup` block signed by the wizard. If an old primary chain does not have
  that block, the route recovers the data persisted in `pom.json`/`pom.db`, signs it with the
  local principal and writes the chain before the download; a secondary authority cannot
  fabricate that metadata.
  `POST /api/ui/chat-key` is allowed to `node` because it only creates the local
  key `pom-<node_id>`, which stays on this node like every key (the secret in
  `local_secrets`, `PomDb::node_api_key`, swapped when it no longer matches an
  active key). Joining or leaving a network keeps the node's keys.
  `POST /api/ui/network-chat` checks the browser's key locally and clears it
  before the action goes to the principal.
  The read `/api/cluster/network-settings` is internal and read-only: a
  node adopts the complete `NetworkSettings` document from the principal, including
  visibility and metadata, after the first authenticated `Announce`.
  `GET /api/ui/infer/heads?model=...` is the only dedicated read of the layer-zero
  heads: it delivers the candidates and the planner's choice, without general peers or control
  actions. A head still loading its model (`is_warming`) is never a candidate. The capacity gate
  (`request_output_limit_for_head`) plans the route from the head the request chose
  (`inference_head`), as generation does. The route is `LocalOnly`, so admin and infer can read it from their own screen. Each candidate's load
  comes from its own `/api/ui/node` and stays `null` when the report does not arrive, so the UI
  shows it unavailable instead of zero. The RTT's validity is five minutes; expired values
  appear as a diagnostic, but they do not enter the generation's selection.
  `NodeInfo` carries the same thermal policy used by the runtime. It also
  carries `pom_dev_mode`, which is true only for the exact startup environment
  value `POM_DEV_MODE=1`; this flag controls only local UI visibility, so every
  deploy boundary preserves development-only runtime fields received in the
  request. A missing inference mode uses `dense` (`llama.cpp`) by default.
  `slice/plan` and `slice` accept the
  explicit `tensor_parallel` mode with `tp_size`; the job extracts every rank
  and seals `tp_parts` in the same source-hash manifest used by pipeline
  `parts`. Jobs and the cluster view use conditional JSON responses:
  matching `If-None-Match` returns 304 without a body, and the cluster keeps
  a 2s fingerprinted snapshot cache that bypasses its TTL on known state
  changes.
  The read of each peer in `/api/ui/cluster` walks `gateway_candidates()`,
  not a single address, and feeds the `RouteTable`'s failure memory
  (`mark_failure`/`mark_success`) - the same one the inference path uses. With no usable
  candidate it calls `Network::request_peer_addresses` and returns
  `rediscovering: true` instead of `unreachable`, and `model_inventory[]` names in
  `unreadable_nodes` the peers whose coverage it depends on without being able to read them.
  `PeerView.load` is the in-flight load reported by the peer itself: for the local node it comes
  from the local `LoadTracker`, and for remote peers it comes from the JSON of
  `/api/ui/node`. A missing or invalid remote read stays `null`, because
  the local tracker's zero only means no request went through
  this gateway.
  `model_inventory[].complete` and `/api/ui/chat/capabilities` only confirm a
  pipeline when every contributing peer has `runtime_capacity` confirmed
  by `/health` or is `idle`; gossip capacities during the boot stay
  visible as coverage, but they are not presented as ready.
  Temporary IPv6 addresses run on their own: see `docs/subsystems/p2p-network.md`,
  "The address that stopped answering".
  `plan` reads the GGUF header from the node that owns `model_path` and, when
  calculating budget-driven ranges, includes the configured context, sessions
  and K/V cache types in the byte arithmetic. The response exposes weight,
  KV and total bytes per shard; absent KV metadata is an explicit bad request.
  `POST /api/ui/deploy/capacity-preview` (`capacity_preview_endpoint`,
  `ConnectionAuth`) is the deploy-time, per-step, peer-local version: it
  resolves a `NodeConfig`/`EngineConfig` for a locally-present slice, reads
  the GGUF layout and live `SystemLoad.gpus[]`, and returns projected VRAM per
  GPU. Weight and KV are calculated analytically; the endpoint never calls
  `llama_backend_init`, `llama_rs_fit_params` or the real-load `fit_plan`.
  `fits` is therefore an estimate. When the raw projection does not fit,
  `degraded` is true only when `fit_memory` is enabled and the requested layer
  count is the default all-layers value, which signals that the real load may
  trim the request; it does not claim a native result. The response note tells
  the operator that the real load confirms the final layout. KV quantization is in the byte math
  (`cache_type_k/v` from the installation); KV only counts toward VRAM when
  `offload_kqv` is on. A process admits four previews at once and returns
  `429` immediately when all slots are occupied. `split_projection` always
  divides projected weight+KV bytes proportionally to current free VRAM. The
  compute buffer is deliberately left out because the estimate is a planning
  signal, not a replacement for the real load.
  `RuntimeParams.prefill_max_inflight_chunks` is persisted with the other
  inference knobs and sets the prefill pipeline depth (1..8, default 8).
  `RuntimeParams.fit_memory_margin_mb` is also persisted and controls the
  per-device safety reserve used by llama.cpp's automatic layer fit (default
  256 MiB).
  `RuntimeParams.unload_if_idle` and `idle_minutes` are persisted with each
  deployment; the runtime applies them independently on each peer and the
  registry reports `idle` while a live peer has released its model.
  `/api/ui/peer/:node_id/runtime` forwards the inference `/health` unchanged,
  including `mtp_ready`, `mtp_allocation` and `mtp_unavailable_reason`, so the
  dashboard can distinguish MTP in VRAM, MTP in RAM, a model without `nextn`,
  and a memory failure.
  GGUF multipart sources are read through models::multipart::GgufSource:
  slice plans and extraction use all tensor parts, and model identity hashes
  the complete ordered set. An incomplete set is a concrete bad request, so
  the UI cannot start a one-file slice by accident.
  `POST /api/ui/slice/plan` and `/api/ui/slice` also accept one `range` with
  explicit start/end layers. The handler validates architectural boundaries,
  estimates the real output against the configured destination volume, and
  appends the resulting part to the source-hash manifest. Existing ranges are
  preserved and a matching file is reused only after SHA-256 verification.
  The same preflight checks architecture-specific output contracts before a
  plan or job is accepted. `deepseek4` requires its complete output head in a
  final slice; non-final ranges repeat the real required head weights, and the
  plan exposes their per-slice disk cost. If the source is missing any tensor
  that must be copied, the refusal names it before writing any payload.
  Slicing does not invoke automatic retention, so it cannot silently remove an
  older range. Jobs report named phases, source bytes read, current file, rate,
  ETA when measurable, terminal errors and partial outputs.
  (local or forwarded to a peer).
- `RuntimeParams.hidden_state_dtype` is persisted and forwarded through the same
  apply/launch path as the other global knobs. f16 stays the default, f32
  is the lossless option without extra negotiation; `DeployPlan.capabilities` for
  f8/i4 blocks the lossy formats when some peer does not announce support. The redeploy queries
  `/api/ui/node` of every participant before reconfiguring any node,
  avoiding a partially mixed deployment.
- **Deployments are versioned** (`Deployment`, `DEPLOYMENT_VERSION = 2`,
  `models/deployment.rs`): `versions` keeps every `RuntimeParams` that already ran
  under the same `deploy_id`, `active_version` points to the one that is live and `nodes` is
  the flattened list of the active version. `POST /api/ui/deploy` (`deploy_endpoint`),
  before minting an id, looks in the local `pom.db` for a deployment of the same
  shape (`deployment::matching_shape`: `kind` + a proven `DeploymentIdentity` +
  the set of `(node_id, range, tp_rank)`); finding one, it passes
  `RecordPlan::ReuseVersion` to `run`, which calls `upsert_version` (it reuses the
  version of equal `params`, otherwise it appends `max+1`). Without proof of identity,
  `RecordPlan::New` with a new id. The response carries `deploy_id` and `reused`. Every
  record read (`PomDb`, `find_record`, `list_deployments`) runs
  `ensure_versions` to synthesize the v1 of a legacy v1 record. `find_record`
  gathers the local copy and the copies of the participating peers and picks, by
  `activated_at` and then `active_version`, the current version before
  `redeploy`/`activate` use it.
- `POST /api/ui/deployments/:id/redeploy` (`redeploy_endpoint` ->
  `run_redeploy`, `deploy/`): same `deploy_id`, same machines and ranges.
  `RedeployRequest` carries `params` (optional), `version` (optional) and
  `per_node`; give one of the two (400 when both are missing or `version` does not
  exist). `RedeployRequest::resolve` runs before the TP detour: `version`
  reactivates a registered version without appending; new `params` append - or
  reactivate - by the same comparison as `deploy_endpoint`; it mutates the record and
  returns the base parameters. The bring-up uses the active version's `no.config` (the
  parameters with the per-machine adjustments embedded), so redeploying a
  version gives each machine back what it was running. It photographs each node's live
  `NodeConfig`, **unloads** every peer announcing the `deploy_id`
  (`release_one` in parallel), and **brings up one machine at a time** waiting for
  each one to report `ready` (`wait_for_peer_serving_with_timeout`). It works
  even if a node was not serving: `/api/ui/reconfigure` accepts an
  optional `ReconfigureSlice` (`webui/`) and `reconfigure_local` rebuilds the
  `NodeConfig` from it. The file is resolved in three steps: the `model_path`
  photographed before the unload; the record's `part_sha256`
  (`transfer::find_part`); and, when the record has no part hash (whole
  model, or a slice from an older round), the local catalog matching the model's label + the
  layer range (`resolve_deployment_slice_file`). Only when all three fail is it an error naming
  the range. `DeploymentNode.source_model_path`
  (`models/deployment.rs`, `#[serde(default)]`) records the file each node
  loaded, so the redeploy finds it even with the node offline. `finish_redeploy`
  records per machine what was accepted, synchronizes the active version's snapshot
  (`commit_active_nodes`) and publishes it; `job.outputs` carries the result per
  machine. Tensor Parallelism still uses the older path
  (`run_redeploy_in_place` -> `reconfigure_one`, without an unload).
- `POST /api/ui/deploy/node/options` traverses the `target_node_id`'s real catalog
  and returns only the sliced ranges and the whole GGUF materialized on that machine, plus the
  local GPU/KV defaults. The screen does not use the old plan as a fallback when it already
  knows the model's group.
  `POST /api/ui/deploy/node` validates that option again on the target, applies
  `cache_type_k`/`cache_type_v` through the local or remote API and
  launches only the chosen machine. When there is a `deploy_id`, the operation appends
  a topology version and changes `kind` to `mixed` when a whole head and
  a sliced one come to coexist. `POST /api/ui/deploy/node/remove` unloads
  only the target and appends a version with the remaining machines; a partial removal
  keeps the record serving, while the last one writes an empty version and leaves
  it out of the active deployments' selection. The empty snapshot is forwarded
  to the peers that already knew the deployment so the removed machine is withdrawn without
  touching the others' inference.
- `gateway/generation.rs` - `log_early_outcome` guarantees that every accepted
  request leaves **exactly one** outcome line. The early exits of
  `generate_stream_inner` (a cancellation before the start, the prompt budget,
  no valid pipeline, vision without a projector) were mute: the record showed the
  request arriving and the output-limit adjustment, and then nothing - neither
  `Pipeline planejado` ("Pipeline planned") nor an error. For the client that is a hung
  request, and that is what put a backing-off agent into repeating the same request in
  production on 2026-09-08.
- `profile.rs` - `NodeProfile` is the durable product profile and the one source
  for network visibility and profile transitions. The web UI serializes it
  through `GET /api/ui/node`. Infer nodes can read a local-only network view;
  a role never changes in place. A secondary admin keeps management access
  but cannot promote another admin, and is not the principal: it decides
  admissions and removes workers and visitors, but the admission list stays
  on the principal, which it reads and sends its decisions to.
- `admin_trust.rs` - the protected, durable Ed25519 authority chain. It stores
  the manually anchored root, one-level secondary grants, signed demotions,
  transfer offer and acceptance state, and full-chain reconciliation. Consumed
  nonces live in a local replay ledger beside the chain
  (`replay_path_for`: `<chain>.replay.json`), never in the shared document:
  control-envelope entries expire with the envelope's `CONTROL_WINDOW_SECS`
  (5 min) window, admin-mutation entries (grants, demotions, transfers, infer
  promotion) have no clock and are kept. `ControlEnvelope.issued_at` is signed
  and checked against that window on both verify paths. A profile set to `admin` without a valid local chain remains
  without administrative capability. `is_anchored`/`anchor_file_exists` are the
  anchored-versus-not judgement the UI shows; `parse_transported`,
  `anchor_fingerprint` and `fingerprint_matches` are the single manual check
  shared by `pom admin-anchor` and the screen (it normalises only human
  transcription - spaces, colons, case - never the value); `adopt_anchor`
  writes a transported chain onto a node that has none and refuses a node that
  already has one, because replacing a root is manual re-anchoring on the
  machine (`--replace`), not an interface gesture. A chain created by first
  access also carries a signed `setup` block with the network name, visibility,
  access policy, `relay_hints` (dedicated relay URLs with host names, never IPs)
  and federated-directory replicas, so a joining node does not retype network
  identity data. The network's location is not in it: country, city,
  coordinates and the discovery label are settings the principal changes at
  any time and advertises in the directory record. The block carries no IP: a joining node finds
  the principal by its key through pkarr (`main::spawn_pkarr_bootstrap`), and
  setup copies `relay_hints` into its `relay_peers`. `ensure_setup_metadata` repairs only a missing setup block
  from the local administrator configuration, and only when the local
  principal is the current primary.
- `deploy/` - `/api/ui/deploy/*` + `POST /api/ui/undeploy`, split into
  `mod.rs`, `routes.rs`, `types.rs`, `survey.rs`, `planning.rs`, `preview.rs`,
  `launch.rs`, `lifecycle.rs`, `node.rs` and test fragments: the capacity survey,
  the simple/full strategies, the directional RTT, coverage, the projected load,
  the machine choice and the sequencing of the copies; it re-surveys the cluster on deploy and
  keeps the technical `group` separate from the operational `deployment_name`.
  The survey's in-flight load comes from each machine's `/api/ui/node`. The internal
  struct keeps zero only so the planner can order machines without a read,
  but `load_known` keeps that fallback from reaching the `MachineView` or the UI;
  in that case the published load is `null`.
  After assembling the steps (and only with `blocked` empty), `plan_endpoint` calls
  `enrich_capacity_previews`: one `capacity-preview` request per locally materialized
  `Step` (not `merged`, with a `target_path`), aggregated
  per machine in `MachineView.capacity` (a sum per GPU). The fan-out uses at most
  four simultaneous calls; TP and steps still to copy/merge stay without a
  projection. It is a warning, never a block: the screen shows the overflowing machine in pink
  and asks for a `Confirm` before deploying.

  When a whole model selected still has no `total_layers` in
  `/api/ui/models`, the survey calls `model-metadata` only for that path;
  the neighbours' `metadata_complete` can still be false without blocking the
  deployment. The complete layout is only asked for when there is an explicit prefix
  cut.
  Before transferring or launching, it proves the identity by `pom_id` or by the GGUF's hash,
  releases only the provably equal deployment and waits for each peer
  to become idle within a deadline; identity refusals, unload failures and
  timeouts stay in the job with the peer and the reason.
  In `tensor_parallel`, it prefers `shard_sets[].tp_parts`, assembles a `TpTarget` per
  rank and transfers by the original file's hash; the `apply` uses
  `skip_split: true` so it does not slice again. Without `tp_parts`, it keeps the
  runtime path that extracts each rank from a whole model.
  `route_options` builds a graph of the selected machines whose execution sub-ranges are
  contiguous; it keeps more than one entry when there are alternative
  paths and it never invents a jump to a later layer. Overlapping loaded
  ranges are allowed when they belong to distinct paths;
  the coverage is validated by the executable path, not by the union of the files.
  `undeploy` takes a whole pipeline off the air.
- `webui/https.rs` - the local CA for the interface (`LocalOnly`):
  `GET /api/ui/tls/ca` (fingerprint, trust state, a past refusal, whether this
  request may install), `GET /api/ui/tls/ca.crt` (the public certificate as a
  download) and `POST /api/ui/tls/ca/install`, which also demands loopback
  (`serve::PeerAddr`) because it opens the system dialog of the node's own
  machine. The CA is `network/web_ca.rs`, the installers `src/trust_store.rs`.
- `webui/sidecars.rs` - `GET/POST /api/ui/model-sidecar-links`: the
  principal's model -> projector / MTP draft links (`model_sidecar_links` in
  `pom.db`). A secondary reads them from the principal and keeps the last copy
  (`stale`), and forwards a save there. `load_sidecar_links` is what the deploy
  preview and launch read.
- `deploy/sidecars.rs` - `attach_sidecars` fills `Plan.sidecars`: the
  compatible files of the network with every holder, the head's projector and
  the tail's MTP draft (screen choice, else the link), whether each is already
  on its target or copied from a holder, and the refusal when nobody has it. A
  compatible external draft turns `capabilities.mtp` supported for a model
  without `nextn`. `lifecycle::run` adds the copies to its `pulls` and sends
  `mmproj_file` / `mtp_draft_file` so the target resolves the path on its own
  disk (`jobs_apply`). The pull request to each target and the polling of its
  copy (`node.rs::wait_for_transfers`) are `control_post_json`/`control_get`
  calls to the peer, so they end in the p2p tunnel for a target behind NAT;
  `node.rs::peer_of` finds the peer. `transfer::servable_path` serves a path outside the
  shard folder only when it is in the node's sidecar inventory, and a finished
  download invalidates the receiver's catalog.
- `webui/membership.rs` - membership tokens at run time (the format is
  `crate::membership`, `src/membership.rs`: a compact EdDSA JWT naming the
  member's node and key, its role, the network and a 10 min expiry, checked
  against the chain alone). `internal_token` issues one on any admin for an
  approved admission; `spawn_token_keeper` renews this node's (none on an
  admin) and sets it in the Announce. On a secondary admin,
  `spawn_roster_keeper` mirrors the principal's admission list
  (`internal_roster`), and `forward_decision` sends its admission decisions
  to the principal (`internal_decide`, applied by `apply_decision`, which
  refuses to remove an admin). `NetworkChat::admitted_role` reads the own
  list and the mirror, ignoring an approval not newer than the member's last
  ban. `ban` signs a revocation on any admin and pushes it to every peer
  (`push_revocations`); `internal_revocations` merges a caller's list and
  answers with its own, and `spawn_revocation_keeper` trades lists with one
  admin per tick. `on_revoked` makes the principal forget the member and
  rotate the key.
- `webui/kv_cache.rs` - this machine's KV cache for the dashboard:
  `kv_cache_disk` measures the snapshot directory (`NodeInfo.kv_cache`) and
  `POST /api/ui/kv-cache/clear` (PrincipalOnly, also empties the `strata-sessions/` subfolder, forwarded by `node_id` like
  `/api/ui/leave`) deletes its `.bin`/`.json` files, resets
  `GatewayState.prefix_disk` and clears the inference server's host-memory
  cache. See `http-api.md`.
- `transfer.rs` (1542) - slice push/pull over HTTP `Range` with resume and MD5;
  offers carry only a `stat`, the digest is computed by the receiver. Pushes
  accept only a catalog basename in `filename`, resolve it server-side and
  require `.gguf` or no extension. Manifest v1 are merged by `pom_id` on the receiving peer,
  preserving the local `parts` and `tp_parts`. **The verdict goes back to whoever
  offered** (`receive_answer`), and every outcome passes through
  `TransferState::finish`, which stamps `finished_at`. A job carries the slice's label
  (`SliceLabel`: model + range), read from the `.pom` - a hash is not a name.
  When no direct address connects, `run_download` falls back to
  `download_over_tunnel`: closed 4 MiB pieces through the p2p tunnel, only
  over a connection that may carry a file - **never across a public relay**
  (`Network::transfer_links`, and the 421 the serving node answers). `download`
  serves a closed range as one piece of the same job.
- `decisions.rs` - typed decisions, `POST /v1/systemone` and `/v1/decisions`
  (Jev API shape). Each question is one blocking generation with
  `max_tokens=1`, thinking off and a `prefill::DecisionProbe`: the last peer
  returns the probability of each single-token label (`A`.. for choice,
  `0`.. for score, `Yes`/`No` for noul) and this file turns them into typed
  answers and confidences. The prompt is ordered fixed system turn ->
  question head (instructions + answer format) -> compact state -> options;
  the text up to the state is the prefix-cache key and the head's retain
  boundary (`retain_prefix_bytes`), so repeated questions over new states
  reuse the question's KV. See `docs/subsystems/inference.md` 3.9.
- `openai.rs` - OpenAI-compatible `/v1/*` on top of the generation path;
  `max_tokens` of the request is `min(requested, n_ctx remaining)`, with
  `DEFAULT_MAX_TOKENS=512` only as the default of whoever does not ask. The
  deployment's `max_new_tokens` does **not** enter that arithmetic: it is the ceiling of one
  **leg** of the continuation loop (see `gateway/` below). The reduction is
  recorded and returned in `x_pom.output_limits`
  (`requested`/`effective`/`deployment`/`context_available`/`leg`),
  and `x_pom.continuation_legs` says how many legs the answer stitched together.
  Streaming and blocking use the same finish-reason order, and it has no
  exception: an exhausted **total** budget is `length`/`max_tokens` even with a tool call read,
  because reaching the ceiling is a terminal stop; an exhausted leg never is. When the cut falls
  **inside** a call, it comes back structured and incomplete (`x_pom.truncated_tool_call: true`),
  which is what the two providers return - never raw markup, never a discard. The audit line of
  that result is `incomplete`,
  while a transport failure stays `partial`. `/v1/models` exposes
  `context_length` and the `top_provider.max_completion_tokens` compatible with
  OpenRouter, which is the **request's** capacity derived from the `n_ctx`, not the
  size of one leg. The available context shrinks the budget when the prompt already occupies
  part of the window. `GenEvent::Reasoning` is never fed to the visible-content
  scanner: OpenAI chat emits it as `reasoning_content` (streaming and blocking),
  which Pi renders as a thinking block; completion-token accounting still uses
  the engine's total generated-token count.
  `messages.rs` accepts `output_config.task_budget` (advisory, it never cuts) and
  injects the countdown marker into the prompt; see `http-api.md`.
  **A call's arguments come out in fragments** (`CallArgsStreamer`),
  as the model generates them: `eager_input_streaming` per tool in
  `/v1/messages`, always in `/v1/chat/completions` because OpenAI has no
  switch. Concatenating the fragments is always the string the non-streaming path would
  produce, and the transmitted prefix only grows - there is no un-sending an SSE fragment.
  The Chat may send an `inference_head` with a layer-0 `node_id`; the gateway
  validates the live candidate, pins that head in the planner and repeats it in
  `x_pom.source`/trace, without re-choosing by RTT or load. The Chat sends it only when the
  user picks a head in the Chat (`headChoice`); the screen's default is not sent. Without a
  pin and with more than one layer-0 head of the model (`generation.rs::auto_head`): a request
  whose session is already held by a live head goes back to that head, reusing its cache;
  otherwise the heads take turns round robin (`routing::fastest::rotate_head`, per-model turn in
  `pipeline::next_head_turn`), and a head with generations in flight is passed over for the next
  free one, or the least loaded when all are busy.
  The capacity published in `/v1/models` and used by
  `request_output_limit_for_model` is the smallest `RuntimeCapacity` among the peers
  of the pipeline chosen for the model/deployment. An idle peer in that
  pipeline is woken by the `reload_model` event in parallel to the other idle
  peers before the first prefill; the barrier waits for `/health` for up to 15
  minutes and fails explicitly if the peer does not come back. A live peer of an alternative
  route does not block the request. The runtime only
  announces that value after the effective `/health`; apply, error and unload take
  the capacity away during a restart.
- `messages.rs` - Anthropic Messages API (`/v1/messages`), same output-limit
  and finish-reason rules, with `x_pom.output_limits` and
  `x_pom.continuation_legs` on blocking responses and the final streaming
  `message_delta`.
- `continuation_lab.rs` (`#[cfg(test)]`) - the bench of the continuation loop: two fake
  hops, the real `/v1` routes on a TCP socket and `reqwest` on the other
  side, including reading the SSE byte by byte. It proves the output comes back whole, that
  the stitching does not appear as an intermediate `finish_reason` nor as a second
  `[DONE]`, that the tool call arrives with complete JSON, and that the ceiling is absolute for a
  looping model. The fragmentation proof does not depend on a fixed fraction of the wall clock,
  because the scheduler and the socket's grouping are not part of the SSE contract.
- `tool_call.rs` (725) - tool-call text rendering + `TagScanner` output scan.
- `devupdate.rs` (647) - env-gated remote `git pull` + rebuild + self-restart.
- `admin.rs` - the operator's control plane: nodes, blocking, keys and
  authority. `promote_node_to_admin` refuses an inference target before
  signing, records the grant, delivers the chain to the target and takes the
  grant back with a signed demotion when the target answers without becoming
  an admin; silence leaves the grant standing (202, `pending`).
  `adopt_trust_chain` is how any member takes a newer chain and sets its
  profile from it (an inference node never rises). `spawn_trust_fanout` sends
  the chain to every member after a promotion or demotion.
  `publish_chain_version` announces the local `chain_version` (counters plus
  content hash); `spawn_trust_chain_keeper` compares it every 30 s with what
  the peers announce and fetches the chain from whoever is ahead
  (`chain_sources`, `POST /api/internal/admin-trust/chain`) - any member, so
  it works with the principal offline. The chain names admins only: a
  worker's or visitor's role is its membership token (`webui/membership.rs`
  below), and a non-admin caller of a `ServingMember` route needs a `worker`
  token (`caller_may_ask`). `chain_status` feeds
  `NodeInfo.cluster_version` and `POST /api/ui/network/trust-sync`.
- `quota.rs` - token budgets before the pipeline: `admit` asks the local logs
  plugin, then each live admin (`peer.quota.check` over the signed peer call),
  and the inference handlers answer its `Denial` with 429 + `Retry-After` and
  `record_rejected`. Fail-open with a per-node memory of refusals; the plugin's
  `cache_ms` sets how long a verdict is reused. Also the node's own Budget
  routes (`/api/ui/quota`, `/reset`, `/request`). Contract:
  `docs/subsystems/http-api.md`, "Token budgets, checked before the pipeline".
- `reqlog.rs` - the audit adapter and the migration fallback. With the plugin
  active, the host writes in `enterprise.db`; independently of that destination, the
  POM's `AuditLog` replicates `begin` and `complete` to the effective admins over the p2p endpoint.
  The receiver orders the events in a bounded queue and hands them to its own active
  host; if it does not accept them, it persists them in the legacy `pom.db`. Received events are
  never retransmitted. `AuditDb` creates the schema and the FTS5 index only in the test
  adapter, opens requests in `in_progress`, completes the same row,
  marks old records as `incomplete` and serves authenticated paginated queries plus statistics on
  the server. The queries accept `node_id`, an API key's identity, a user label, a model, a prompt
  and an input range; the user is the API key's human label because the audit does not
  store a separate account. An enqueueing or write failure always records
  the operation and the request id. Requests and conversations stay in `pom.db`; the
  Enterprise audit stays in the isolated `enterprise.db`, while the legacy
  adapter remains as a fallback. The permissions of the databases and of the WAL/SHM sidecars
  are applied at startup. Empty user and model filters are ignored
  before the SQL query, preserving the broad search when the field was not filled in. The routes
  `/api/ui/requests` and `/api/ui/requests/stats` are Enterprise and fail closed with
  `enterprise_required` without an entitlement. With an active entitlement, the handlers forward
  the listing and the statistics to the `pom-plugin-host` process.
- **Every call to a peer leaves through `GatewayState.peer_http`** (`PeerClients`), which
  pins the destination's certificate by node_id. There is no longer a shared client without
  pinning in the state: with TLS on it verified no one,
  and a caller that took it out of habit failed silently. Loopback and Ollama use
  `peer_http.plain()`. A control call to another node uses
  `GatewayState::control_get`/`control_post_json`/`control_post_empty`: it signs
  once, tries each announced address - in `gateway_bases` order, the host that already answered
  first - and falls back to the p2p tunnel
  (`peer_client::send_to_gateway_over`), which is the path to a peer behind symmetric
  NAT. A signed command that changes state is sent to **one** delivered address: after a timeout on an established connection it is not repeated elsewhere, because the same envelope would be refused as a replay (409) while the peer still runs the first copy. Calls that stop a server (`/api/ui/unload`, `/api/ui/stop-inference`, `/api/ui/leave`) use `webui::PEER_STOP_TIMEOUT` (60 s), not `PEER_TIMEOUT`. `control_get_via_tunnel` is the last resort of whoever already
  traversed the addresses with the `RouteTable`'s failure memory (the read of
  `/api/ui/cluster`). `admin.rs` calls to the principal (`post_internal_json`)
  use the same `send_to_gateway_over`: direct addresses and, last, the
  tunnel. Only weight transfer stays outside
  the tunnel.
- `serve.rs` - the accept loop: it spies on the first byte, keeps the `PeerAddr` in the
  request (`request_origin`: a request from the loopback carrying the p2p tunnel's mark becomes
  `TUNNELED_ORIGIN`, non-loopback, and that mark header leaves every request - see
  `p2p-network.md`) and serves TLS or cleartext on the same TCP port, with connection upgrades
  enabled (the plugin proxy's WebSocket,
  `crate::plugin_proxy`). The HTTP/3 endpoint
  receives a separate QUIC configuration and comes up on the dedicated announced UDP port,
  even when the TCP is in plain HTTP; `TlsServer` covers only the TCP configurations, the strict
  mode and the duration of the last handshake. The TLS configuration is picked per connection
  from the ClientHello (`LazyConfigAcceptor`, `tls::wants_peer`): a peer is asked for its node
  certificate, a browser for nothing. A plain connection from outside is redirected to https, or, with
  `POM_INSECURE_ADMIN_UI=1` (`insecure_admin_ui`), served as is except the peer-only routes.
  `TransportPeer` is the caller's node as the transport
  proved it - the client certificate, or the tunnel's PeerId beside the mark - and goes into the
  request's extensions. See
  `docs/subsystems/p2p-network.md`, section 5.
- `models::OllamaClient::is_available` is only an optional probe of the
  `/health`: it returns the last value and schedules a background check of at most 500 ms
  every 15 s. It does not inherit the generation's 300 s, and a cold or absent Ollama never
  holds up the gateway's answer.
- `session.rs` - `POST /handshake` proves the connection key without sending
  it. The prober sends `{node_id, pubkey, challenge}` (32 random bytes, hex);
  the node answers `{node_id, proofs, signature}`: one HMAC-SHA256 per key it
  accepts (current, and the old one during a rotation's grace) over a fixed
  label, the challenge, its own id and the prober's, all signed with its node
  key. `answer_proves_key` is the prober's check: the signature against the
  peer's announced key, then the HMAC recomputed with its own key, in constant
  time. Refusals (403 `reason`): `blocked`, `no_key`, `identity_mismatch`,
  `invalid_challenge`. The node keeps no state per probe.

## Key functions and routes

### The network Chat

`GatewayState.network_chat` exposes the network Chat's p2p service. `webui`
serves `GET/POST /api/ui/network-chat` for the interface, the persistence and the actions
with an API key. `POST /api/ui/network/chat` writes on the real principal; when the
interface is on a secondary admin, the route forwards the authenticated mutation
to that principal. The policy route between nodes is
`GET /api/cluster/network-chat`, and the probe only treats the `authority_pubkey`
confirmed by the `AdminTrust` as the principal, not the announced profile.
The policy GETs (`/api/cluster/logs` and `/api/cluster/network-chat`) demand
`InternalNodeAuth`: a `connection_key` plus `x-pom-internal-node: 1`, from a
caller the transport proves is a member; they are not a human-token surface.
The backend does not retransmit through the HTTP UI: the action enters the `NetworkChat`'s bounded
queue and `discovery.rs` validates and forwards it over the endpoint. The snapshot does not
repeat file bytes on every polling; `GET /api/ui/network-chat/file/:message_id`
does the authenticated download of the persisted blob.

### What each file owns

The route-by-route contract (method, path, scope, body, refusal) is
`docs/subsystems/http-api.md`; the generation pipeline, the two output budgets and the KV
session rules are `docs/subsystems/inference.md` (9, 9.1); the deploy and undeploy
lifecycle is `docs/subsystems/deployment.md`; the transfer protocol is
`docs/subsystems/file-transfer.md`. What follows is only which file owns which piece, so a
change lands in the right place.

- `gateway/` - `GatewayState` (network, ollama, default_model, local_assignment, loads,
  http, runtime, gateway_port, auth, transfers, `reply_inbox`); `FromRef<GatewayState> for
  AuthState`; `peers_visible_to_cluster` filters blocked nodes; `all_peers` appends the local
  node with `self_dial_host` (advertised > local IP > loopback). Admission probing, the
  pinned-route order (`bases_by_route_plan`), the quarantine and the perf feed
  (`AdmissionHealth::note_perf` -> `fastest::PeerPerf`, read by `generation::plan_by_delivery_time`
  and `catalog::model_peer_perf`) are `docs/subsystems/inference.md`, "Pipeline admission
  probe"; a direct success is remembered with `RouteTable::mark_success`. The capability list a
  node advertises (`inference/frame-v1-batch` only when `network.batching_ready()`,
  `transport/link-v1`, `relay/v1-chain`, `relay/v2-direct`, `speculate/v1-lookup`,
  `speculate/v1-mtp-sliced` always, `speculate/v1-mtp` only when `mtp_ready`) is
  `docs/subsystems/p2p-network.md`. `prompt_budget_of` / `prompt_budget_error_of` and the
  chunked prefill are `docs/subsystems/inference.md` (9, "Adaptive chunk").
  `generate_stream_with_stops` builds a `crate::direct::DirectReply` from `reply_inbox` and
  `self_reply_urls(self_reply_hosts(), gateway_port)`; the log line `resposta=direta|anel|hub`
  says which of the three carried the answer. `GenSummary` (`source: "pipeline:hop_chain"`) is
  the terminal telemetry event: it carries `GenerationTimings` aggregated over the pipeline plus
  `legs`, which is what makes the leg stitching inspectable. `generation_finish_reason` tests
  the total budget before the EOS in each decoding branch; `audit_status_for_summary` records
  `length` as `incomplete` - a typed decision always stops at its single token, so it is `ok` -
  and reserves `partial` for a transport failure with saved text. `start_gateway_with_ready`
  binds the listener before signalling the Windows shell, registers `POST /v1/relay-result` and
  `POST /v1/remote-stream-result` (`crate::direct::RELAY_RESULT_PATH`, 1 MB body limit, no
  client API-key auth - the per-frame `Ticket` is the credential) beside `POST /v1/link`, and
  serves with `tcp_nodelay(true)` because Nagle delayed the small SSE writes. `#[cfg(test)] mod
  testkit` is the shared scaffold for route tests - a whole `GatewayState` with the peer table
  seeded by hand, loopback-only socket, nothing touches the LAN: `peer(node_id, hostname,
  gateway_port)`, `state_with(node_id, peers)`, `state_with_secondary_admin(node_id, peers)`,
  `body_of(response)`.
- `openai.rs` - `render_prompt` flattens the conversation into one user message and labels
  **every** turn including the first, so one turn's prompt is an exact prefix of the next one's
  - the property the prefix cache rests on; raw unlabelled text is what `/v1/completions` and
  `/api/generate` are for. `conversation_cache_key` is the canonical rendered prompt the head
  tokenizes, travelling as `GenerationInput::cache_key`; the Anthropic version
  `conversation_cache_key_with_budget` prefixes the task budget's identity so an entry without
  a budget or with a different marker is never reused. `usage.prompt_tokens_details.cached_tokens`
  reports the prompt tokens the pipeline did not reprocess, read from the trace - the executed
  number, not the cache's choice. `approx_tokens` is chars/4. `model_ids` comes only from live
  peer announcements (never the stale CLI default) and keeps an `idle` deployment selectable
  while its weights are unloaded; `resolve_model` accepts only a name a peer announces or an
  explicit `<model>@<deploy_id>` selector still pending gossip, refusing anything else with 404
  before generation starts. `chat_completions`/`completions` -> `blocking_response` or
  `sse_stream`; `chat_message_json` (flat content + `tool_calls` array), `apply_scan_events_openai`
  (tool_call deltas), `chunk_json` (+ the `x_pom` extension), `cors_headers` (allow `*`).
- `messages.rs` - Anthropic `POST /v1/messages` (+ OPTIONS) and `POST /v1/messages/count_tokens`,
  with its own `render_prompt` and `conversation_cache_key` for the same reason as `openai.rs`;
  `content_blocks` for tool_use/tool_result, `stop_reason` (end_turn / max_tokens / tool_use /
  stop_sequence), `usage.cache_read_input_tokens` carrying the honoured prefix reuse, and
  `blocking_response` / `stream_response` with Anthropic's SSE event names. `tool_choice` is
  parsed-and-ignored; `stop_sequences` is applied by the gateway matcher.
- `webui/` - `CATALOG_TTL=20s`, `CLUSTER_CACHE_TTL=2s`, `pub(crate) PEER_TIMEOUT=4s` (reused by
  deploy/), the job store (`Job`, `JOB_RETENTION=600s`, `jobs()`/`new_job`/`set_job`/
  `set_job_progress`) and the catalog cache (`CachedCatalog`, `warm_catalog` at startup,
  stale-while-revalidate, `metadata_complete`). The catalog publishes a quick inventory first and
  the GGUF headers later; a file the enrichment discards (a sidecar only the header reveals, like
  an MTP draft without the `mtp-` prefix) also leaves the current inventory (`drop_discarded`) -
  pending forever it restarted the enrichment in a loop, hundreds of times per second.
  `leave_network` takes an optional `LeaveRequest { node_id }`: absent, or this node's own id,
  detaches this node; a peer's id is **forwarded** to that peer's same route (`forward_leave`,
  same pattern as `dev/update` - the panel only talks to its own node, and only the owner of a
  process can detach it). The local leave stops the runtime and detaches without consulting model
  config, health or readiness, so a node can still use it after repeated load failures.
  `merge_parts`, `run_apply`/`run_slice`/`release_slice`, `apply_cluster_policy` and the reads
  `crate::reqlog::list_requests` / `stats_requests` live here too. The setup handlers for
  hardware, models, trust-anchor, health and save are deliberately NOT registered here - they
  belong to the deprecated wizard's router in `main.rs` (`start_setup_mode`), which nothing starts
  any more.
- `deploy/` - `DeployKind` (`shards` | `whole` | `tensor_parallel`) chooses which inventory the
  `survey` reads (`shard_sets`, or whole models through `parse_whole`, which reads an unsliced
  GGUF as the range `0..total-1`); `DeployKind::unit` is the only thing that changes noun between
  the three kinds - from the plan on it is the same planner, the same `launch_targets` and the
  same `launch` (`a_whole_model_is_launched_as_one_plain_target` is the test that pins that).
  `TpLaunch {size, primary_node_id, worker_node_ids}` carries the operator's choice; the node
  derives `rank0_url` and its candidates from `Machine.inference_url` /
  `Machine.inference_candidates` (`tp_identity`). Two-layer validation before the deploy:
  `tp_group_checks` inside the plan, and `tp_shardability_check` (`POST /api/ui/tp/check` ->
  `splitter::validate_tensor_parallel`) refusing MoE/SSM/weight-tied on the screen. That route is
  **authenticated** (`ConnectionAuth`), so the call carries `bearer_auth(connection_key)`:
  `peer_http.for_peer` resolves only the TLS, the credential is per request - without it the
  primary answers 401 and no TP deployment passes, on any machine. `tp_check_verdict` reads the
  answer in a separate function so it is testable and so it repeats the peer's `error` instead of
  calling every answer without `ok` an old binary's. `tp_members` transfers each machine's
  endpoint id, UDP address candidates and tunnel capability to every rank so `broadcast_to` and the
  reduce can hand the destination's dial hints to the endpoint and try each reachable HTTPS address when the
  tunnel is not available; `register_tp_reduce` receives the group's size, and the node publishes
  it and the collectives snapshot in `node_timings.tp` without mixing TP bytes or waits with the
  pipeline telemetry. `RuntimeParams.fit_memory` stays on by default in shards and whole models so
  llama.cpp can fit the layers to free memory; `effective_params_for_kind` turns it off in tensor
  parallelism, which does not support the automatic fit in `split-mode tensor` - that is the only
  structural rule it applies. `RuntimeParams.inference_mode` travels with the request in all three
  kinds independently of `POM_DEV_MODE`; an absent mode is `dense`, `auto` only enters when
  explicitly asked. The survey carries `is_moe` from the GGUF catalog (only expert metadata
  identifies MoE) and the plan exposes `inference_mode_supported` / `inference_mode_reason`.
  `overlapping_layers`, `split_position`, `resolve_group` (empty picks the only pipeline in the
  air; two pipelines is a 400 - never guess), `undeploy_endpoint` and `deployment_authority` are
  the deploy and undeploy surface; the rules they enforce are `docs/subsystems/deployment.md`.
- `transfer.rs` - `CHUNK=1MB`, `DISK_HEADROOM_MB=512`, `GGUF_MAGIC=b"GGUF"`. Path safety is
  directional: `safe_model_path` confines the *pull* direction to `models/` (`servable_path`
  canonicalizes and checks `starts_with`), while `local_send_path` lets the *operator's own* screen
  offer any readable GGUF through an opaque `grant` id (`TransferState::grants`) so a path never
  travels; `ensure_gguf` (magic + name, conjunction: `.gguf` or no extension, never an Ollama
  `sha256-` blob) gates every exit. `DEV_AUTO_ACCEPT_ENV` (`POM_DEV_AUTO_ACCEPT_TRANSFERS`, read
  once at startup by `dev_auto_accept`) is the receiver's own variable and is OFF unless the
  receiver sets it - a sender cannot ask for it. The protocol, the `.part`/`Range`/MD5 order and
  the two-sided outcome are `docs/subsystems/file-transfer.md`.
- `admin.rs` - `admin_routes()`: `GET /admin/nodes`, `POST /admin/nodes/:id/block` + `/unblock`,
  `DELETE /admin/nodes/:id`, `POST /admin/regenerate-key`, `GET+POST /admin/api-keys`,
  `DELETE /admin/api-keys/:key`, `GET /admin/transfer-offers` (the same offers as
  `transfer.rs::list_offers`), `GET /admin/slots` (local `/health` view of KV snapshots plus
  `prefix_auto_cache` index stats when Phase 3 is on) and
  `POST /admin/slots/:name?action=save|restore|erase` (the deployment-wide KV snapshot via
  `gateway::{snapshot,restore,erase}` - `docs/subsystems/inference.md` 3.1.1). `list_nodes`
  appends blocked-but-disconnected nodes so the panel is not empty, and gives each remote peer a
  `route` (`Network::peer_route`): the path this node actually uses to reach it - direct HTTP,
  a direct endpoint path (maybe hole punched) or relayed - as opposed to `addr`, which is only what it
  advertises. **The wizard is deprecated and unreachable**: nothing starts its router, so none of
  the `/api/setup/*` handlers below is served; they are kept until the wizard is removed for good.
  Setup mode exposed `/api/setup/hardware`, `/api/setup/models`,
  `/api/setup/geo/{countries,cities,nearest}`, `/api/setup/admin-anchor/preview`,
  `/api/setup/trust-anchor`, `/api/setup/health` and `/api/setup/save` on its loopback-only
  router; saving installed the trust document, persisted network details in both `pom.json` and
  `pom.db`, and respawned. `setup_pending` keeps that router active after leaving, and
  `setup_public_networks`, `setup-backups/<date>/`, `Config::first_run_guide` and `advance_guide`
  belong to the same dormant code.
- `webui/network.rs` - `GET /api/ui/network/info` delivers the `network` block the interface
  reads; `lobby_info` / `setup_lobby_info` the lobby's view; `lobby_requests` / `lobby_latency`
  what a candidate reports about itself. `fanout_network_settings`, `gateway::collect_public_networks`,
  `gateway::listed_public_networks`, `gateway::directory_public_networks`,
  `gateway::local_network_view`, `local_public_network`, `pinned_networks`,
  `remember_listed_publishers`, `pkarr_publisher`, `relay_directory`, `PomDb::custom_networks`,
  `known_networks_page` and `import_document` are the network/listing side of `webui`. The joining
  rules are `docs/subsystems/web-ui.md` and `docs/subsystems/p2p-network.md`.
- `devupdate.rs` - `ENV_GATE=POM_DEV_UPDATE`, `ENV_FEATURES=POM_DEV_FEATURES`, the `Pending` job
  state and `gate_rejection`: the development-only update/feature gate, with `dev/update` forwarded
  to the owning node the same way `leave` is. The gate contract is `docs/subsystems/build-and-run.md`.
- `auth.rs` - `ConnectionAuth` accepts the private connection key or a signed internal-node token
  (`internal_node_token`); `AuthRejection` is the refusal type. API keys: `generate_api_key_secret`
  (`sk-pom-` prefix), `api_key_hash` / `api_key_identity` / `api_key_hint` / `key_hint` (the hint is
  non-secret and survives the hash), `migrate_api_key_secrets` for records written before the
  secret existed, `IssuedApiKey` as the one-time answer, `ApiKey.id` and `connection_key_accepted`
  for the key-vs-credential distinction. `decide_admission`, `issuer_is_principal`,
  `outbound_connection_key`, `restore_primary`, `save_primary`, `set_aside_orphan_trust`,
  `admin_trust::parse_transported` and `network_from_trust_document` are the trust-chain side. The
  whole authorization model is `docs/subsystems/http-api.md`.
- `reqlog.rs` - `AuditLog::begin` records the exact prompt, request body, resolved model, entry
  node, API-key identity and arrival time before generation when the Enterprise host is
  unavailable; the active Enterprise path sends the same neutral events to `pom-plugin-host`, which
  persists them in `enterprise.db`. `spawn_logged_generation_with_stops` later calls `complete` on
  that id with result, telemetry, timings, token counts, hop chain and error. The request body
  contains no headers or secrets. `list_requests` requires `ConnectionAuth`, supports node, API-key
  identity, prompt FTS, time-range, limit and offset filters, and returns newest first with
  incomplete/error tallies; `stats_requests` uses the same auth and filters and returns token,
  node, day, model, peer, cache, terminal, inference and TTFT aggregates for the Logs statistics
  tab. The retention policy is an open item in `docs/subsystems/http-api.md`.
- `tool_call.rs` - `render_tools_block` (empty when no tools),
  `render_tool_call` (`<tool_call><function=NAME><parameter=P>...</parameter>`,
  string args raw, others re-serialized). `TagScanner` incrementally splits a
  token stream into text / tool-call spans tolerating tags split across chunk
  boundaries (`CALL_FORMATS` + `longest_prefix_overlap`); the registry is
  extended only with markers verified in the active GGUF's
  `tokenizer.chat_template`, except explicit compatibility recovery for output
  observed in audited sessions. `CallKind::Wrapped` (the real `<tool_call>`
  convention), `CallKind::FunctionCalls` (the older
  `<function_calls><invoke name=...>` repair dialect, including multiple
  invokes) and `CallKind::BareFunction` (a tolerated wrapper-less
  `<function>NAME</function>` fallback, name-only, whitespace guard). A
  `<tool_call>` opened with `<parameter=NAME>` where `<function=NAME>`
  belongs is read as a call to NAME (`repair_parameter_opening`) when the next
  tag is another `<parameter=` and, for a complete call, it closes with
  `</function>`. The quantized models also misspell the wrapper, so
  `<function_call>` and `<parameter_call>` open a `Wrapped` call, a bare
  `<invoke name=...>` is `CallKind::Invoke`, and any closer of the family
  (`CallKind::closers`) ends a span, whichever tag opened it. A
  `<function_calls>` holding a `<function=...>` body is read as one call, a
  repeated `<tool_call>` opener and a doubled or foreign trailing closer are
  normalized (`normalize_wrapped_body`), both `<parameter=P>` and
  `<parameter name="P">` are read inside one call (`parameter_open`), a
  complete call the model left unshut at the end of its output is a call
  (`TagScanner::finish`), and a closer that trails a finished call is dropped
  instead of shown (`after_call`, `STRAY_CLOSERS`).
  A call written with parameters and no name at all is attributed through
  the request's declared tools (`ToolSignatures`, passed by both adapters to
  `TagScanner::with_tools` / `parse_output_with_tools`) when exactly one tool
  takes every parameter it carries; otherwise it stays text.
  `parse_call_body` accepts the nested `<function=...>` XML form or the legacy
  Hermes JSON-object form, including an extra JSON-encoded `arguments` string;
  a sole object parameter named `arguments` is unwrapped for provider schemas;
  malformed spans and incomplete calls degrade to literal text. `parse_output`
  is the scanner over the whole text -> `Vec<OutputPart>` (`Text` |
  `ToolCall { name, arguments }`), with adjacent text merged. The same scanner
  feeds the blocking OpenAI message and its streaming tool-call deltas.
- `webapp.rs` - `API_PREFIXES = ["api/", "v1/", "admin/", "peers", "health",
  "generate"]`; `routes<S>()` serves `GET /` (index) and `GET /*path`. A miss
  under an API prefix is a JSON 404, a miss with a file extension is "asset
  não encontrado" ("asset not found"), anything else falls back to `index.html`. `resolve`
  rejects `..`, absolute and prefixed components. Cache: index.html no-cache,
  hashed assets immutable (1 year). Missing bundle -> 500 explaining the
  `npm run build` / `POM_UI_DIST` remedy.
- `session.rs` - `handle_handshake` answers a challenge with signed key proofs
  and `answer_proves_key` checks them; see the file's entry above.

## Links to other folders

- This folder calls: `crate::network` (`Network`, `PeerInfo` - discovery,
  `alive_peers`, `advertised_host`, `self_addr_candidates`), `crate::pipeline`
  (`plan_pipeline`, `LoadTracker`, `PipelinePlan` - the distributed path),
  `crate::ollama` (`OllamaClient` - model resolution and `/health` only, no
  generation fallback), `crate::runtime`
  (`NodeRuntime`, `NodeConfig` - model/slice lifecycle), `crate::config`
  (`Config`, `ApiKey`, `ui_dist_dir()`, `shard_dir_path()`,
  `model_dir_paths()`, `workspace_root_path()`),
  `crate::models`/`crate::catalog`/`crate::gguf`/`crate::plan`
  (`Catalog`, `GgufLayout`, `PeerBudget`, `pom::PomManifest`), `crate::splitter`
  (`MergePart`, `merge`), `crate::sysmon`, `crate::route` (`RouteTable` for the
  cluster view), `crate::addrs` (`outbound_local_ip`), `crate::prefill`
  (`RequestTrace` in the summaries), `crate::direct` (`ReplyInbox`,
  `DirectReply`, `Ticket`, `self_reply_urls`, `handle_relay_result`,
  `RELAY_RESULT_PATH` - the direct-reply relay that lets the tail answer the
  originator instead of bubbling through every hop),
  `REMOTE_STREAM_RESULT_PATH` - the bounded callback inbox used when a remote
  head owns the decode loop. A remote-capable head is selected by
  `generate_stream_inner` only when the gateway can receive callbacks. A
  sliced MTP plan sends the initial MTP request to that head; every hop rolls
  back its own rejected KV suffix before bubbling the existing reply, so no
  rollback request or probe frame is added per token. The local path and peers
  without the capability keep the established route. The stream degrades with
  `peer_without_sliced_mtp_wire` when a peer lacks the MTP wire capability.
  `routing_contract` capability types.
- Who calls it: `crate::main` (`start_gateway_with_ready` spawns the router, `warm_catalog`
  runs at startup);
  the frontend bundle in `apps/frontend/dist` hits `/api/ui/*`; harnesses hit
  `/v1/*` (OpenAI/Anthropic) and `POST /api/generate`; peers call each other's
  gateway HTTP for transfers, deploy forwarding, and cancel notices.

### Vision in the deployment and in the chat

The projector side (`mmproj_candidates` / `mmproj_reason` from the whole model or the chosen
`ShardSet`, `DeployRequest.vision` enabling it only on the peer whose `start_layer` is zero,
`planning::resolve_head_projector` resolving a path or a unique file name against the head and
refusing with the file named, `webui::ApplyRequest.use_mmproj` persisting the choice in
`NodeConfig`) is `docs/subsystems/http-api.md` and `docs/subsystems/deployment.md`. The Chat side
(`GET /api/ui/chat/capabilities?model=<label>` aggregating coverage, `mmproj_path` and `no_thinking`
of the deployment's head without exposing the topology to the `visitor` profile; `supported` /
`unsupported` / `unavailable`, never an invented empty value; `openai::ChatRequest.thinking_enabled`
travelling in `GenerationInput` and the prefill metadata, with the head applying the global
kill-switch before rendering the template) is `docs/subsystems/web-ui.md` and
`docs/subsystems/inference.md`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
