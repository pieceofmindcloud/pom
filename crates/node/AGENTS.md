# crates/node - the pom node

Crate `node` (binary `pom`), the cluster's whole process: P2P, distributed
inference, HTTP gateway, web UI and monitor. It is a single-root binary
(`src/main.rs`) that declares its modules via `#[path]`, with a workspace
`lib.rs` acting as the crate root for the auxiliary modules (`routing-contract`
in `../routing-contract`, empty for the runtime).

## Cargo.toml

- Single binary: `pom` in `src/main.rs`.
- `cuda`/`metal` features passed on to `llama-cpp-sys-2` (GGML compiled with GPU).
  Used by `scripts/build-docker.sh`/`POM_BUILD_FEATURES` in the devupdate. The CUDA
  build keeps CUDA Graphs off by default; enabling it with `POM_CUDA_GRAPHS=1`
  requires rebuilding the binary and additional free VRAM.
- Heavy dependencies: `libp2p` 0.56 (tokio, tcp, dns, noise, yamux, identify,
  kad, mdns, gossipsub, relay, dcutr, autonat, request-response), `pkarr` 6,
  `axum` 0.7, `reqwest` (json+stream), `url`, `tokio` full, `clap` 4 (derive),
  `socket2`, `tower-http` (compression-full), `rusqlite` bundled for the
  persistent audit, `arc-swap` for lock-free snapshots of the log capture
  context, `h3`/`h3-quinn`/`quinn` for the optional HTTP/3 of the inference hops,
  `routing-contract` (path).
- On Windows, `windows_firewall.rs` uses COM `INetFwPolicy2` in a UAC-elevated
  helper to keep four idempotent inbound rules: UDP P2P, inference UDP QUIC,
  gateway UDP QUIC and libp2p TCP. The automatic request happens once per
  effective port and stays in `pom.db`'s `local_settings`; the UI offers a retry
  without blocking the node or `Leave network`.
- The Hugging Face provider uses native HTTP via `reqwest`; there is no `hf`
  executable, Python runtime or external install on the normal path.
- `build = "build.rs"`.

## build.rs

- `build.rs` stamps the `POM_BUILD_COMMIT` fallback into the binary:
  `git rev-parse --short HEAD` + a `-dirty` suffix if `git status --porcelain`
  shows changes → `unknown`. `build_commit()` prefers the runtime variable
  `POM_BUILD_COMMIT` when it is not empty. In Docker, the host's commit is
  exported only in the runtime stage so as not to invalidate Cargo.
- `rerun-if-changed` on `.git/HEAD` and `.git/index` updates the local fallback.
- The comment at the top warns: the UI **is not** embedded here, it is read from
  disk (`webapp.rs` + `config::ui_dist_dir`); `npm run build` in `apps/frontend`
  is a separate step.
- `build_commit()` in `main.rs` is exposed in `/api/ui/node` and used in the
  update check - it identifies a peer or an executable without a local shell.
- `main.rs` reads `POM_DEV_MODE` once at startup; only the exact value `1`
  enables development controls and the local plugin rebuild routes, and
  `NodeInfo.pom_dev_mode` publishes the decision to the interface. The flag does
  not change the licensing policy nor the `RuntimeParams` received over the
  network; the absent mode uses `dense` (`llama.cpp`) as the default.
- The native build automatically prepares the `llama.cpp` fork in a new worktree,
  cloning the `pom-tensor-parallel` branch when the versioned symlink points at a
  missing tree. `LLAMA_SOURCE` allows reusing a local checkout that contains
  `ggml`; preparation failures show the recovery command. The full procedure is in
  `docs/subsystems/build-and-run.md`, section "Preparing a local worktree".
- During the build, `llama-engine`'s `build/mtp_arch.rs` (repository
  `pom-plugin-llama.cpp`) reads the fork in read-only mode: it discovers the
  effective `LLM_GRAPH_TYPE_DECODER_MTP` checks in `src/models`, pairs each check
  with its `build_arch_graph`, crosses the class with `llama-model.cpp` and
  resolves the GGUF name in `llama-arch.cpp`. It generates
  `OUT_DIR/mtp_graph_architectures.rs`; a missing fork, an empty extraction, fewer
  than thirteen entries or an inconsistent mapping stops the build. The extractor's
  rules have unit tests in the file itself; the extraction's operational
  dependency is in `TODO.md`, item 10.
- With the `cuda` feature, `build.rs` compiles the MoE kernels with `CUDACXX`; in a
  validation on the CUDA server, `CUDAARCHS=120` pins the architecture and
  `CCACHE=ccache` wraps the `nvcc`. The procedure and the separate target are in
  `docs/subsystems/build-and-run.md`.
- On the macOS target, `build.rs` compiles `src/macos_status.m` and links only the
  system's AppKit and WebKit frameworks. Linux and Windows do not get that bridge.

## src/main.rs - startup flow and CLI

Responsibilities: CLI parsing, startup orchestration, bringing up the HTTP
gateway and the inference server (Rust, in-process) and spawning the background
tasks. **The model never comes from the command line**: it is deployed by the
interface, and on a restart it comes back through the slice's journal
(`lastslice`).

**Flags (clap derive, `Args`):** `-p/--port` (42777), `-b/--bootstrap`,
`--advertise-addr`, `--ollama-url`, `--gateway-port` (8080),
`--pkarr-peers`,
`--public-addr`, `--relay-peers`, `--serve-relay` (relay for everyone: a
public relay), `--no-cluster-relay`, `--no-relay-transfers` (the three are the
`relay` section of the config file, see `p2p-network.md`),
`--no-default-relays`,
`--no-discovery`, `--no-pkarr` (no pkarr client: a network without internet
access).
`--relay-peers` adds to the `pom.json`'s `relay_peers` (`merged_relay_peers`,
config first), as `--bootstrap` does to `bootstrap_nodes`. There is no flag for
model, memory, layer range, sampling or inference port: that comes from the
deployment (`runtime::NodeConfig`) and from the `pom.json` (`inference_port`,
default 8181; `slot_save_path`, default `~/.pom/kv-slots`).

**Every flag has an environment variable** (`ARG_ENV`, name by `brand::env_var`:
`--gateway-port` → `POM_GATEWAY_PORT`), applied by `cli_command` with clap's
builder, because the derive only accepts a literal. The flag beats the variable,
which beats the default; the booleans use `BoolishValueParser`. The variable beats
the `pom.json`: `apply_env_overrides` swaps the written `p2p_port` and
`gateway_port` when `parse_args` saw the value come from the environment
(`ValueSource::EnvVariable`). On the command line those two keep yielding to the
config - they only count while there is no `pom.json`, and that is what keeps a
port change made through the interface after a respawn, which repeats the argv.

There is no additional absolute ceiling in the `pom.json`. The output capacity is
derived from the effective `n_ctx` each runtime confirms and aggregated across the
live peers; `max_new_tokens` stays only the size of one leg. Consult `/v1/models`
after every peer confirms its runtime. The full operational step is in
`docs/subsystems/deployment.md`, section 1.3.

**Startup flow (`main`):**

Before the UI or service starts, if no `pom.json` exists, `initial_ports::select`
probes the implicit default gateway/P2P port set. It checks the HTTP gateway and
its derived inference port, P2P UDP, both QUIC UDP ports, and the libp2p TCP/QUIC
port. Occupied implicit defaults move to available ports; CLI/environment
choices stay fixed, and existing configuration is never rewritten. The new
values flow into local-network creation or setup and are persisted with the
new configuration.

1. `respawn::remember_argv` + tracing.
2. Without `pom.json`, or with `Config.setup_pending` (the preference shell
   left by leaving a network), `webui::bootstrap_local_network` creates a
   local network and the node goes on as a configured one. A failure there is
   a startup error. When there was no config at all, `first_run_guide` is set
   so the interface shows its first run hints. Without a config the ports come
   from `--port` and `--gateway-port`, which lets a second local node be
   isolated. **The setup wizard is deprecated and unreachable.**
   `start_setup_mode`, `force_setup_if_requested`, `force_setup_mode` and
   `WIZARD_FORCED` stay in `main.rs` under `#[allow(dead_code)]` and nothing
   calls them; `--setup`, `--wizard`, `--setup-bind`, `POM_FORCE_SETUP` and
   `POM_SETUP_BIND` no longer exist. Do not wire them back.
3. With a config: validates the interface port and runs the availability
   preflight; the P2P, HTTP and inference ports come from the config.
4. `start_node`, the only path of a configured node:
   - Identity `Identity::load_or_create` (`.pom-node.key`, resolved against
     `config::project_root()`): the node's only key, from which come
     `node_id`, `PeerId`, the pkarr key and the authority key.
   - Bootstrap = config + `--bootstrap`, both going through
     `config::normalize_peer_addr` in `merged_bootstrap_nodes`; relays =
     `relay_peers` + `--relay-peers`. `--advertise-addr` beats `--public-addr`,
     which beats the config.
   - `start_p2p_stack`: `Network::new` receives the initial TLS policy and the
     admission ratchet before `start` (receive, gossip 5 s, route measurement
     30 s) and `bootstrap_discovery`. A private network proves the
     `connection_key` through `/handshake`; a public network requires a
     `cluster_id` and a principal/concession of the chain in
     `.pom-admin-trust.json`. This guarantees that the first probe uses the same
     scheme the UI announced and that no unauthenticated Announce enters
     `peers`; the public network still keeps a local `connection_key` for the
     administrator's UI, but it is not shared nor used in admission;
     `--no-discovery` → `detach`.
     `Discovery::new` (libp2p, TCP = UDP+10000) + a publish task on the gossipsub
     every 5 s (`Network::gossip_announce_bytes`, the same `self_announce` as the
     UDP, with or without assignment).
   - pkarr: `PkarrPublishHandle::spawn_republisher` always publishes (with
     `r` equal to `--serve-relay`, the path of a dedicated relay,
     `examples/relay-node/`) and republishes when the swarm addresses change;
     every 30 s `spawn_pkarr_bootstrap` resolves and dials by key
     `--pkarr-peers`, the `relay_peers` relays and the trust chain principals
     that are not alive yet. `--no-pkarr` skips all of it; without it, a failed
     publication backs off from 15 s to 30 min and only warns at the 1st, 2nd,
     4th... consecutive failure. The Mainline DHT used by pkarr keeps an actor on
     its own thread; on Windows the socket's idle read timeout shows up as
     WSAETIMEDOUT (10060) every 50 ms in the dependency, and
     `application_log_filter` hides only that expected warning.
   - `ClusterLogs` starts the bounded capture and the best-effort tasks; the
     history uses `INFO` as the minimum level by default and accepts
     `cluster_logs_min_level` in the `pom.json`; `attach_plugins` hands the batches
     an admin accepts to the logs plugin, which stores and prunes them. The
     `Discovery` keeps the logs request/response behaviour separate from the
     inference tunnel and only sends batches to the principal.
   - The `NodeRuntime`'s thermal monitor, then `GatewayState` +
     `start_gateway_with_ready` spawned (every node serves HTTP/UI). The capacity
     preview route does only an analytic estimate, with four admissions per
     process and an immediate `429` when all are occupied.
   - Only with the gateway up does it inspect the slice's persistent journal
     (`lastslice::inspect()`) and restore only `Serving` or `Idle`. A `Pending`
     slice counts attempts on disk and, after `MAX_AUTOMATIC_ATTEMPTS=10`, comes
     up empty with `note_unloadable_slice`. The deployment through the UI calls
     `NodeRuntime::apply` with its `NodeConfig` (it brings up the Rust inference
     server **in-process** and supervises it); the `NodeConfig` also carries the
     configurable reserve of the automatic memory fit, and the VRAM query and the
     application live in the engine's `llm` module (`llama-engine`, re-exported
     by `main.rs`). A GPU above the limit removes the
     assignment and the `thermal` state waits for the configured cooldown. A lock,
     a fit or a model load cannot close the control port.

On macOS, `main` resolves the gateway port, moves the Tokio service to a
background thread, and gives the main thread to the status item (`macos_status.rs`
+ `macos_status.m`), because `NSApplication` must run there. The item opens
`/admin-ui/chat` in a 390 x 844 point popover and a 1120 x 880 point window;
Rust passes dimensions, URL, and `brand::name()` through the ABI instead of
hardcoding them in Objective-C. Both WebViews use WebKit's default data store
and therefore share the frontend session.
The AppKit application menu contains About and Quit. Quit uses the standard
`terminate:` action and ends the process after the event loop returns, because
the long-running Tokio service does not yet have a cooperative cancellation
channel. When the service ends on its own, the bridge only stops the event loop
and wakes it; it does not end the process, because the Rust `join()` must
propagate the message and the exit code of failures such as an invalid
`pom.json`. That makes a failed startup exit on its own without swallowing the
error. Outside macOS the entry point stays a normal CLI Tokio, with no graphical
interface dependency.

On Windows, `main.rs` marks the executable with `windows_subsystem = "windows"`
so the process does not create a console. `windows_status.rs` moves the Tokio
service to a background thread, waits for the signal sent after the gateway
bind, and only then drives the Tao window on the main thread; Wry loads
`/admin-ui/chat` in the system-installed WebView2 and `tray-icon` registers the
notification area menu. Tracing also writes `<project_root>/pom.log`, because
PowerShell's stdout/stderr redirection is not reliable for a GUI binary. Closing
the window hides it in the tray, while `Open POM` restores it and `Quit POM`
terminates the whole process.

The QUIC server keeps the bind on `[::]`, but creates the `UdpSocket` with
`socket2::Socket::set_only_v6(false)` before handing it to Quinn. That way the
same endpoint accepts IPv4 and IPv6 on Windows, Linux and macOS; Quinn's client
constructor already applies the same option.

`start_setup_mode` (dead code, see above) and `start_node` live in `main.rs` (not
in a separate module). `advertised_ollama_port` (over `parse_ollama_port`) extracts
the port from `--ollama-url` and announces it **always**.

On Windows, the parent process supervises the native child: an abnormal exit
relaunches the POM, but `Quit POM` returns exit code zero and does not relaunch.
`pom undeploy` writes a persistent block to prevent automatic resumptions of a
troubled deployment.

## src/ subfolders

- `initial_ports.rs` - first-boot selection of available gateway, inference,
  P2P, QUIC and libp2p ports when `pom.json` is absent.
- `network/` - P2P UDP + libp2p/pkarr. See `crates/node/src/network/AGENTS.md`.
- `routing/` - the pipeline planner. See `crates/node/src/routing/AGENTS.md`.
- `utils/` - config, respawn, sysmon. See `crates/node/src/utils/AGENTS.md`.
- `models/` - model profiles and the GGUF catalog: `model.rs`
  (`ModelProfile`, `LayerAssignment`), `catalog.rs` (scan of
  `model_dirs`; the fast scan publishes roots and files without computing a
  SHA-256 of whole models, leaving the `pom_id` validation to the enrichment),
  the engine's `gguf` module (header/tensor table reader, in `llama-engine`), `plan.rs`
  (slicing by budget/size), `splitter.rs` (split and merge),
  `lastslice.rs` (slice annotated with the Pending/Serving state),
  `ollama.rs` (`OllamaClient`), `provider.rs` (provider-agnostic contract) and
  `huggingface.rs` (`HuggingFaceProvider`, native HTTP).
- `inference/` - the Rust server and the wire format: `infer_server/`
  (inference routes, local scheduler + `should_draft`/`kv_can_shift`), `runtime.rs`
  (`NodeRuntime`/`NodeConfig`, in-process supervision + slice annotation),
  `wire.rs` (single initial frame `[len][JSON][items][tensor]`, f16/f8/i4
  groupwise and validated batch), `relay.rs` (chain relay + grouping by next hop),
  `prefill.rs`, `llm.rs`, `scheduler.rs`, `session.rs`, `spec_lab.rs`, `mtp.rs` +
  `llama_mtp_shim.cpp` (multi-token prediction through the model's `nextn` block -
  another mechanism, and another key, than the lookup speculation).
- `backend/` - HTTP and state: `gateway/` (`start_gateway_with_ready`, the
  `AdminAuth` boundary for `/api/ui/**` and `/admin/**`, and the port preflight),
  `generate_stream_with_stops` and the aggregated summary of TTFT, prefill,
  generation and total), `webapp.rs` (serves the UI from disk), `webui/`
  (the `/api/ui/*` routes), `auth.rs` (`ApiKeyAuth`, connection key), `admin.rs`,
  `deploy/`, `transfer.rs` (slices over HTTP Range), `devupdate.rs`
  (update/restart), `openai.rs` (OpenAI-compatible routes), `messages.rs`,
  `reqlog.rs`, `session.rs`, `tool_call.rs`.

- `macos_status.rs` and `macos_status.m` - the macOS-specific bridge for the bar
  item, the compact popover and the WebKit window; they carry no credentials and
  do not implement a parallel API.
- `windows_status.rs` - the WebView2 window and Windows tray, with the testable
  policy of hiding to the notification area on close and really quitting.

`profile.rs` is the single source of the node's persisted profile (`admin`,
`node` or `infer`) and of the visibility/promotion capabilities and deployment
management. `ProfileAction::ManageDeployment` limits adding a machine and
undeploying to the `admin` profile; removing one's own machine stays a local
action of the other profiles. The profile does not replace `Role`, which keeps
describing authentication and topology.

The inference protocol has not shipped yet. `frame/v1-batch` is the first and only
official contract: do not keep a fallback, adapter or compatibility code for
earlier v1/v2 formats. Future changes must evolve this contract explicitly.

The startup creates the `NetworkChat` next to the `Discovery`, using a libp2p
behaviour separate from the inference tunnel. The `GatewayState` shares the service
with the `/api/ui/network-chat` routes; the cluster policy is applied by the
`network_chat_policy` ratchet.

`src/plugins.rs` manages licensed plugins and independent local installs:
`PluginManager` creates a `PluginRuntime` for codes granted by a license or when a
local GitHub release is installed. Licensed runtimes persist in
`plugin_license:<code>` and keep their license/heartbeat cycle. Registered GitHub
repositories live in `plugin_github_repositories` in the `pom.db`; the POM reads a
`manifest.json` at the root or, in its absence, a `ui/manifest.json` of the
`pom-plugin-ui/v1` contract, always directly from GitHub and without consulting the
license server or checking `requires_license`. That second form uses the release
manifests `pom-plugin-<os>-<arch>.json` to discover and validate the binary. A
public install persists separately in `plugin_public_install:<code>`, without a
lease or `licensed_plugins`. The repository's manifest declares `plugin_code`, ABI,
features, preferences, optional screenshots and the asset names per platform; the
operator chooses which release to install. `GithubRepository` also carries
`description` per language and `screenshot_urls` already resolved to
`raw.githubusercontent.com/<owner>/<repo>/HEAD/<path>`, which the browser loads
directly from GitHub. Saving preferences creates the missing absolute storage
directory and sends the host the native `host.event` (`preferences.changed`, the
`pom-plugin-events/v1` envelope assembled by `plugin_event_envelope`).
`PluginManager::publish_event` keeps node events in a short queue
(`GET /api/ui/plugins/events`) and delivers them through `host.event` to every
running host: `deployment.started` comes from the deploy and redeploy handlers,
`model.serving`/`model.stopped` from `spawn_serving_watcher`. Batch license
activation validates once, then activates/installs each granted code separately.
Plugin features come from the manifest, NOT from the license. The server's public
catalog remains the commercial storefront; `GET /api/ui/plugins` publishes the local
states the node knows.
`PersistedPluginState.installed_release_id` (JSON key `plugin_release_id`, kept for
compatibility) only advances when a download is actually installed
(`activate_and_install`/`apply_update`) - the heartbeat does not write into it, it
only uses its own `release_changed`/`available_release_id` to trigger
`detect_update` and feed `status.available_update` (2026-09-21: the heartbeat wrote
there directly and corrupted the base `detect_update` compares against the manifest,
making "Update" never show the real update). `PluginStatus.update_checked_at` only
advances when the manifest query finishes successfully; `installed_at` only advances
when the host is ready after an install or an apply. `active` means a ready host,
not just a valid license; `state` preserves the license validity and
`installation_error` publishes the actionable install/handshake failure, persisted
per plugin. Those fields have `serde(default)` for existing states and are not
derived from `last_success_at`, which the heartbeat updates. `public_install`
identifies a GitHub host without a lease and `PluginStatus.licensed_plugins` stays
empty in that case. `PluginStatus.license_expires_at` is the LICENSE validity
(`GET /v1/licenses/status`, queried in `restore()` and on the manual "Update" click
through `refresh_license_status`), never the technical `lease_expires_at` -
confusing the two already leaked into the UI once, see
`docs/subsystems/enterprise-licensing.md`. GitHub repositories are an independent
local source: `plugin_github.rs` validates the root manifest or the UI manifest
`ui/manifest.json`, the ABI, the target, the SHA-256 digest and the size of the
selected asset. Installing the chosen release does not query the catalog or the
license server and creates no lease; a local `plugin_code` conflict with a licensed
installation prevents a host from being replaced silently.
`src/plugin_dev.rs` discovers direct child sources under
`/opt/pom/dev-plugins` whose names match `pom-plugin-*`; it derives each
`plugin_code` from valid, agreeing root/UI manifests and rejects unsafe codes.
It canonicalizes source and build roots before checking that neither contains
the other, including macOS paths that resolve through `/private`. It validates
the package's single `cdylib`, then runs fixed `cargo metadata`/`cargo build`
arguments. Cargo artifacts go to
`<project_root>/.pom-dev-plugin-builds/<repository>`, outside the source mount,
so read-only source binds work. The cluster development image exposes Cargo to
its non-root node user and points `CARGO_HOME` to a writable cache in the
project data volume.
AdminOnly routes work only with `POM_DEV_MODE=1`; a build may install a local
plugin without a license or prior installation,
persisting its binary and state in `plugin_dev_install:<code>` without granting
licensed features. If a licensed or GitHub installation already exists, the
local artifact replaces its binary without changing entitlement. The flow
restarts only the plugin host and does not query GitHub/licenses or restart the
node.
The manifest's optional preferences are kept locally by
`plugin_preferences:<code>`, outside the snapshot/license.
The `PluginStatus` also publishes the expiry, the granted plugins, candidates with
an eligible release and authenticated usage per `plugin_code`, preserved from
validation/activation/heartbeat. `GET /api/ui/plugins` is open to every profile;
that JSON does not include license tokens. The commercial catalog stays public and
separate. Every SUCCESSFUL heartbeat updates the license validity and calls
`ensure_host_running` (the same logic as the boot restore): if that plugin's host is
`None`, it tries to bring it up again without waiting for a full restart of the
`pom` process. Only a host that completes the handshake makes `PluginStatus.active`
true; a failure keeps the license state separate and publishes `installation_error`,
which the Plugins screen shows. `ingest_audit_event` logs a warning when it drops an
event for lack of a host.

The boot restore (`PluginRuntime::restore`) is a concurrent task that overwrites the
in-memory state with the database's and may reattach the host on its own. That is why
`from_catalog` takes `restore_on_boot`: production passes `true`, and the test
constructors pass `false`. A test that builds state by hand loses the race against it
in a non-deterministic way - that is what made
`heartbeat_success_restarts_the_stopped_plugin_host` fail sometimes for a missing
`plugin_path`, sometimes for a host revived after the revocation.

On-demand plugins (`lifecycle: "on_demand"` in the `ui.manifest`, see
`docs/subsystems/plugins-ui.md`) do not get their host started at boot, by the
heartbeat or by restore: `PluginRuntime::on_demand()` reads the flag from the
last manifest saved in `plugin_ui_manifest:<code>`, and `open_on_demand` /
`close_on_demand` start and stop only the process, without touching the license
or the installation. While the host is stopped, `ui_manifest()` returns the saved
manifest so the menu keeps listing the plugin. The plugin's own manifest decides
the flag, never a list in the code.

`plugin_host.rs` (`PluginHost`) is the `pom`'s only adapter to the auxiliary
process `pom-plugin-host` - it already supports several concurrent instances, one
per active plugin. `PluginHost::start` passes the `plugin_code` to the auxiliary
binary, which resolves `pom_<plugin_code>_plugin_v1` and uses `POM_PLUGIN_DB`.
`PluginRuntime::plugin_database_path` derives the file from the POM's database
directory: `enterprise` gets `logs.db` (in Docker, `/data/logs.db`) and other
plugins get `<plugin_code>.db`; there is no separate Enterprise volume or
variable. The host uses `kill_on_drop` and kills the process explicitly when the
`ready` handshake fails or exceeds the 30 s timeout - without that, a slow
handshake (a machine still recovering from an OOM) left `pom-plugin-host` orphaned
in the OS while the `PluginRuntime` stayed without a host, and the audit fell to
the legacy fallback with no warning. The host keeps a persistent stdout reader and
correlates answers by request ID; a timeout removes only the pending record, and a
late answer is consumed without shifting the next call's frame.
`enterprise_p2p.rs` signs fingerprint proofs by nonce (a generic use, not specific
to one plugin). The `GatewayState` exposes the `PluginManager` to the backend,
`webui` publishes list/activation/per-plugin update and `reqlog` routes events and
queries to the `enterprise` plugin specifically. Both sides of the IPC use the
shared limit `enterprise_protocol::MAX_IPC_FRAME_BYTES` of 16 MiB.

`src/plugin_ui.rs` is the interface provided by plugins: it validates the
`pom-plugin-ui/v1` manifest a plugin returns in the `query` operation's
`ui.manifest` (`PluginUiManifest::parse`, `validate_catalog` for the i18n
catalogs), and serves `GET /api/ui/plugins/ui` (only active plugins with a valid
manifest) and `GET /api/ui/plugins/:code/assets/*path` (only listed assets, via
`ui.asset`). The manifest may declare `icon_image` as the menu's PNG,
`documentation` with guides and Markdown reference under `docs/`, `description`
per language, `screenshots` (repository paths or `https://`, never served by the
node) and `menu[].children` for submenus; MIME types and declarations are checked
before serving.
Menus and routes accept `roles` and `profiles` as validated metadata preserved in
the manifest; the POM frontend does not use them to restrict plugins, which are
available in every profile. The POM's native access restrictions stay independent.
`PluginRuntime::ui_manifest` reuses `query_audit`. The full contract is in
`docs/subsystems/plugins-ui.md`.

Every plugin host receives the node's context through the `query` operation's
`host.configure`: `{"gateway": {"openai_base_url", "api_key"}, "workspace_root": "..."}`.
`workspace_root` is the absolute writable root shared by projects and plugins'
namespaced dynamic data. `plugins::spawn_host_context` (started in `main.rs` when
the gateway is assembled) assembles that JSON with
`http://127.0.0.1:<gateway_port>/v1` and the node's chat key
(`admin::node_chat_key`, the same one the interface's Chat uses, because it is the
only key class a node without authority registers on the principal) and re-evaluates
it every 10 min, or every 30 s while the key does not come out.
`PluginManager::set_host_context` re-delivers it to running hosts when the context
changes, and `PluginRuntime::start_host` hands it to every host that comes up. A
plugin that does not know the operation answers an error, and that is ignored. None
of this goes by environment variable or argv.

`src/plugin_proxy.rs` is the reverse proxy `/api/ui/plugins/:code/proxy/*` of the
web interfaces a plugin serves on loopback: it asks the host `ui.upstream`
(`{"status":"ready","port","token"}`), forwards HTTP in streaming and upgrades
(WebSocket; that is why `serve.rs` uses `serve_connection_with_upgrades`) to
`127.0.0.1:<port>` with `x-pom-plugin-token` and `x-forwarded-prefix`, and never
passes a cookie, `Authorization` or `x-pom-access-token` nor accepts a `Set-Cookie`
from the plugin. The authorization is the administrative surface's (open to every
profile), cross-site forgery is blocked by `same_origin`, and calls between nodes
are refused.

`src/plugin_services.rs` covers what a plugin offers to the cluster: it
validates the optional `host.services` answer of every running plugin into
`NodeInfo.plugin_services` (5 s cache, drawn by the dashboard next to the
machine) and serves `POST /api/internal/plugins/:code/peer/:op`, which forwards
a signed call from a live peer to the plugin's `peer.<op>` operation together
with the caller (`node_id`, `head`). A call without the signed envelope is
refused. A service of kind `inference` may also list the GGUF
`architectures` its engine runs. See `docs/subsystems/plugins-ui.md`, "Services
a plugin offers to the cluster", and `src/inference/plugin_engine.rs` for how
an inference service replaces llama.cpp on a deployment.

`src/rag.rs` is transparent retrieval for `/v1/chat/completions`: it finds
the ready `rag` service of the network (15 s cache, peers asked behind the
request), calls that plugin's `peer.search` within `POM_RAG_TIMEOUT_MS`
(300 ms) along the measured gateway route
(`GatewayState::control_post_json_routed`), and appends the passages after the
conversation. With `"rag": true` it may first run short generations of its own
on the pipeline: a typed yes-or-no decision on whether documents are needed
(reusing `decisions.rs`) and, for a long message, its search terms; both take a
KV session per hop. The retain boundary at the end of the conversation travels as a
label-less `DecisionProbe`, so the prefix cache keeps the conversation and
never the passages. Any failure leaves the request untouched. See
`docs/subsystems/plugins-ui.md`, "Transparent retrieval (RAG)".

`src/pom_core_update.rs` is deliberately SEPARATE from `src/plugins.rs`: the
update of the `pom` binary itself is not a plugin and will become public GitHub
Releases without a license (another task). Until that migration, `PomCoreUpdater`
borrows the `enterprise` plugin's lease to authenticate the generic `pom_core`
manifest and, only with an explicit confirmation from the UI, delegates decision,
hash, atomic swap and restart to `core_update.rs` - which stays pure and with no
licensing dependency. The boundary and the remaining limits are in
`docs/subsystems/enterprise-licensing.md`.

## Links

- **Depends on**: `routing-contract` and `brand` (sibling crates) and
  `llama-engine` (repository `pom-plugin-llama.cpp`, a git dependency);
  `config` is an internal module of `node`.
- **Called by**: nobody (it is the binary). The inference server and the splitter
  are Rust, in-process.

## Defects noted

None open in this folder.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.