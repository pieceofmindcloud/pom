# AGENTS.md - scripts/

Operational scripts (build and cluster config generation) and the real home of
the Python code. Authoritative: read the files; this is a map.

## scripts/

- `build-env.sh` - opt-in helper, loaded with `source`, that detects
  `sccache` and exports the shared CAS cache, the 80G limit, the Rust/CMake/CUDA
  wrappers, `CCACHE`, the dedicated socket and a common `CARGO_TARGET_DIR`.
  Without `sccache` on the `PATH`, it does not change the build environment.
  `test-build-env.sh` checks both paths and the preservation of overrides.
- `build-docker.sh` - builds the cluster image: it reads the commit **on the
  host** (`git rev-parse --short HEAD`, with a `-dirty` suffix when the tree has
  changes) and exports `POM_BUILD_COMMIT` only for the runtime. By default it
  resolves the tag of the remote `pom` branch (the same algorithm
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
- `release-ksia.sh` - release build on the ksia runner for `linux-cpu`,
  `linux-cuda` (CUDA 12.9, wide architecture list) or `windows-cpu`
  (cross-compiled with `cargo xwin` + LLVM 20's `clang-cl`/`lld-link`; the MSVC
  STL requires Clang >= 19), with state in `~/.cache/pom-release` and package in
  `dist-release-ksia/`; `--dry-run` only prints the commands; `--debug-symbols`
  (windows-cpu) includes `pom.pdb`. Every variant compiles
  `-p node -p pom-plugin-host` and puts the host beside the `pom` in the package.
  The Linux variants build with `embed-ui,desktop` and are packed as AppImages
  (`pom-<v>-x86_64-unknown-linux-gnu-{cpu|cuda}.AppImage`) by
  `make-linux-appimage.sh`, with appimagetool kept in the cache's `tools/`;
  Windows stays a zip. Called by `.github/workflows/release-ksia.yml`.
- `make-linux-appimage.sh` - packs `pom` + `pom-plugin-host` into an AppImage
  (AppRun, desktop entry, icons, and `LICENSE` + `THIRD-PARTY-NOTICES.txt` in
  `usr/share/doc/pom/`). The optional fifth argument names the file;
  the default `pom-<v>-x86_64` is the CUDA + Vulkan package of
  `release-linux.yml`.
- `test-release-ksia.sh` - dry-run of each variant (including the
  `pom-plugin-host` copy) and a check that the Linux runner's workflow uses no
  GitHub storage.
## Legacy Python

There is no longer a Python inference server or splitter in the repository: the
runtime is Rust (`crates/node/src/inference/infer_server/`,
`crates/node/src/models/splitter.rs`). The old `old/` folder was removed.

## Build and dev infrastructure at the root

- `Dockerfile` - shared stages of dependencies, Rust, frontend and runtime. All
  cluster machines compile one `pom` executable with CPU, CUDA and Vulkan, plus
  `pom-plugin-host`; the builds use cache mounts and the frontend is independent.
  There are two final images: `runtime`, the distribution one, without a
  compiler; and `dev-runtime`, the development one, with Rust/Cargo, native
  tools and Google Chrome (amd64, first layer of `dev-os`). The toolchain sits in the `dev-os` stage, over the operating system
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
  `pom-cluster-config` repository. It extends `docker-compose.common.yml`,
  which holds the services' context, image, volumes and variables. Every cluster node uses the
  same executable, and the backend is detected from visible devices. The UI is
  found at `POM_UI_DIST=/app/apps/frontend/dist` because `POM_HOME=/data` is
  not the image's `/app` directory.
- `.github/workflows/release-{linux,macos,macos-self-hosted,windows}.yml` and
  `docker-hub.yml` - manual-only releases (`workflow_dispatch`); the table of
  what each one builds, where and with which inputs is in
  `docs/subsystems/build-and-run.md`, and `.github/AGENTS.md` is their map.
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
  `devcontainer.json` mounts the docker.sock and the host's ssh key; the
  `Dockerfile` downloads the `linux-amd64` buildx binary (arm64 needs
  `docker-buildx-plugin`).
- `.dockerignore` - cuts `target/`, `target-*/`, `.fm-secondmate/`, `.git/`,
  `venv/`, `models/`, `*.gguf`, `*.key`, `pom.json` and `docs/superpowers/` from
  the context. It keeps `llama.cpp/` because the link in
  `vendor/llama-cpp-sys-2/llama.cpp` needs the source code in the build.

## About the names and the state of the docker-compose fixtures

`docker-compose.yml` extends `docker-compose.common.yml` for its four services,
while keeping the state directories each node already uses.
`gen-node-configs.py` keeps writing into `tests/fixtures/node-<N>`; Compose
mounts the folders named by hardware role. Do not swap the volume paths as part
of aligning the services: that would move where the persistence lives and require
a separate migration. `tests/` stays outside the image's context, but its
directories are mounted by the host at runtime.

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
Only `pom.icns` is not square: `macos_icon` puts the art on an opaque rounded
824 tile, centered on a 1024 canvas with fully transparent margins.
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