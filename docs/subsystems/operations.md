# Operations: bringing up and checking a cluster

Operator manual. Build and environment variables are in `build-and-run.md`;
this file covers bring-up, verification and the pitfalls measured in the
field.

## How the pieces fit

- **One process per machine.** The Rust binary starts the embedded inference
  server (`inference/infer_server.rs`) when a slice is deployed; the model is
  loaded in-process by `inference/llm.rs`. There is no Python process.
- **The model and the layers come from the deployment.** The binary does not
  accept a model, a layer range or inference parameters on the command line:
  the UI plans each peer's range and each one extracts its own slice (see
  `deployment.md`). On restart, the slice comes back through the journal
  (`lastslice`).
- **The model name must match between peers.**
  `ModelProfile::guess_from_name()` reduces the file name to a label (any name
  containing `qwen` becomes `qwen35`). The planner (`routing/pipeline.rs`) only
  chains peers with an identical `model` - shards with different labels never
  form a pipeline.
- **Ports:** UDP P2P `--port` (42777), inference QUIC UDP = P2P + 1000 (43777),
  gateway QUIC UDP = P2P + 2000 (44777), libp2p TCP/QUIC = UDP + 10000 (52777), HTTP
  gateway `--gateway-port` (8080, served by *every* node), inference HTTP on
  `inference_port` in `pom.json` (8181, or gateway + 101 when unset). Different
  machines can use the same numbers.
- **Fresh-install port selection:** when `pom.json` does not exist, POM probes
  the default gateway and P2P port sets before starting the service or native
  macOS/Windows shell. It keeps free defaults and moves only an implicit default
  that is occupied, checking the gateway/inference HTTP, P2P UDP, both QUIC UDP
  ports and the libp2p TCP/QUIC port. The selected `gateway_port` and `p2p_port`
  are written into the new configuration; an unset inference port continues to
  follow the gateway at +101. Explicit CLI or environment port choices are not
  silently changed, and an existing configuration is never rewritten by this
  first-boot selection. If no compatible free set is found, startup reports an
  error. This is a bind-time probe, not a reservation against another process
  taking a port later.
- **Context per session:** the deployment's `n_ctx` is the full window of each
  session. `max_sessions` (default 1) multiplies the total KV cache capacity
  without shrinking the individual window (`n_ctx_total = n_ctx *
  max_sessions`, in `llm.rs`); each request's prompt budget is `n_ctx` minus
  `max_new_tokens` (see `inference.md`, "Prompt limit").
- **KV cache type:** `cache_type_k` and `cache_type_v` live in each
  installation's `pom.json` and apply to every context of that installation.
  The default is `f16`; the accepted types and the confirmation in `/health`
  are in `inference.md`. They are not per-machine choices in the deploy.
- **KV cache snapshots:** the directory where each peer writes and reads back
  KV snapshots, like llama-server's `--slot-save-path`. It belongs to the
  machine, not the deploy. Two sources, in this precedence: the
  `POM_SLOT_SAVE_PATH` variable and the `slot_save_path` field of `pom.json`.
  The env var is per process and is not persisted to `pom.json`. With neither,
  the directory is `~/.pom/kv-slots` (created on the first save); an empty
  value turns the feature off and `POST /v1/session/slot` /
  `POST /admin/slots/{name}` answer "not configured". See `inference.md` §3.1.1 and `http-api.md`.
- **Persistent prefix cache:** `slot_persist_prefix_cache` in `pom.json` (or
  `POM_SLOT_PERSIST_PREFIX_CACHE=1`) makes the gateway write the retained KV at
  the end of each turn and restore it on a miss after a restart, without a
  manual call. On by default (`false` in `pom.json` or
  `POM_SLOT_PERSIST_PREFIX_CACHE=0` turns it off); requires `slot_save_path`
  on every hop, which the `~/.pom/kv-slots` default provides. A
  growing conversation keeps a single snapshot (supersession); short turns do
  not rewrite it (debounce). `slot_auto_max_bytes` caps the disk use (default
  2 GiB) and `slot_auto_ttl_secs` / `POM_SLOT_AUTO_TTL_SECS` the TTL (default 7
  days, renewed on each use). See `inference.md` §3.1.1.

## Bring-up (two-machine cluster)

```bash
# machine A (will be the administrator):
./target/release/pom
# open http://<machine-a>:8080/ - with no pom.json the node creates a local
# network of its own (admin, named after the host, discovery off) and opens the
# panel. Rename it and turn discovery on in Network settings; other machines
# then ask to join from their Networks screen and an admin approves them.
# The old wizard (profile, invitation, trust.json) only opens with --setup.

# machine B (node):
./target/release/pom
# open http://<machine-b>:8080/, pick "node" and join: the wizard finds the
# administrator through local discovery, or takes the pasted trust.json and key.
# On another network, start B with --bootstrap <machine-a>:42777.
```

The first access wizard is described in `web-ui.md` ("First access wizard").
The model never comes from the command line: once both machines are in the
network, the model is sliced and deployed from the UI (Deploy screen, see
`deployment.md`).

The HTTP gateway is bound before the model loads, so the UI and the control API
are available during startup. Loading the model takes about a minute; wait for
the inference server's `/health` before sending requests:

```bash
until curl -sf localhost:8181/health; do sleep 5; done
```

### Verification

```bash
curl -s localhost:8080/peers | python3 -m json.tool   # 2 peers, e.g. 0-15 and 16-31
nvidia-smi --query-compute-apps=pid,used_memory --format=csv   # on each GPU machine
curl -s localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $POM_API_KEY" -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"Say hi in one sentence."}],"max_tokens":32}' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["x_pom"]["source"])'
# -> pipeline:<peer-a-id>→<peer-b-id>
```

The `/v1/` routes only accept an active API key (created in the UI). A
`source` starting with `pipeline:` and with **two** hops is the only proof
that inference was joint. A single hop, or `"source": "none"`, means the
planner did not close the `0..total_layers` coverage.

## Route battery and output shape

To separate the effect of output shape from the effect of network origin, run
`POM_BENCH_API_KEY=sk-... ./scripts/bench-rotas.sh <label>`. The variable is
required, used only as the inference header and never appears in the CSV or
the summary. The script runs one warm-up call per origin, then three
repetitions of each combination of the four origins defined in the script
(local and remote origins against the target gateway) with the prompts `p-curto-repetitivo`, `p-curto-variado`,
`p-longo-repetitivo`, `p-longo-variado` and `p-prefill-aleatorio` (short/long,
repetitive/varied and random prefill). It omits `model` by default, so the
gateway uses whatever model the network is serving at that moment. Use
`POM_BENCH_MODEL=...` only for a comparison that needs an explicit model.

The CSV has one line per warm-up, measurement or failure. `phase=warmup` is
left out of the medians; `status=unavailable` in the warm-up stops further
attempts from that origin and keeps the rest of the matrix.
`fichas_por_chamada` (tokens per call) is computed on each line as
`trace.output_tokens / trace.hops[0].decode.calls`. For the origin reached over SSH,
set the password variable the script reads in the environment at run time; the
password does not belong in the script. The long prompts are filled with deterministic text and the final
table shows the medians of tokens/call, `generation_tok/s`,
`mtp_acceptance_rate`, `ttft_ms`, `node_p50_ms` and `total_ms`. The
`p-prefill-aleatorio` prompt uses 1200 new hexadecimal words per call and caps
output at 8 tokens, like the reference benchmark.

The deployment's `n_gpu_layers` decides how many layers go to VRAM; without
the right build feature (`metal`/`cuda`) the model runs entirely on CPU. To
check the offload, watch VRAM during loading.

## Pitfalls found

- **`git pull` on another user's machine.** If the files belong to `<user>`
  and you access as `root`, run
  `git config --global --add safe.directory <path>`. A private repository needs
  a read-only *deploy key* and a `git@github.com:...` remote.
- **Shards without `pom.*` metadata** (old splits) do not correct
  `total_layers` through `/health`; the value falls back to the
  `guess_from_name` guess. New shards (Rust splitter) write
  `pom.original_block_count` and are the reliable path. How many layers the
  model has is **an `Option`, not a number**, from the catalog to the plan:
  `catalog.rs` derives the total from the set's coverage when the slice does
  not declare it (`total_layers_inferred`) and `deploy::model_size` does the
  same in the cluster. Turning that "unknown" into zero was the 2026-08-10 bug:
  the plan claimed to be complete and every `apply` died in
  `NodeConfig::validate`, after `launch` had already stopped inference on the
  target machines.
- **`pkill -f "target/release/pom"` over ssh kills the ssh shell itself** -
  the pattern matches the command line of the remote `bash -c`, and ssh
  returns exit 255 without killing the peer. Use a pattern that does not match
  itself, or `pgrep -fa` first to check what will die.
- **Inference port in use:** the supervisor tries to free it
  (`runtime::free_port`, `lsof -ti :PORT` on Unix, `netstat -ano` + `taskkill`
  on Windows, probing for up to 5 s). The failure symptom is in the node log
  (`Address already in use`). Check the real routes:
  `curl -s localhost:8181/health` must answer; the server exposes
  `/v1/prefill` and `/v1/step` (there is no `/openapi.json` - it is Rust, not
  FastAPI).
- **Never just launch the binary by absolute path.** `models/` and `pom.json`
  come from the project root (`utils/config.rs`: `$POM_HOME` -> cwd if it
  looks like the project -> the executable's directory, walking up from
  `target/release`). An `ssh host '/path/pom'` runs in `$HOME`, reads another
  config and finds no slice. The `Project root: ...` line at startup says which
  one was chosen; check it before suspecting the catalog.
- **Clocks out of sync break control between nodes.** The signed control
  envelope carries its issue time and is refused outside 5 min of the
  receiver's clock (`admin_trust::CONTROL_WINDOW_SECS`), with the message
  "control envelope outside the time window". Deployment, slice surveys, idle
  model reloads and the policy ratchets depend on it. Keep NTP on the nodes; on
  a network without internet access, point them at an internal NTP server.

## Two peers on the same machine - the trap is identity

Useful for a large window without risking the RAM of the smaller machine. What
is lost is the validity of network measurements - the hop becomes loopback.
Bytes per hop still hold; network time does not.

Different ports are **not enough**. The identity lives in
`<project_root>/.pom-node.key`, so two processes in the same directory load the
same key and announce the same `node_id` - and the cluster indexes peers by
`node_id`. The second peer needs another *home*, through `POM_HOME`:

```bash
B=/path/pom-b
mkdir -p $B && cd $B
for f in models apps; do ln -s ../pom/$f $f; done
# the config is not inherited: without it the node creates a local network of
# its own and loads no model. Copy peer A's, changing gateway_port, p2p_port,
# inference_port and bootstrap_nodes; each peer's slice comes from the
# deployment in the UI.
POM_HOME=$B ../pom/target/release/pom \
    --port 42778 --bootstrap 127.0.0.1:42777 --gateway-port 8091
```

The UI is read from disk at `<POM_HOME>/apps/frontend/dist`, hence the `apps`
symlink - without it the second peer starts but answers 500 on the UI. Check
with `/api/ui/cluster`: two **distinct** `node_id`s.

To try the UI next to a live cluster without joining it, start the node with
`--no-discovery`: no announce, no LAN broadcast, no bootstrap.

## Inference over the internet - what is verified

Discovery and addresses: see `p2p-network.md`. The inference tunnel
(`relay::send_frame_or_tunnel` -> `network/infer_tunnel.rs`) was verified over
the internet on 2026-08-16: the connection closes in ~70 ms and the remote
peer's inference server answers. The live probes are
`infer_tunnel::tests::tunnel_reaches_a_live_peer` and
`relay::tests::the_tunnel_carries_a_real_frame_to_a_live_peer`, both
`#[ignore]` and driven by `POM_TUNNEL_PROBE`. They do not fire on a healthy
LAN.

Two honest caveats: the machines reached each other over global IPv6, so what
was exercised is "over the internet", not "behind a blocking NAT"; and the
detour only starts after direct HTTP gives up - **75 s measured on macOS**,
because the peer client sets 600 s per request and overrides the client's.
The deadline of direct HTTP is still an open product decision. TLS between
nodes is covered in `p2p-network.md`.

## Optional TCP tuning between peers

On Linux nodes, the operator can try these settings for transfers between
distant peers:

```bash
sudo sysctl -w net.ipv4.tcp_congestion_control=bbr
sudo sysctl -w net.ipv4.tcp_slow_start_after_idle=0
```

`tcp_slow_start_after_idle=0` keeps the kernel from dropping the send window
after a pause, even with the connection open. That can help because prefill
travels in bursts. It is an optional operational recommendation, never a
requirement: the settings need administrator privileges and are not a POM
dependency, which must work well without them.

The only measurement so far, from 1.3 to 4.1 MB/s on a 214 KB file, was in the
download direction and does not allow attributing the gain to a single factor.
No performance number is promised. POM's own file transfer over libp2p reached
about 40 MB/s between two machines; so 126 MB would be about 3 s if the
inference path used that same transport. That is a condition, not a
measurement of the inference path: the earlier estimates of 20 to 90 s came
from HTTP on a small object and do not decide the choice. Compare runs by the
`hidden_state_bytes_*` of each crossing and the effective `hidden_state_dtype`
in telemetry.

## Connecting a harness

OpenCode (verified): `opencode.json` with the `@ai-sdk/openai-compatible`
provider, `baseURL` `http://<peer>:<port>/v1`, an API key, and an agent with a
short prompt and tools turned off - the default agent's prompt does not fit
the prompt budget.

```bash
OPENCODE_CONFIG=/path/opencode.json opencode run --pure --agent short "..."
```

Measured numbers: OpenCode 1.18.21 sends ~2200 characters in the title call
and ~54k in the default agent, with `max_tokens: 32000`, tools and
`stream_options.include_usage`. The config that made the default agent work:
`n_ctx` 32768, `n_batch` 3072 and `max_new_tokens` 256 in the deployment ->
a budget of 32768 minus 256 tokens. If the prompt exceeds the budget,
`/v1/chat/completions` now answers HTTP 413 before the SSE; that makes the
error visible in OpenCode instead of leaving it waiting for an error event that
has no chunk shape.

Pi only exposes a base URL for Azure; there is no way to point it at an
arbitrary OpenAI-compatible endpoint through an environment variable.

## Load monitor (`crates/llama-engine/src/sysmon.rs`)

Linux: RAM from `/proc/meminfo`, VRAM, usage, temperature and board power from
`nvidia-smi` (checked against `nvidia-smi` by hand: an exact match). The
frequent calls to the utility run outside Tokio's process reaper, so they do
not interrupt the pkarr DHT UDP socket with `EINTR`. macOS: RAM from `sysctl` +
`vm_stat`; unified memory does **not** report dedicated VRAM - the screen says
so and shows wired memory, which is where the Metal buffers live, instead of
inventing a number.

**Radeon (Linux).** Name, VRAM, usage and temperature come from the `amdgpu`
sysfs (`/sys/class/drm/card*/device`), with no external binary - the
`nvidia-smi` equivalent would be `rocm-smi`, which only exists with the whole
ROCm installed, and the backend in use is Vulkan. Who holds VRAM comes from
`/proc/<pid>/fdinfo` (`drm-resident-vram`, or `drm-memory-vram` on older
kernels), deduplicated by `drm-client-id`. The reading is honestly **what this
user can read**: another owner's process refuses `fdinfo`, and the node -
which is what matters - is always in the list. When the same `hwmon` publishes
`power1_average` or `power1_input`, that value becomes the power in watts; if
it does not, it stays absent.

**Apple Silicon.** The accelerator publishes `PerformanceStatistics` in the
IORegistry, without root and without `powermetrics`:
`ioreg -r -d 1 -w 0 -c AGXAccelerator` gives `Device Utilization %` and
`In use system memory`, which become `utilization_pct` and `vram_used_mb`
(with `vram_total_mb` as `None`, because there is no dedicated VRAM there).
Board power also stays absent: this reading does not publish it. Temperature
stays unknown on purpose: it lives in the SMC, behind a private API, and
"unknown" is the only honest answer without privileges.

**Disks.** `disk` is the volume where the slices live - the one that answers
"does another slice fit?". `disks` is every relevant mounted partition, from
`df -k -P`, without pseudo file systems (`tmpfs`, `devfs`, `overlay`...),
without repeating mount points and without macOS infrastructure volumes
(`/System/Volumes/Preboot`, `VM`, `Update`, `Recovery` and equivalents).
`/System/Volumes/Data` and data volumes in `/Volumes` stay visible. The same
rule drops `/boot/efi`, `/boot` and `/efi`, which are infrastructure mounts on
POSIX systems too; Windows has no `df` and only reports the project volume. It
is `-k`, not `-m`: `-P` forces 512-byte blocks on macOS and ignores `-m`,
which would make every volume look 2048 times larger.

## Comparing POM with plain llama.cpp

The operational benchmark is `scripts/bench-llamacpp.sh`. It takes the
`pom-bench` `*.config.json` snapshot, selects by default the `serving`/
`is_default` deployment of the active model and only accepts `kind=whole`,
because a plain `llama-server` does not reproduce the RPC of `shards` or
`tensor_parallel`. Before starting any process on the target server it queries `nvidia-smi`
and requires, by default, 14,000 MiB free on each GPU; when POM holds the
memory, it ends with `status=blocked_vram` without touching the POM process.
The server uses the dedicated port 8090 on `127.0.0.1`, and cleanup is limited
to the PID the harness created.

When a CUDA build is needed, the harness passes `nvcc` to CMake explicitly. It
tries the default CUDA install paths on the target server first; `--cuda-compiler` or
`LLAMACPP_CUDA_COMPILER` pins an alternative path.

Minimal usage, from the repository root:

```bash
scripts/bench-llamacpp.sh <reports-dir>/<date>-<model>.config.json \
  --deploy-id <deploy-id> --output-dir ./bench-llamacpp-results
```

The result is a CSV with one line per warm-up, measurement or error, plus a
`summary.md` with the medians of the five `bench-rotas.sh` prompts. The
`generation_tok_s` field comes from `timings.predicted_per_second`;
`prefill_ms` and `prefill_tok_s` come from `timings.prompt_ms` and
`timings.prompt_per_second`; `ttft_ms` is the observed time to the first SSE
event and `total_ms` is the wall time of the local call on the target server. See
`scripts/bench-llamacpp.sh --help` for the overrides of `n_ctx`, remote paths
and VRAM policy. Without `--output-dir`, CSV, summary, metadata and the
Markdown report are written to the default reports directory;
`--report` pins the path of the individual report.

In the POM runtime, `n_ctx` is the window of each session and the sequence
budget is `2 * max_sessions`. That is why the harness passes `-c n_ctx`,
`-np 2 * max_sessions` and `--kv-unified`; the capacity is not multiplied per
session. `offload_kqv=true` stays at the llama.cpp default and only `false`
passes `-nkvo`. A pre-existing `llama-server` is only reused when
`CMakeCache.txt` confirms the fork tree with CUDA and `LLAMA_BUILD_SERVER=ON`;
required builds use an external directory so the source tree is not changed.

When the snapshot has `mtp_enabled=true`, the local reference also turns on the
MTP head of the same GGUF with `--spec-type draft-mtp`,
`--spec-draft-n-max=mtp_draft_tokens` and
`--spec-draft-p-min=mtp_min_probability`. With both MTP and lookup on in POM,
POM picks MTP first, so the harness maps the effective MTP path and records
`speculative_k` and `speculative_lookup_window` as configuration not executed
at the same time.
