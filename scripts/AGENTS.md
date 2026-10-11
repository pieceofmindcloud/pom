# AGENTS.md - scripts/

Operational scripts (build and cluster config generation) and the real home of
the Python code. Authoritative: read the files; this is a map.

## scripts/

- `build-env.sh` - opt-in helper, loaded with `source`, that detects
  `sccache` and exports the shared CAS cache, the 80G limit, the Rust/CMake/CUDA
  wrappers, `CCACHE`, the dedicated socket and a common `CARGO_TARGET_DIR`.
  Without `sccache` on the `PATH`, it does not change the build environment.
  `test-build-env.sh` checks both paths and the preservation of overrides.
- `bench-routes.sh` - runs the reproducible battery measuring the distributed
  generation: four origins, five prompts, warm-up per origin, a CSV with the
  timings/trace/decode/prefill telemetries and a medians table. The CSV
  appends at the end the counters
  `trace_mtp_draft_tokens_{proposed,accepted,rejected}` and
  `trace_mtp_acceptance_rate`, preserving the order of the earlier columns; the
  fifth prompt uses 1200 new hexadecimal words and `max_tokens=8`. It sends the
  inference API key in `POM_BENCH_API_KEY` for every call to
  `/api/generate`; the variable is neither printed nor written to the CSV. By
  default it omits `model` in `/api/generate`, so the gateway uses the model
  currently served; `POM_BENCH_MODEL` is an optional override. The remote origin
  reached over SSH uses the script's password variable at run time, never a
  password written in the repository. See the script's header for the columns,
  URLs and the `--origins`, `--repetitions` and `--csv` options.
- `build-docker.sh` - builds the cluster image: it reads the commit **on the
  host** (`git rev-parse --short HEAD`, with a `-dirty` suffix when the tree has
  changes) and exports `POM_BUILD_COMMIT` only for the runtime. By default it
  resolves the tag of the remote `pom-tensor-parallel` branch (the same algorithm
  as the release action, completing the history of a shallow clone without
  downloading blobs) and uses `POM_LLAMA_CPP_MODE=release`; the
  `LLAMA_CPP_CACHE_KEY` then includes that tag. `POM_LLAMA_CPP_RELEASE_TAG` pins
  an existing release.
  `POM_LLAMA_CPP_MODE=source` compiles the local fork;
  `POM_LLAMA_CPP_SOURCE_DIR` may point at another Git clone, whose content is
  prepared in a temporary named context without `.git`, the root's `models/`
  data, build directories, keys or dotenv files; the `src/models/` sources and
  `build-*` files in other folders are preserved. The cache is keyed by commit
  and delta. The main context ignores the local copy of `llama.cpp`, because the
  fork only enters through the named context. The script derives
  `POM_CUDA_ARCH_CACHE_KEY` from the real architecture list and calls
  `docker compose build`. The per-cluster composes live in the
  `pom-cluster-config` repository, which calls this script with `COMPOSE_FILE`.
  All cluster machines use the same `runtime` target, with a universal
  CUDA+Vulkan `pom` and the CPU/Vulkan/CUDA modules in the same runtime.
  The frontend is an independent stage and its copy of `dist` is the image's last
  layer; fork/tag, toolkit, architecture and toolchain do not invalidate the UI,
  and editing the UI does not redo Cargo. All services use
  `/usr/local/bin/pom`.
  Build through these scripts; do not run `docker compose build` directly.
- `test-docker-build-cache.sh` - proves the image reuses what did not change. The
  structural part checks the independent Rust/frontend stages, the final frontend
  copy, the `.dockerignore`, the exclusion of the commit stamp from the build
  stages, the native Cargo cache serialized across the services, that there is
  only one POM build combining CPU, CUDA and Vulkan, and that kache is installed
  after rustup and before the sources, with `kache init` before the
  `cargo build` and `XDG_CACHE_HOME=/build/target`. The kache `cargo install`
  counts in the `toolchain` group. Then, if a daemon exists, it derives a stub
  from the Dockerfile and uses BuildKit over a synthetic context to verify cache
  in source, commit, fork, CUDA, toolchain and local artifact scenarios.
  `POM_CACHE_TEST_DEBUG=1` prints each step, group and cache state.
- `test-docker-build-mode.sh` - tests the release/source modes with a fake
  `docker`: source prepares a named context without `.git`, the root's `models/`
  data, build directories, GGUFs and credentials, preserving `src/models/` and
  `build-*` sources in subfolders; release keeps only a marker and propagates the
  pinned tag and cache key.
- `test-docker-entrypoint.sh` - smoke test of the Docker entrypoint with fake
  binaries: it confirms the host path, the `exec` of `pom` and the preservation
  of the arguments.
- `test-docker-compose.py` - materializes the Compose files of the cluster machines. It
  validates that all nodes use the same image/build/binary, plus volumes and
  NVIDIA/Radeon visibility; the CUDA server's entry has a `node-1` service with
  GPUs 0 and 1 and coherent architecture/cache arguments.
- `gen-node-configs.py` - generates one `pom.json` per node in
  `tests/fixtures/node-<N>/`. Each node's ports come from the config, not from
  the CLI (with `network_mode: host`, a port collision and the typical error).
  Node 1 = principal (keeps `connection_key` and `api_keys`); the rest = node
  with `bootstrap_nodes` pointing at `127.0.0.1:42777`. It refuses to generate
  without a `connection_key`; `--force` overwrites an existing config. It never
  generates the connection key - it inherits it from the base config (`--base`,
  default `pom.json` at the root) so an existing cluster is not broken.
  `--advertise-addr` updates only the announced address, also in existing
  configs, preserving the other fields. When it receives `HOST:PORT`, it treats
  the port as the set's base and uses each node's own `p2p_port`; that way four
  processes on the same host do not all announce socket 42777.
- `test_gen_node_configs.py` - unit tests of the `advertise_addr`
  override, including the preservation of the config's other fields.
- `bench-llamacpp.sh` - CLI of the pure llama.cpp benchmark on the CUDA server. It takes the
  pom-bench `.config.json` snapshot, selects a `whole` deployment, checks free
  VRAM, brings up the `llama-server` of the POM's remote fork on a local port
  beside the remote host, measures the battery's five prompts, and tears down
  only the PID it created. The Python implementation lives in
  `bench_llamacpp.py`; the pure tests of selection, payload, SSE and medians
  live in `test_bench_llamacpp.py`. The geometry equivalent to POM uses
  `-c n_ctx`, `-np (max_sessions * 2)` and `--kv-unified`, preserving the per
  session window without multiplying `n_ctx`; `offload_kqv=true` uses llama.cpp's
  default and only `false` adds `-nkvo`. A preexisting binary
  is only accepted when the `CMakeCache.txt` proves the same source with CUDA and
  `LLAMA_BUILD_SERVER=ON`; otherwise the build goes outside the tree.
  When `mtp_enabled=true`, the configuration becomes `--spec-type draft-mtp`,
  `--spec-draft-n-max` and `--spec-draft-p-min`; the fork uses the GGUF's own
  `nextn` head, with no separate draft model.
  Artifacts go to the standard reports folder by default, and the CLI, the flag
  map and the limitations are in the task's operational report. If `nvcc` is not
  on the remote `PATH`, the harness auto-detects `/usr/local/cuda/bin/nvcc`,
  `/usr/local/cuda-13.0/bin/nvcc` or `/usr/local/cuda-12.9/bin/nvcc`;
  `--cuda-compiler` and `LLAMACPP_CUDA_COMPILER` allow pinning another path.
- `migrate_pom_db.py` - a disposable, explicit tool to copy, once, deployments,
  audit, API keys, thermal preference and token from the old sources into a
  `pom.db` already created by the binary. Startup never calls the script; it
  validates conflicts, prints no credentials, and does not delete or alter the
  sources.
- `run-docker.sh` - `docker compose down` + `docker compose up -d`. It does not
  recreate the image; use `build-docker.sh` for that.
- `build-enterprise-plugin.sh` - compiles a workspace `cdylib` crate (default:
  `enterprise-plugin-fixture`, the only one left here) for an explicit platform,
  locates the right extension, and copies the artifact with SHA-256 and size to
  the output directory. The production cdylib (`enterprise-plugin`, the
  Logs/Statistics audit domain) was extracted to the `pom-plugin-logs`
  repository, which has its own build script, packaging, release workflows and
  publication on the license server - see
  `docs/subsystems/enterprise-licensing.md`.
- `resolve-release-version.py` - reads the `GET /internal/releases` answer and
  picks the largest SemVer across all platforms, incrementing the `build.N`
  metadata for the `pom-release.sh`.
- `release-ksia.sh` - release build on the ksia runner for `linux-cpu`,
  `linux-cuda` (CUDA 12.9, wide architecture list) or `windows-cpu`
  (cross-compiled with `cargo xwin` + LLVM 20's `clang-cl`/`lld-link`; the MSVC
  STL requires Clang >= 19), with state in `~/.cache/pom-release` and package in
  `dist-release-ksia/`; `--dry-run` only prints the commands; `--debug-symbols`
  (windows-cpu) includes `pom.pdb`. Every variant compiles
  `-p node -p pom-plugin-host` and puts the host beside the `pom` in the package.
  Called by `.github/workflows/release-ksia.yml`.
- `test-release-ksia.sh` - dry-run of each variant (including the
  `pom-plugin-host` copy) and a check that the Linux runner's workflow uses no
  GitHub storage.
- `test_pom_release.sh` - local smoke test of the automatic increment, the manual
  override and `--dry-run`, using fake `curl` and `cargo`. Covers only
  `pom_core` (the `pom`/`pom.exe` binary) on the three platforms.

`pom-release.sh` lives at the root because it operates on the POM's remote
checkout: it looks for `origin/feat/enterprise`, creates a throwaway worktree and
sends a multipart to `/internal/releases/upload`. Without `--version`, it queries
`/internal/releases?plugin=pom_core&status=all` with the `POM_RELEASE_TOKEN`,
groups all platforms by SemVer version and increments `+build.N`; `--version` is
a manual override and skips that query. It releases only `pom_core` (it compiles
the real binary of the `node` package with `cargo build --release -p node
--target ...`): the defaults are `cuda,vulkan` on Linux (the same set as the
`Dockerfile`), `metal` on macOS (release workflow) and no feature on Windows CPU.
`--features` replaces the build's features and `--feature-set` changes only the
upload metadata (default `core`). The artifacts are `pom` on Linux/macOS and
`pom.exe` on Windows; the operator's `cargo` wrapper must route the Windows MSVC
target to `cargo xwin` when the host is not Windows. Before Cargo, the script
removes `CMAKE_CUDA_ARCHITECTURES` and forces `CUDAARCHS=''`: CMake treats an
empty value as not set, and the vendored `CMakeLists.txt` picks the wide CUDA
architecture list, never the build machine's GPU. The server requires a positive
`plugin_abi` for `pom_core` too, so the script sends `1` as transport
compatibility metadata, not as a cdylib ABI. The `feature_set=core` is metadata
only and creates no entitlement. `--dry-run` needs no token and makes no HTTP call,
and shows `+build.1` as a preview when the version is not given.

The `enterprise` plugin (Request Logs/Statistics) is no longer released by this
script: its release (build, packaging, GitHub Actions and publication on the
license server) lives entirely in `pom-plugin-logs`.

## Legacy Python

There is no longer a Python inference server or splitter in the repository: the
runtime is Rust (`crates/node/src/inference/infer_server.rs`,
`crates/node/src/models/splitter.rs`). The old `old/` folder was removed.

## Build and dev infrastructure at the root

- `Dockerfile` - shared stages of dependencies, Rust, frontend and runtime. All
  cluster machines compile one `pom` executable with CPU, CUDA and Vulkan, plus
  `pom-plugin-host`; the builds use cache mounts and the frontend is independent.
  There are two final images: `runtime`, the distribution one, without a
  compiler; and `dev-runtime`, the development one, with Rust/Cargo and native
  tools. The toolchain sits in the `dev-os` stage, over the operating system
  (`runtime-os`) and before any POM file, and `dev-runtime` copies the finished
  artifacts from `runtime`: a POM build reuses the toolchain layers instead of
  reinstalling it. The host commit becomes `ENV POM_BUILD_COMMIT` in the runtime,
  without invalidating the Cargo/npm layers. The `docker-entrypoint.sh` does an
  `exec` of the `pom` and exports `POM_PLUGIN_HOST`; on activation, the `pom`
  starts the host as a child with the downloaded plugin and IPC over
  stdin/stdout, with no socket or IPC volume.
- `docker-entrypoint.sh` - validates that `pom` and `pom-plugin-host` are
  executable, pins the host path, and preserves the container's arguments. It
  does not start an empty host at boot because the host needs the Enterprise
  plugin's path, which only exists after activation.
- `docker-compose.yml` - four-service local cluster (`node-gpu`, `node-gpu2`,
  `node-gpu-3`, `node-cpu`) using `network_mode: host`, shared build/image
  anchors and persisted state under `tests/fixtures/<name>`. The default local
  target is `dev-runtime`, which includes Cargo and native tools for plugin
  builds; `POM_DOCKER_TARGET=runtime` selects the image without a toolchain.
  Each node mounts `POM_DEV_PLUGINS_DIR` (default `..`, the workspace directory
  containing sibling plugin repositories) at `/opt/pom/dev-plugins` and sets
  `POM_DEV_MODE=1`; the API still restricts the repository allowlist. The default
  setup maps a mixed-GPU topology; the CUDA server is started from the
  `pom-cluster-config` repository. `docker-compose.salem.yml` can also be
  selected directly. Both extend `docker-compose.common.yml`, keeping the same
  services, context, image, volumes and variables. Every cluster node uses the
  same executable, and the backend is detected from visible devices. The UI is
  found at `POM_UI_DIST=/app/apps/frontend/dist` because `POM_HOME=/data` is
  not the image's `/app` directory.
- `.github/workflows/release.yml` - manual-only release (`workflow_dispatch` with
  `version` and `prerelease`). Job `ui` builds the interface once; a matrix of 4
  targets (macOS arm64 Metal, Linux x86_64 CPU, Linux x86_64 CUDA 12.6, Windows
  x86_64 CPU experimental) with `continue-on-error` and `fail-fast: false`; the
  `release` job (`if: always()`) publishes with whatever compiled. Package =
  binary + `apps/frontend/dist` + README, so `config::project_root` finds the UI
  from the executable's directory. Windows packages with 7z.
- `.github/workflows/release-ksia.yml` - manual release compiled only on the
  self-hosted Linux runner (label `ksia`) (no GitHub quota): jobs `linux-cpu`,
  `linux-cuda`, `windows-cpu` chained by input, each calling
  `scripts/release-ksia.sh` and attaching the package to the tag. See
  `build-and-run.md`, "Release on the self-hosted Linux runner".
- `.cargo/config.toml` - `BINDGEN_EXTRA_CLANG_ARGS` points the llama-cpp-sys
  bindgen at GCC's headers on Ubuntu (apt's clang does not bring them); the
  release profile with `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`,
  `incremental = true` (the sampler's hot loop: ~30x measured); the mold linker
  enters on Linux via `[target.'cfg(target_os = "linux")']`.
- `.devcontainer/` - Ubuntu 24.04 dev container with Rust, node, docker CLI +
  buildx and sshd (`root:123`, local sandbox, not the internet).
  `devcontainer.json` mounts the docker.sock and the host's ssh key;
  `install-buildkit.sh` downloads the `linux-amd64` buildx binary (arm64 needs
  `docker-buildx-plugin`).
- `.dockerignore` - cuts `target/`, `target-*/`, `.fm-secondmate/`, `.git/`,
  `venv/`, `models/`, `*.gguf`, `*.key`, `pom.json` and `docs/superpowers/` from
  the context. It keeps `llama.cpp/` because the link in
  `vendor/llama-cpp-sys-2/llama.cpp` needs the source code in the build.

## About the names and the state of the docker-compose fixtures

`docker-compose.yml` and `docker-compose.salem.yml` extend
`docker-compose.common.yml` to use the same four services on the two machines,
while keeping the state directories each node already uses.
`gen-node-configs.py` keeps writing into `tests/fixtures/node-<N>`; Compose
mounts the folders named by hardware role. Do not swap the volume paths as part
of aligning the services: that would move where the persistence lives and require
a separate migration. `tests/` stays outside the image's context, but its
directories are mounted by the host at runtime.

## migrate-to-pom.sh

Migrates a machine from the project's old name to POM. The binary no longer
knows the old name - it looks for `pom.json` and `.pom-node.key` (the node's
only key) and reads the `POM_*` variables - so this script carries the
compatibility: it renames the config (including variants such as `-cpu.json`),
the `*.last-slice.json` annotation and the identity key. The old separate pkarr
key (`.rdma-p2p.key`) is no longer used and stays where it is. It does not touch
models or slices. Once it has run on every machine, it can be deleted.

A slice cut before the rename loses its `original_block_count` (the metadata is
now called `pom.original_block_count`) and goes back to "unknown layer total", a
case the code already handles; re-slicing restores it.

`--quiet` only speaks when it renamed something and silences the closing
reminder: that is the mode the operator's deploy scripts use, because they run the
migration on every bring-up and on an already migrated machine there is nothing to
say.

It also covers the **per-role variants** (`rdma-p2p-cpu.json`,
`.rdma-p2p-node-cpu.key`): the project's `.gitignore` always used the patterns
`rdma-p2p*.json` and `.rdma-p2p-node*.key`, and one cluster machine had both
variants on disk - migrating only the exact names would leave an orphan key.

## make-macos-app.sh

Assembles `POM.app` around the already compiled binary (default:
`target/release/pom` -> `dist-release/POM.app`) and copies its sibling
`pom-plugin-host` into `Contents/MacOS/`, refusing to assemble the bundle without
it. It is the only way an executable gets an icon on macOS: the icon belongs to
the bundle, not to the file. Name, slug and `.icns` path come from
`brand.json`; the Info.plist marks `LSUIElement` (the node opens no window - the
interface is web, on the gateway port) and the bundle stays callable from the
command line at `POM.app/Contents/MacOS/pom`. The release workflow calls this
script on the macOS target and copies the interface's `dist` to
`Contents/Resources/apps/frontend/`, which is where
`config::bundled_ui_dist()` looks for it. The script also copies
`assets/brand/macos/pom-status-template{,@2x,@3x}.png` and the
`assets/brand/macos/*.lproj` localizations to `Contents/Resources`, and refuses to
assemble the bundle if the status item's art is missing - an item without art
falls back to a text label, silently. The same bundle opens `/admin-ui/chat` in
the 390 x 844 point panel and offers the larger window through the system WebKit.

## derive-macos-status-icon.py

Derives the `NSStatusItem` art from `pom-icon-src.png`. The menu bar uses
**template images**, which only have an alpha channel: the POM mark is an opaque
blue square and, used as is, it would become a black rectangle. The script
converts the white stroke into alpha, discards the wordmark (illegible at 18
points), and writes 1x, 2x and 3x into `assets/brand/macos/`. Run it after
changing the mark; a test in `macos_status.rs` fails if the art disappears.

## derive-dark-logo.py

Derives `assets/brand/pom-logo-dark.png` from the light art. It is not new art:
it mirrors the lettering's luminance (a flat petrol blue, drawn for a light
background, which vanished on the dark panel) preserving hue and saturation, and
**does not touch the brain illustration** - recoloring it flattened the volume,
and it already has its own contrast in the dark. The preserved region is the
golden glow rectangle, with a soft edge so no seam is left. When a real designed
dark art exists, overwrite the PNG and delete this script.

## generate-brand-assets.py

Regenerates the square derivatives of `assets/brand/pom-icon-src.png`: web
sizes, `pom-mark.png`, Linux icons, `favicon.ico`, `pom.ico` and `pom.icns`.
`pom-logo-light.png` and `pom-logo-dark.png` are source lockups and are not
resized by this script. Run `python3 scripts/generate-brand-assets.py` after
updating the manual art to keep every platform aligned.

## Defects noted

- `test-docker-build-cache.sh`, scenario "Rust toolchain changed": it also fails
  with no change in the Dockerfile. The `RUN` that downloads the fork's release
  comes after the rustup, rebuilds when the toolchain changes, and falls in the
  `base` group, which the scenario does not expect. And because the test toolchain
  is pinned (`1.90.0`), from the second run on the same builder the layer is
  already cached and the `toolchain` group disappears from the result.