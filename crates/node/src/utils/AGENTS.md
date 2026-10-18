# utils/ - support code with no network state

> **Crate change:** `brand.rs` became the crate `crates/brand`; `sysmon.rs`
> moved to `crates/llama-engine` (it uses the ggml FFI to list GPUs). `node`
> re-exports both in `main.rs` (`use ::brand as brand;`,
> `pub use llama_engine::sysmon`), so `crate::brand::…` and `crate::sysmon::…`
> keep working. `sysmon` receives the project root through `sysmon::set_disk_root`
> at startup, instead of calling `config::project_root()`.
> `KvCacheType` left `config.rs` for `llama_engine::kv_cache` and is
> re-exported by `crate::config`.

Configuration paths, the respawn of this process, and the system monitor.
Nothing here talks to peers; everything is used by `main.rs` and
(in the case of `config` and `sysmon`) by practically the rest of the crate.

## Files

- `config.rs` - the `Config` persisted in `pom.json` and the resolution of the
  project root, the `pom.db` directory and the UI. `POM_DB_DIR` points the
  database at a volume; without it, it stays in the project root. Operational
  state uses `backend::db::PomDb`; the legacy fields stay serializable in
  `pom.json` so an old backup is not destroyed. The required field `profile` is
  the node's official profile; an old `pom.json` without it requires a new setup,
  with no migration.
  `NetworkSettings` holds name, organization, description, rules, channels,
  visibility (`local`, `private`, `public`), access policy and geo location
  (country, city, coordinates and canonical label). `setup_pending` marks a `pom.json` that is only a
  preference shell (the node left a network): startup then creates a local
  network instead of loading it. `first_run_guide` is the progress of the
  first run guide (`backend/webui/guide.rs`): `None` when there is no guide,
  else the ids of the steps done; `prepare_for_setup` drops it.
  `ResourcePolicy` holds the RAM and CPU percentages reserved for the system.
  The first access uses `~/.pom/models` and `~/.pom/models_sliced` as defaults
  and writes `shard_max_gb` to limit the shared slices.
  `cluster_logs_min_level` also lives in the JSON, accepts
  `trace`, `debug`, `info`, `warn` and `error`, and assumes `info` when absent;
  it only controls what `CaptureLayer` queues.
- `geo.rs` - offline city database and the rule that turns a pinned point
  into a network's published location (see below).
- `crash.rs` - native crash report, installed first thing in `main`: on
  SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT it writes the signal, the faulting
  address, the thread and the native stack as `binary(+offset)` to stderr
  (async-signal-safe calls only), then restores the default action and
  re-raises, so the exit status and core dump do not change. llama.cpp runs
  in-process and a native fault otherwise leaves no line in the node log.
  Resolve a frame with `addr2line -f -C -e <binary> <offset>` against the
  same build. A kernel line `in pom[X,...]` gives the **file** offset: add the
  difference between the executable `LOAD` segment's `p_vaddr` and `p_offset`
  (`readelf -lW`) before `addr2line`.
- `instance_lock.rs` - one running node per project root: an advisory file
  lock (`.<slug>-instance.lock`) the kernel releases at exit. `acquire` waits
  up to `RESPAWN_GRACE` because a respawn starts the new process just before
  the old one exits; the holder records its interface port in the file so a
  second launch can open it (unreadable on Windows, where the lock is mandatory).
- `respawn.rs` - restart this process preserving the startup argv, creating a new
  session on Unix and redirecting stdout/stderr to `<project_root>/pom.log` so
  failures right after the restart leave a trace. `respawn_at` receives the path
  captured before an atomic rename, avoiding the `(deleted)` path that
  `current_exe()` can return on Unix.
- `sysmon.rs` - RAM/VRAM/disk sampling per platform, with a cache.
- `thermal.rs` - temperature policy, hysteresis and cooldown as a pure state
  machine, with no vendor or hardware dependency. Validation requires a cutoff of
  at least 70 °C and a cooldown of at most 60 seconds.

## Key functions

`config.rs`:
- `brand.rs` - the product identity, read from `brand.json` at the repository
  root at compile time (`include_str!`). It is the single source of the name, the
  slogan and the logo files, and **every derived name comes from here**:
  `config_filename()` (`pom.json`), `env_var("HOME")` (`POM_HOME`),
  `gguf_key("start_layer")` (`pom.start_layer`), `dns_record()` (`_pom`) and the
  identity key names. Renaming the product means editing `brand.json` - and,
  only for the executable, the `[[bin]]` of `Cargo.toml`, which cargo requires to
  be literal. The frontend reads the same file
  (`apps/frontend/scripts/build-brand.mjs`), so screen and binary cannot disagree
  about the name.

- `project_root` - `$POM_HOME` → a cwd that "looks like the project" → upward
  from the executable's directory → the default home (`~/.pom`; on Windows, the folder of `pom.exe`). This is where `models/`, `venv/` and the config
  come from; without it, an `ssh host '/path/pom'` read another config and found
  no slice at all.
- `Config::load`/`save`/`config_path` - `$POM_CONFIG` or
  `<project_root>/pom.json`.
  `Config::save` writes with owner-only permission (`0600` on Unix), even when the
  file already existed; the optional field `huggingface_token` is a secret and
  `Config`'s `Debug` masks it. The runtime reads keys, thermal policy and token
  from the `pom.db`, without using those fields as a fallback. On load, the legacy
  plaintext administrative credential is migrated once to an Argon2id hash; the
  file never persists the plain password.
- `normalize_peer_addr` - completes the missing port in a typed address (a bare
  IPv6 without a port becomes `invalid port value` in the resolver, repeated
  forever in the log); it keeps what is stored identical to what is printed.
- `ui_dist_dir` - `$POM_UI_DIST` or `<project_root>/apps/frontend/dist`
  (the UI is read from disk at runtime by `webapp.rs`).
- `model_dir_paths`/`shard_dir_path` - where to look for whole models and where
  to write/read slices. The configuration created by the wizard uses
  `~/.pom/models` and `~/.pom/models_sliced`; existing configurations keep their
  choice. An empty `model_dirs` searches nowhere. `shard_max_gb` limits only the
  slices folder and assumes 100 GB when missing from an old `pom.json`.
  `POM_MODEL_DIRS` (several folders, separated like `PATH`) and `POM_SHARD_DIR`
  override the `pom.json` fields in those two accessors (`model_dirs_from_env`,
  `shard_dir_from_env`); they are not persisted, and empty counts as absent.
  `POST /api/ui/paths` does not save a field that a variable defines, and
  `GET /api/ui/paths` returns its name in `model_dirs_env`/`shard_dir_env`.
- `workspace_root_path` - the effective absolute root of the workspace shared by
  the user's projects and the plugins' dynamic data. The persisted field is
  optional for old configurations and the default is `~/pom_workspace`, with no
  question in the wizard and separate from the model folders.
- `create_default_storage_dirs` creates POM's own default folders when they
  are the effective ones (`~/.pom/models` among the model folders,
  `~/.pom/models_sliced`, `~/pom_workspace`), at node start, after setup and
  after `POST /api/ui/paths`. Setup used to save them without creating them,
  so a fresh install could not receive a file. A folder the operator typed is
  never created at start (it may be on an unmounted drive); saving one in
  Storage does create it, from an absolute path (`check_dir`,
  `check_workspace_dir` in `backend/webui/catalog.rs`).
- The configuration has no absolute output ceiling. The effective capacity is
  observed at runtime after the model loads and reaches the gateway through
  `n_ctx`; the gateway aggregates the smallest value among the live peers. The
  only generation limit defined in the deploy is `max_new_tokens`, which bounds
  one leg. See `docs/subsystems/deployment.md`, section 1.3, and
  `docs/subsystems/inference.md`, section 9.1.
- `cache_type_k`/`cache_type_v` - the numeric K/V cache type for every llama.cpp
  context of this installation. The default is `f16`; the accepted values are
  `f32`, `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `iq4_nl`, `q5_0` and `q5_1`.
  These fields live in the `pom.json` and are not part of a deployment's
  per-machine parameters.
- `cluster_logs_min_level` - the minimum threshold of the deployment log capture;
  the console keeps its own `RUST_LOG` policy independent. Retention is fixed on
  the principal and is not a preference of the request audit.
- `DEFAULT_INFERENCE_PORT` (8181) - announced from startup even before a slice is
  assigned (without it the `apply` refused every model).

`respawn.rs`:
- `remember_argv`/`original_argv` - captures the argv (without argv[0]) at the
  start of main; that is what preserves `--advertise-addr` etc. across a restart
  (a naive respawn with `Command::new(current_exe())` lost the arguments).
- `respawn_self` - spawns `current_exe` + the original argv in an independent
  session, with stdout/stderr in `pom.log`, and exits (exit 0); if opening the
  log or the spawn fails it returns the error and the node stays alive to report.
- `respawn_at` - the same policy as `respawn_self`, but using the executable
  supplied by the authorized update after the rename.

`sysmon.rs`:
- `sample` - a read with a 1.2 s cache (`nvidia-smi` costs a spawn per read; the
  UI polled faster than the numbers changed). The frequent `nvidia-smi` queries
  run in `spawn_blocking` with `std::process::Command`, without using Tokio's
  SIGCHLD reaper, so they do not interrupt the DHT pkarr UDP socket.
- `read_memory` - Linux `/proc/meminfo` (MemAvailable, not MemFree); macOS
  `sysctl hw.memsize` + `vm_stat` (active + wired + compressor = the Activity
  Monitor's "Memory Used", and it returns wired for the Metal buffers); Windows
  `GlobalMemoryStatusEx` declared by hand to avoid dragging in `windows-sys`.
- `read_gpu`/`nvidia_smi` - total/used VRAM and processes per NVIDIA card;
  `current_process_gpu_memory_mb` does the synchronous probe of the current PID's
  usage to correlate the process with the fit snapshot, and returns `None` when
  the driver does not report the line.
  Apple Silicon reports `GpuKind::Unified` with the honest note (no dedicated
  VRAM to measure) instead of inventing a number. That variant is only built on
  macOS, and outside it `dead_code` complained - hence the `cfg_attr(not(macos),
  allow)` on it. Removing it by `cfg` would be worse: `gpu_kind` is a serialized
  field, and `deploy::free_memory_mb` reads the one of **other** machines to
  decide whether the slice fits.
  The name follows a common policy: the matching backend's description,
  `pci.ids` on Linux, and the card identifier as a last resort. The wrapper
  `llama_rs_backend_devices_json` exposes the ggml description and `device_id`;
  the PCI `device_id` is what pairs the Vulkan description with the right Radeon
  on machines with several cards. Apple Silicon reports `GpuKind::Unified` with
  the honest note (no dedicated VRAM to measure) instead of inventing a number,
  using the Metal description and then `system_profiler` for the name. That
  variant is only built on macOS, and outside it `dead_code` complained - hence
  the `cfg_attr(not(macos), allow)` on it. Removing it by `cfg` would be worse:
  `gpu_kind` is a serialized field, and `deploy::free_memory_mb` reads the one of
  **other** machines to decide whether the slice fits.
- `sample_disk` - `df -m` on the project root (not on the cwd); Windows
  `GetDiskFreeSpaceExW`. `sample_disk_at` receives the configured destination and
  measures the volume containing that path, even when the folder does not exist
  yet. The screen uses this variant to check whether a slice's output fits on the
  real disk the operator pointed at.

`sysmon.rs` also publishes `GpuStat.temperature_c`: NVIDIA comes from the same
`nvidia-smi` line that enumerates VRAM and utilization, Radeon comes from
`amdgpu`/hwmon, and a unified GPU stays explicitly without a reading. `None`
means unknown temperature and is never turned into an alarm by the thermal
protection.

`config.rs` keeps `validate_network_settings` as the single validation of the
network block. The wizard and the later editing both use it, and it requires a
public name and a city (`city_id`) when the visibility is public; local and
private networks may leave the location empty.

`geo.rs` is the offline database of countries, states and cities: the
unmodified `assets/geo/csv-cities.csv.gz` (Countries States Cities Database,
ODbL 1.0, pinned by `RELEASE` and a checksum test) embedded in the binary and
decoded on first use. `GeoDatabase::nearest` answers the cities around a
coordinate, `resolve` the single closest one with a small memo (it names the
place of every listed network on each poll of the Networks screen), `search` the cities of a country by name (accents folded), `city`
one row by id. `apply_location` is the only writer of a network's location:
`NetworkSettings.city_id` must be a row of the database, the country, state
and city names are copied from that row (never the operator's text), and
`location` is reduced to `location_precision` before it is stored - `exact`
keeps the pin, `approximate` (the default) snaps it to a 0.1 degree grid with
`accuracy_m` 10000, `city` stores the city's reference point. A pin more than
200 km from the chosen city is refused. The result is stable under
reapplication, which is what lets every member apply the settings the
principal sends. See `assets/geo/README.md` for the update procedure.

`NetworkSettings.accepted_enrollments` (`AcceptedEnrollments`, a list of
`EnrollmentRole`: `worker`, `visitor`) is what the network advertises as open to
new members. A missing field deserializes to every role, an empty list means
closed. It is an advertisement for the public directory only: admission
(`Network::public_peer_is_admitted`) does not read it and current members are
never affected.

The lobby, where machines that are not members ask to be admitted
(`network/network_lobby.rs`), has no setting: it exists whenever discovery is
on (`public` or `private`). It does not need the network Chat: the Chat only
adds the conversation to the lobby. `NetworkSettings.access_policy` says who
decides a request: `authorized` (an admin), `auto_approve` (the principal
approves every request on arrival) or `local_only`.
`Config.quic_enabled` and `Config.network_chat_enabled` are on by default:
`main.rs::blank_config` starts a new network with both, and a config
without the fields reads them as on. Members still adopt the principal's
value through the authenticated read.
`NetworkSettings.visibility` is what the interface calls discovery: `local`
announces nothing (the node runs detached and publishes no pkarr record),
`private` announces without being listed, `public` is listed.
`NetworkSettings::open_admission` is the one case where membership needs no
connection key: a public network left on `anyone_with_anchor`.
`NetworkSettings.public_stats` publishes the served models, node count and
total VRAM to non-members, answered live by the principal; off by default and
public networks only.

## Links

- **`config`** is used by nearly everything: `main.rs`, `network/identity.rs`
  (`key_path`), `models/catalog.rs` (the folders scanned), `backend/transfer.rs`
  (`safe_model_path`), `backend/webapp.rs` (`ui_dist_dir`), `backend/deploy/`,
  `backend/webui/`.
- **`display`** only in `main.rs` (banner + refresh loop).
- **`respawn`** is called by `backend/devupdate.rs` (update and restart through
  the interface) and by `main.rs` at startup.
- **`sysmon`** in `main.rs` (hostname) and `backend/webui/` (`/api/ui/*` endpoints).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.