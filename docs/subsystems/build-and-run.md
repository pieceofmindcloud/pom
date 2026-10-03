# Build and run

How the project compiles, packages and runs. The code is authoritative; this
document reflects the current state of the repository. The root `README.md`
is presentation, not a source of truth for architecture.

## Crate workspace

The root `Cargo.toml` defines a workspace with five members and
`default-members = ["crates/node"]`, so `cargo build`/`cargo test` at the root
build and test the node. For every public crate plus the node, use
`cargo test --workspace`. `resolver = "2"`; version and edition (0.1.0, 2021)
come from `[workspace.package]`; the shared dependencies are `anyhow`, `serde`
and `serde_json`.

- `node` - the only binary (`pom`), the whole product. It only depends on
  `routing-contract` (path dep). It is a single-root binary (`src/main.rs`)
  that declares its modules through `#[path]`; the `cuda`/`metal` features are
  passed on to `llama-cpp-sys-2`.
- `routing-contract` - serializable, versioned routing contract.
- `routing-base` - open deterministic planner (called by nobody today; see
  Defects).
- `config`, `types` - extraction points, empty scaffolding today (lib.rs with
  no content, nothing links them).

Note: the `crate::config` references inside `node` are its own module
`crates/node/src/utils/config.rs`, not the `config` crate.

## Cargo build

```bash
cargo build --release    # binary at target/release/pom
cargo build              # debug
cargo test               # node tests
```

- The release profile in `.cargo/config.toml` uses `opt-level = 3`,
  `lto = "fat"` and `codegen-units = 1` (measured ~30x in the sampler's hot
  loop), `debug = 0`. On Linux the linker is clang with `-fuse-ld=mold` to
  speed up the fat LTO link; **the `mold` package is a build dependency on
  Linux** (`apt-get install mold`). `BINDGEN_EXTRA_CLANG_ARGS` points
  llama-cpp-sys's bindgen at the GCC headers on Ubuntu (apt's clang does not
  bring them); on macOS it is harmless.
- **Features**: `cuda`/`metal` (empty default) passed on to `llama-cpp-sys-2`,
  which builds the native GGML with CUDA or Metal. E.g.
  `cargo build --release --features cuda`. They are used by the `Dockerfile`
  and by devupdate (`POM_BUILD_FEATURES`). In CUDA builds, CUDA Graphs are off
  by default to avoid the extra VRAM reservation that can fail in
  `cudaGraphLaunch` even after warm-up. To enable it, only when free VRAM is
  proven, use `POM_CUDA_GRAPHS=1 cargo build --release --features cuda`.
- sysmon queries the device descriptions in ggml's own backend registry after
  it is initialized. The common bridge `llama_rs_backend_devices_json`, in
  `vendor/llama-cpp-sys-2`, also includes the PCI `device_id` so the Vulkan
  description is matched with the right card. Linux still uses
  `/usr/share/misc/pci.ids` or `/usr/share/hwdata/pci.ids` as a fallback; macOS
  uses the Metal description and then `system_profiler`.
- The Hugging Face provider is native HTTP in Rust (`reqwest`): the POM build
  does not install `hf`, Python or any other external runtime.
- `crates/llama-engine/build.rs` reads, read-only, the same checkout prepared by
  `vendor/llama-cpp-sys-2/build.rs` to generate
  `OUT_DIR/mtp_graph_architectures.rs`. The first path is the Cargo metadata
  `DEP_LLAMA_LLAMA_SOURCE`, published after the dependency picks the source;
  there is a fallback to `LLAMA_SOURCE` and then to
  `vendor/llama-cpp-sys-2/llama.cpp`. On Windows that is the real directory
  installed by the earlier preparation, not the ignored `llama.cpp` folder at
  the root; Cargo runs the dependency's build script before the build script of
  the crate that generates the list. The extractor walks the effective
  `LLM_GRAPH_TYPE_DECODER_MTP` checks in `src/models`, crosses each class with
  the dispatch in `src/llama-model.cpp` and uses the `LLM_ARCH_NAMES` table in
  `src/llama-arch.cpp` to get the string compared with the GGUF's
  `general.architecture`. The check must be in the unambiguous scope of
  `build_arch_graph`; comments do not count. If the fork is missing, no check is
  found, the result has fewer than thirteen entries, or it is inconsistent, the
  build fails loudly.

### Cache shared between worktrees

For local builds, load the helper before calling Cargo:

```bash
brew install sccache             # once on macOS
source scripts/build-env.sh
cargo build --release --features metal
```

The helper only turns on the cache when it finds `sccache` in `PATH`. Without
the tool, it leaves the environment unchanged and the build goes the normal
way. When the cache is available, these are the defaults:

| Variable | Default | Effect |
|---|---|---|
| `SCCACHE_DIR` | `$HOME/.cache/sccache` | local content-addressed cache shared by the worktrees |
| `SCCACHE_CACHE_SIZE` | `80G` | compile cache ceiling |
| `SCCACHE_SERVER_UDS` | `$SCCACHE_DIR/server.sock` | socket of the server dedicated to the shared cache |
| `CARGO_TARGET_DIR` | `$HOME/.cache/pom-target` | Cargo output shared by the worktrees |
| `RUSTC_WRAPPER` | `sccache` | caches Rust compilations |
| `CMAKE_C_COMPILER_LAUNCHER` | `sccache` | caches C in llama.cpp's CMake |
| `CMAKE_CXX_COMPILER_LAUNCHER` | `sccache` | caches C++ in llama.cpp's CMake |
| `CMAKE_CUDA_COMPILER_LAUNCHER` | `sccache` | keeps the CUDA path consistent on the CUDA build server |
| `CCACHE` | `sccache` | wraps the `nvcc` called directly by `llama-engine` |

The values can be overridden before the `source`, for example
`CARGO_TARGET_DIR=/tmp/pom-target source scripts/build-env.sh`. The shared
target is a Cargo output, not sccache's CAS: for incompatible toolchains,
platforms or feature combinations, use a separate explicit target. sccache's
CAS is safe for concurrent compilations and avoids recompiling the same objects
in different worktrees. `vendor/llama-cpp-sys-2/build.rs` passes the three
`CMAKE_*_COMPILER_LAUNCHER` variables on to CMake and watches them as Cargo
rerun inputs.

To inspect and clear the cache, with no builds running:

```bash
source scripts/build-env.sh
sccache --show-stats
sccache --zero-stats
sccache --stop-server
rm -rf "$HOME/.cache/sccache"
```

`.cargo/config.toml` does not define the wrappers: that would make a wrapper
mandatory on machines without sccache and would not allow explicitly selecting
the shared server/socket.

### Cache in GitHub Actions

The three release workflows (`.github/workflows/release-{linux,macos,windows}.yml`)
resolve the current SHA of the `pom-tensor-parallel` branch before the build.
The fork checkout lives in `.ci-cache/llama.cpp`, outside the versioned symlink,
and is stored by `actions/cache` with the key:

```text
llama-source-v1-<system>-<architecture>-<fork-sha>
```

After the restore, the preparation step confirms with `git rev-parse` that the
checkout has exactly that SHA. On a cache miss, it does the shallow clone and
refuses to continue if the branch moved between resolution and clone. The build
gets `LLAMA_SOURCE` to use that directory; so `actions/checkout`'s `git clean`
does not turn the vendored clone into an implicit dependency on the runner's
previous state.

The `llama-cpp-sys-2` wrappers use header names without a directory prefix.
`build.rs` explicitly adds to bindgen and to the C++ bridge the `include/`,
`ggml/include/`, `src/`, `common/`, `tools/mtmd/` and `vendor/` directories of
the selected tree. So the cache is not restored over
`vendor/llama-cpp-sys-2/llama.cpp`, avoiding a conflict with the placeholder
the Windows checkout materializes. The same path works with the local vendored
checkout, with `LLAMA_SOURCE` and inside the Dockerfile.

On Windows, `canonicalize()` can return the extended `\\?\` prefix. bindgen
accepts that path, but CMake/MSBuild and some MSVC versions do not. Before
handing the source, directories or files to `cmake::Config` or `cc::Build`,
`build.rs` strips the prefix; the paths reach the compilers as conventional
`D:\...` or UNC, even when the source came from `.ci-cache`.

The workflows also enable `mozilla-actions/sccache-action` with the GitHub
cache backend. `RUSTC_WRAPPER`, the three CMake launchers and `CCACHE` point
to `sccache`, so the same content-addressed cache covers the Rust units,
llama.cpp's C/C++ and the CUDA bridge when there is CUDA. Windows does **not**
use `sccache` (a wrapped `nvcc` on Windows is not worth the risk); there, where
the runner is ephemeral, only `Swatinem/rust-cache` covers `~/.cargo` and
`target/`; its extra key includes the fork SHA and `hashFiles('**/Cargo.lock')`.
Linux and macOS use persistent self-hosted runners and keep the local target,
while `sccache` covers recompilations across revisions and worktrees.

That combination invalidates the checkout when the fork changes, invalidates
the Cargo cache when the lockfile changes and lets `sccache` itself separate
compilers, flags and source content. The first build of a key still pays for
clone, configure and compilation; on the following ones, the expectation is to
skip the minutes of unchanged C/C++ and Rust units. The final link, especially
release LTO, still runs, so the gain is not the total build time. The strategy
was validated offline by inspecting the workflows,
`vendor/llama-cpp-sys-2/build.rs` and the keys; no workflow run was triggered.

### Development Docker with a release or a local fork

The cluster's Docker builds use `POM_LLAMA_CPP_MODE=release` by default.
`build-docker.sh` queries the remote `pom-tensor-parallel` branch, derives the
same tag as `get-tag-name` in the workflow and passes the exact tag to Docker.
It also accepts `POM_LLAMA_CPP_RELEASE_TAG` to pin an already chosen release.
The fork's workflow must finish and publish the assets before this build: a
tag without the CPU x64, CUDA 13.4 x64, Vulkan x64 packages and the source
archive fails with no fallback. The manual dispatch must use the
`pom-tensor-parallel` branch; a push to that branch does not trigger the
automatic rule, which is restricted to `master`.

The tag's source archive provides the headers and sources used by bindgen, MTP
and POM's bridges; in the builder it sits at `/build/llama.cpp`, the target of
the vendored symlink, and an explicit `LLAMA_SOURCE` takes precedence over that
symlink. That keeps the release's empty named context from being mistaken for
an incomplete checkout. `POM_LLAMA_CPP_PREBUILT_LIB_DIR` makes
`vendor/llama-cpp-sys-2/build.rs` validate and copy `libllama.so`,
`libggml.so`, `libggml-base.so` and `libllama-common.so`, and skip only
llama.cpp's CMake. The C++ wrapper, mtmd, bindgen and POM's own CUDA MoE
kernels stay in the local build. Every cluster machine uses the same `pom` executable,
built with CUDA and Vulkan; the GGML CPU, Vulkan and CUDA modules sit together
next to the binary and the runtime picks the backend from the visible devices.
The `CUDA_VERSION-runtime` image provides cudart/cuBLAS and the Vulkan/RADV
loader.

To build against a local checkout, even outside the POM root:

```bash
POM_LLAMA_CPP_MODE=source \
POM_LLAMA_CPP_SOURCE_DIR=/storage/nvme1/projects/llama.cpp \
  ./scripts/build-docker.sh
```

The script prepares a temporary named context without `.git`, model data in
`models/` at the root, credentials or build directories, but keeps uncommitted
local sources, including `src/models/` and `build-*` files inside `cmake/` and
`common/`. The target cache is isolated by that checkout's commit and delta.
In `source`, the Dockerfile keeps the normal CMake build and picks the CUDA
`base` image. In `release`, it uses the cache keyed by the tag and the
`runtime` image. POM's Cargo/GitHub Actions builds still compile the fork from
source; this prebuilt mode is only for development Docker.
`scripts/test-docker-build-mode.sh` tests the selection without real Docker.

### Measurement on macOS

Measurement taken on 2026-09-09, Apple Silicon, release profile and
`--features metal`, with a local `llama.cpp` checkout. The baseline was a clean
worktree without wrappers, with `GGML_CCACHE=OFF`; the numbers after the helper
use the shared target and the cache in `~/.cache/sccache`.

| Scenario | Time | Cargo output | Cache |
|---|---:|---|---|
| Before, clean build | 6m39s | `target/`: 1.4G in the worktree | no sccache requests |
| After, cold cache | 6m33s | `$HOME/.cache/pom-target`: 1.4G; local `target/` unused | 732 misses, 224 MiB |
| After, warm cache after `cargo clean` | 6m39s | `$HOME/.cache/pom-target`: 1.4G; local `target/` unused | 728 cumulative hits, 42.47% of requests |

The fat LTO link is still the largest share of the time and is not cached by
sccache; the measured gain is in the Rust/C/C++ compilations. In a second
worktree on the same commit, the shared target also lets Cargo find the binary
already built, without creating another 1.4G tree. The cold cache recorded four
errors on sccache's C/C++ path, but the compiler recovered and the final binary
passed; llama.cpp's existing warnings still show up.

To validate Vulkan on this macOS, the host needed `vulkan-headers`,
`vulkan-loader`, `shaderc`, `molten-vk`, `spirv-headers` and `glslang`. Since
Homebrew does not expose a monolithic SDK, CMake got the paths explicitly and
the SPIR-V headers were added to the include path:

```bash
brew install vulkan-headers vulkan-loader shaderc molten-vk spirv-headers glslang
source scripts/build-env.sh
export CMAKE_Vulkan_INCLUDE_DIR=/opt/homebrew/opt/vulkan-headers/include
export CMAKE_Vulkan_LIBRARY=/opt/homebrew/opt/vulkan-loader/lib/libvulkan.dylib
export CMAKE_PREFIX_PATH=/opt/homebrew/opt/vulkan-loader:/opt/homebrew/opt/vulkan-headers:/opt/homebrew/opt/shaderc:/opt/homebrew/opt/molten-vk:/opt/homebrew/opt/spirv-headers:/opt/homebrew/opt/glslang
export CPATH=/opt/homebrew/opt/spirv-headers/include
cargo build --features vulkan
```

That validation passed on the dev profile in 13.35s. Without those
dependencies, the attempt fails at configure with `Vulkan_LIBRARY`,
`Vulkan_INCLUDE_DIR` or `glslc` missing; that is a host precondition, not a
change to this wiring.

### Isolated CUDA validation on the CUDA build server

CUDA validation must not write to the served checkout nor to the directory the
deployment script uses. The validation checkout is `/data/ftk-cuda-src`, with
`llama.cpp` pointing to `/data/ftk-cuda-llama`, and the separate target is
`/data/ftk-cuda-target`. On that server, `nvcc` lives in `/usr/local/cuda-12/bin`, but
does not enter the `PATH` of a non-interactive SSH session. The reproducible
invocation is:

```bash
ssh <cuda-server> 'bash -lc "cd /data/ftk-cuda-src && \
  export PATH=/usr/local/cuda-12/bin:\$PATH && \
  export CUDAARCHS=120 && \
  export CUDACXX=/usr/local/cuda-12/bin/nvcc && \
  export NCCL_ROOT=/usr && \
  export GGML_CUDA_NCCL=ON && \
  export CCACHE=ccache && \
  CARGO_TARGET_DIR=/data/ftk-cuda-target \
  cargo build --release --features vulkan,cuda"'
```

The `PATH` is needed because `build.rs` runs the `nvcc` program directly;
`CUDACXX` must also be set for the sub-builds that follow the CMake
convention. `CUDAARCHS=120` limits the MoE kernel to that server's GPU.
`NCCL_ROOT=/usr` points `FindNCCL` at the host install, and
`GGML_CUDA_NCCL=ON` is needed to opt into the collective path. `build.rs` also
reads the resolved library from `CMakeCache.txt` and emits the dynamic link for
the Rust executable; without it, llama.cpp's static archive leaves `nccl*`
symbols unresolved at the final link. `CCACHE=ccache` makes `build.rs` run
`ccache nvcc`; the Rust target stays in `/data/ftk-cuda-target`.
The served binary's own target directory is reserved for it and is never used to
validate a revision.

## Product identity (`brand.json`)

`brand.json`, at the root, is the single source of the name, the slogan and the
logo files. Renaming the product means editing that file - and, only for the
executable, the `[[bin]]` in `crates/node/Cargo.toml`, which cargo requires as
a literal.

- **Binary**: embeds the file at compile time (`utils/brand.rs`,
  `include_str!`), so the name travels inside the executable and there is no
  file to ship next to it. Everything that carried the old name now derives
  from the `slug`: `brand::config_filename()` (`pom.json`),
  `brand::env_var("HOME")` (`POM_HOME`), the identity key (`.pom-node.key`,
  the node's only key), `brand::gguf_key("start_layer")` (`pom.start_layer`) and
  `brand::dns_record()` (`_pom`, the pkarr TXT). A test breaks if any of them
  stops deriving.
- **UI**: `apps/frontend/scripts/build-brand.mjs` runs before dev/build/test
  (npm pre-hooks), copies the manifest to `src/brand/` and the images to
  `public/brand/` - both folders are generated and ignored by git.
  `src/lib/brand.ts` exposes `brand` and `storageKey()`, which gives the prefix
  of the `localStorage` keys (`pom.theme`, `pom.locale`, ...). `index.html`
  cannot import anything, so the `brandHtml` plugin in `vite.config.ts`
  replaces `%BRAND_NAME%`, `%BRAND_SLUG%` and the icons at build time - that is
  what keeps the browser tab from announcing an old name.
- **Images**: `assets/brand/` holds the full logo (`pom-logo.png`, the lockup
  with lettering and slogan), the icon art (`pom-icon-src.png`, the source), the
  square 1024 mark (`pom-mark.png`), the PNGs from 16 to 512 and `favicon.ico`.
  The copies served by the UI carry a **content hash in the name**
  (`pom-icon-512.f95e6ea3.png`), because `webapp.rs` serves everything that is
  not `index.html` as `immutable` for a year - with a fixed name, a new logo
  would not reach anyone who already opened the screen.
- **The lockup has two artworks, one per theme**: `logo.light` and `logo.dark`
  in `brand.json`, so it needs no plate behind it - each theme gets the art drawn
  for its background. The dark one is **derived** from the light one by
  `scripts/derive-dark-logo.py`: the lettering (flat petrol blue, made for a
  light background) gets its luminance mirrored, and the brain illustration is
  left alone, because recoloring it flattened the volume. When a real dark
  artwork exists, overwriting `assets/brand/pom-logo-dark.png` is the whole
  change - and the script can be deleted.
- **Language selector flags**: `assets/brand/flags/{us,br}.svg`, mapped by
  locale in `brand.json` (`flags`). They are SVG because the target is a 24x16
  icon in the sidebar and a small stretched PNG looks dirty; swapping the art
  means replacing the file and running the UI build.

### Application icon, per platform

A Unix executable carries no icon; each system solves that its own way, and all
three come from the same `brand.json` (`app_icons`):

| System | File | How it gets in |
|---|---|---|
| Windows | `assets/brand/pom.ico` | a resource **inside the `.exe`**, written by `crates/node/build.rs` (`embed_windows_icon`, via `winresource`). The `cfg(windows)` there is the host's: the release builds the Windows target on a Windows runner. Cross-building from Linux comes out without an icon, silently, instead of breaking the build. The binary uses `windows_subsystem = "windows"`; `crates/node/src/windows_status.rs` opens `/admin-ui/chat` in WebView2 and keeps the window reachable from the notification area. |
| macOS | `assets/brand/pom.icns` + `assets/brand/macos/pom-status-template{,@2x,@3x}.png` | the bundle shows the icon, not the binary: `scripts/make-macos-app.sh` builds `POM.app` around the already compiled executable (Info.plist, `LSUIElement`, Finder icon in `Resources/` and the `NSStatusItem` template image, generated by `scripts/derive-macos-status-icon.py`). The binary turns on the AppKit/WebKit bridge only on that target - `cc` is a build-dependency only for a macOS host; the release runs the script and ships the `.app` in the macOS package. |
| Linux | `assets/brand/linux/hicolor/**` + `pom.desktop` | `hicolor` tree from 16 to 512 and the `.desktop` entry, packaged in `share/` in the tarball. Installing means copying to `~/.local/share/{icons,applications}`. |

**The `.app` must find the UI.** Inside the bundle the binary sits in
`Contents/MacOS/`, and from there `project_root()` would fall back to the cwd of
whoever opened it (Finder opens at `/`), with the screen answering 500.
`config::bundled_ui_dist()` resolves `Contents/Resources/apps/frontend/dist`
before trying the project root; the release script copies `dist` there.

To rename the product again: edit `brand.json`, change the `[[bin]]`, produce
the new images in `assets/brand/` and run `scripts/migrar-para-pom.sh` (or the
equivalent) on each machine, because config and keys already written to disk
still carry the previous name.

## Commit stamp (crates/node/build.rs)

The commit shown in `/api/ui/node` (`build_commit`) identifies a peer without
a shell. `build.rs` compiles a fallback: `git rev-parse --short HEAD` + the
suffix `-sujo` ("dirty") if `git status --porcelain` shows changes, or
`desconhecido` ("unknown") if git cannot be read. At runtime, `build_commit()`
prefers `POM_BUILD_COMMIT` when it is not empty. Docker injects that variable in
the final stage from the commit read on the host; it does not enter the build
environment and invalidates neither Cargo nor npm. `rerun-if-changed` on
`.git/HEAD` and `.git/index` refreshes the fallback of local builds.

## Licensed executable update

The production path is in `crates/node/src/core_update.rs` (pure
validation/install) plus `crates/node/src/pom_core_update.rs` (stateful
orchestration, deliberately separate from `crates/node/src/plugins.rs`) and
does not use `devupdate.rs`: while the `enterprise` plugin is active (a
transitional mechanism that borrows its lease - see
`docs/subsystems/enterprise-licensing.md`, Phase 7), the node queries the
`pom_core` manifest on the license server and publishes the result at
`GET /api/ui/pom-update/status`. Detection does not download, swap or restart.
The UI authorizes a specific release through `POST /api/ui/pom-update/apply`;
only then does the client download the signed artifact, check size/SHA-256,
swap the executable and schedule the restart keeping the argv. The
`release_id` published for this plugin must be the same short commit
`build_commit()` returns (compiled locally or injected into the Docker
runtime); the version comparison is SemVer and does not allow downgrades.

On Linux and macOS the temporary file sits in the same directory and the
`rename` replaces the running executable through Unix unlink-on-exec
semantics. On Windows the running executable cannot be overwritten directly:
after authorization the client leaves a staged file and starts a PowerShell
helper that waits for the process to end, moves the file and relaunches `pom`
with the same arguments. If the helper does not start, the old process stays
intact; the limitation and the full contract are in
`docs/subsystems/enterprise-licensing.md`.

## Updating a node from its own UI (development only)

`backend/devupdate.rs`. Each peer's card gets "Update and restart" when **that
process** started with `POM_DEV_UPDATE=1` - it is an environment variable, per
process, because the config is rewritten by the UI itself and a field there
would leave the gate switchable over the network. `POM_BUILD_FEATURES=cuda` (or
`metal`) says which features the machine builds with; without it a GPU machine
loses offload on the first click.

- **Acting on another peer is forwarding**: the panel only talks to its own node
  (relative paths in `apps/frontend/src/lib/api.ts`), so
  `POST /api/ui/dev/update` with `{"node_id": "..."}` makes the local node
  forward. The gate is checked **on the target**, never on the forwarder.
- **No destructive command**: the sequence refuses a branch other than `main`,
  a detached HEAD and a dirty tree, and only then runs `git pull --ff-only` and
  `cargo build --release`. There is no `reset`, `checkout`, `clean` or `stash`,
  so on a machine where someone is coding the button fails and explains. If any
  step fails, nobody restarts.
- **Each node restarts itself** (`/api/ui/dev/restart`): none can respawn its
  sibling, because it knows neither its `POM_CONFIG` nor its `POM_NODE_KEY`. A
  sibling is a peer with the same hostname **and** the same network host
  (`same_machine`) - hostname alone would match two VMs called `ubuntu`; a peer
  with `addr` `0.0.0.0`/`::` is discarded, because that unrewritten address
  points to the node itself.
- `devupdate` does **not** write the last-slice record: that lifecycle belongs
  to `runtime.rs` (see `deployment.md`), and a write here could only lie.

## Docker

- **`Dockerfile`**: independent stages for dependencies, Rust, frontend and
  runtime. The build base `nvidia/cuda:13.4.1-devel-ubuntu24.04` installs the
  native tools; the Rust stage builds a single `pom` with
  `--features cuda,vulkan` and `pom-plugin-host`, while the frontend stage
  installs npm and runs `npm run build`. CPU stays available through the GGML
  CPU module; CUDA and Vulkan are modules of the same executable, discovered
  according to the hardware. **nvcc 13.2.0 and 13.2.1 generate wrong code for
  the IQ1_S/IQ2_S/IQ3_S kernels on sm_120**: every GGUF with those tensors (for
  example GSQ-RCO and UD-Q3) produces nonsense text on Blackwell, and K-quant
  models are not affected. That is why the base moved off 13.2.1; 13.2.2, which
  already fixes it, has no published `nvidia/cuda` image. The fork also works
  around the defect since commit `8dd48c3dd` of `pom-tensor-parallel` (a port of
  ggml-org/llama.cpp#28784, which upstream closed without merging), which
  protects any toolchain. In `release` mode the `libggml-cuda` module comes from
  the fork's `cuda-13.4` package, built by its CI (`release.yml`, CUDA 13.4.1)
  and not by this image's base; a fork tag without that package fails at
  download. To check, run `test-backend-ops test -o MUL_MAT -b CUDA0` and see
  that no `iq*_s` fails. Every cluster machine's release build uses the same
  `runtime` target, so they neither repeat CPU-only and Vulkan-only builds nor
  select distinct binaries per node. This repository's local Compose uses
  `dev-runtime` to allow plugin rebuilds without changing the release artifact.
  In `release`, the core and the modules come from a published fork tag; in
  `source`, CMake builds the checkout chosen by `POM_LLAMA_CPP_SOURCE_DIR`. The
  runtime uses the CUDA `runtime` image for cuBLAS in release mode (`base` in
  source), keeps the Vulkan loader and RADV, sets `POM_HOME=/app` and uses
  `docker-entrypoint.sh`. `POM_BUILD_COMMIT` is exported as an `ENV` only in
  the runtime. The local `dev-runtime` target derives from it and installs
  Rust/Cargo under world-readable toolchain directories, plus native tools for
  plugin rebuilds. The cluster's optional development overlays use this target
  and give non-root nodes a writable `CARGO_HOME` in the project data volume;
  plugin build output stays in
  `<project_root>/.pom-dev-plugin-builds`, not in
  the mounted source. The release `runtime` target still has no toolchain.
- **Image context and content**: the main context ignores the local
  `llama.cpp/` copy; the fork only enters as a named context, and the build
  script removes `.git`, builds, models, credentials and dotenv files from that
  context. `.dockerignore` also excludes the Compose fixtures at the root, GGUFs,
  logs, virtual environments and artifacts generated by the frontend. It does
  not exclude `crates/**/tests` as a directory because Cargo resolves a support
  binary declared in the node's `Cargo.toml` even in the release build. The
  final image gets no Rust/C++ sources, `node_modules` or build tools. It only
  gets `pom`, `pom-plugin-host`, the GGML/llama.cpp libraries, the entrypoint
  and the frontend `dist`. Copying `dist` is the last runtime instruction/layer,
  so a UI-only change does not redo the backend assembly layers nor Cargo.
  `vulkan-tools` is installed only for operational diagnostics with
  `vulkaninfo`; POM does not depend on it for inference.
- **The builder's layer order is the cache contract**, and each `COPY` names
  exactly what the step below it reads - there is no `COPY . .`. With one, any
  single file in the tree reset both the Rust build key and the UI build key at
  once, and both ran again with nothing of theirs changed. Measured on a
  development checkout, with the real context: dropping an `infer_server_*.log` next to
  `crates/node` - a file a node started from the checkout writes on its own -
  was enough to rerun `cargo build`; a UI-only edit did the same. The Rust and
  frontend stages are siblings, with separate inputs, and what the structure
  guarantees is:
  - nothing changed -> nothing recompiles;
  - only local artifacts appeared (log, `tsbuildinfo`, `src/docs`, tests,
    models, credentials or the fork's primary tree) -> nothing recompiles,
    because they stay outside the context;
  - `apps/frontend` changed -> rebuilds the UI, reuses backend and fork;
  - `apps/frontend` and the backend changed -> rebuilds both, reuses the fork;
  - fork, CUDA architecture, Rust toolchain or the app stamp changed -> does not
    rebuild the UI; changing the CUDA toolkit changes the base and invalidates
    the stages.

  `bash scripts/test-docker-build-cache.sh` proves these scenarios against the
  `Dockerfile` itself, with real BuildKit when there is a daemon (see
  `scripts/AGENTS.md`). An `ARG` in scope enters the key of every `RUN` below
  it, even if the command does not mention it. That is why `POM_BUILD_COMMIT`
  only exists in the runtime stage, while fork, architecture and toolchain only
  exist in the Rust stage. A change only to the stamp redoes just the image's
  final layer, without running Cargo or npm. The POM binary has a single Cargo
  target (`cuda-vulkan`); the plugin host has a separate target. The common
  build runs one compilation of the executable for every cluster machine, without
  switching features in the same cache nor recompiling `llama-engine`/MoE for
  the Vulkan-only and CPU variants that no longer exist as separate binaries.
- **`docker-entrypoint.sh`**: validates the binary selected by `POM_BIN` and
  `pom-plugin-host`, exports `POM_PLUGIN_HOST=/usr/local/bin/pom-plugin-host`
  and `exec`s the path in `POM_BIN` (default `/usr/local/bin/pom`) with the
  arguments received. The Compose files pass the same `/usr/local/bin/pom` on
  every node; it includes the CPU/Vulkan/CUDA modules to choose from according
  to the visible devices. `pom` starts the host as a child only when a plugin
  is installed, passing the artifact as an argument and connecting stdin/stdout
  through length-prefixed JSON frames. So there is no Unix socket and no extra
  IPC volume. `bash scripts/test-docker-entrypoint.sh` covers the entrypoint's
  forwarding; `python3 scripts/test-docker-compose.py` validates the variant
  selection and the resulting Compose devices.
- The `target/` cache also keeps `CMakeCache.txt` and llama.cpp's native tree.
  Since the fork collects `src/models/*.cpp` through `file(GLOB ...)` without
  `CONFIGURE_DEPENDS`, a tree generated before a model file existed produces a
  `libllama.a` without the matching object and only fails at the Rust link
  (`undefined symbol: vtable for llama_model_*`). `llama-cpp-sys-2`'s
  `build.rs` calls `always_configure(true)`, but that only acts when Cargo
  reruns the build script - with the persistent cache mount it does not rerun,
  and the old `libllama.a` is relinked on every build. That is why the
  `Dockerfile` keys the `id` of the `/build/target` cache mount on
  `pom-target-v2-${LLAMA_CPP_CACHE_KEY}-${CUDA_VERSION}-${POM_CUDA_ARCH_CACHE_KEY}-${RUST_TOOLCHAIN}`:
  the `v2` prefix gives a clean mount after the previous concurrent use;
  `LLAMA_CPP_CACHE_KEY` is the published tag in `release` or the local folder's
  commit/delta in `source`; CUDA toolkit, Rust selector and architecture also
  get isolated native trees. Compose can start simultaneous builds of the same
  image for several services, so `sharing=locked` serializes access to that
  shared target and keeps two Cargo processes from writing the same artifact.
  Cargo also checks fingerprints of sources, manifests, dependencies and rustc.
  Old IDs keep taking space until a `docker builder prune`
  (`--filter type=exec.cachemount` only cleans the mounts).
  `docker compose build --no-cache` does **not** fix it: cache mounts survive
  `--no-cache`.
- **kache** is the Rust stage's compile cache. `cargo install --locked kache`
  runs right after rustup, in a layer source edits do not invalidate;
  `kache init --no-service` runs in the same `RUN` as `cargo build`, before it,
  and writes the `rustc-wrapper` and the C/C++ `[env]` keys of the build scripts
  (the fork's CMake) into the Cargo config. `--no-service` because the build
  container has no service manager. `XDG_CACHE_HOME=/build/target` puts the
  store in `/build/target/kache`, inside the `target/` cache mount: it survives
  between builds, inherits the same `id` (fork, CUDA, architecture, toolchain)
  and sits on the same file system as the targets, which lets kache link the
  artifacts instead of copying them. A build can skip the cache with
  `KACHE_DISABLED=1`.
- **`scripts/build-docker.sh`**: reads the commit **on the host**
  (`git rev-parse --short HEAD`, suffix `-sujo` if the tree has changes),
  resolves the fork mode (published release by default or local source),
  prepares a temporary named context and exports `POM_BUILD_COMMIT`,
  `LLAMA_CPP_CACHE_KEY`, the release tag/repo and `POM_CUDA_ARCH_CACHE_KEY`
  before calling `docker compose build`. `POM_LLAMA_CPP_SOURCE_DIR` can point to
  another local Git clone; the context folder excludes `.git` and large
  data/builds. The per-cluster compose files (the `pom-cluster-config`
  repository) call the same script with `COMPOSE_FILE`. It does not run
  `npm run build`: the UI is built in its own stage. Failing to read the fork's
  commit **aborts** instead of falling back to `unknown` - that value is a cache
  mount id like any other, and two different checkouts that failed the read
  would share the same native tree, which is exactly the stale `libllama.a` the
  id exists to avoid. Build the image through this script; a manual
  `docker compose build` stamps "desconhecido" (unknown) on every container, on
  purpose.
- **`scripts/run-docker.sh`**: `docker compose down` + `docker compose up -d`
  (does not rebuild the image).
- **`docker-compose.yml`**: four services shared between the cluster machines:
  `node-gpu`, `node-gpu2`, `node-gpu-3` and `node-cpu`. All inherit the same
  common definition in `docker-compose.common.yml`, which centralizes build,
  the `pom:latest` image, context, image target, volumes and
  `network_mode: host`. The local Compose uses `dev-runtime` by default;
  `POM_DOCKER_TARGET=runtime` selects the image without the toolchain. The
  default is a mixed-GPU topology, with NVIDIA 0 and Radeon on `node-gpu`,
  NVIDIA 0 on `node-gpu2`, Radeon on `node-gpu-3` and no reservation on
  `node-cpu`. The mixed-GPU machine uses `docker-compose.yml` by default;
  `docker-compose.salem.yml` can also be selected directly. The CUDA server's
  compose left this repository and lives in `pom-cluster-config`; device IDs, volumes
  and data are runtime configuration, not distinct build requirements.
  `pom-docker.sh` also generates a remote Compose with GPU reservations for
  the CUDA server. The services keep the model and state volumes in
  `tests/fixtures/<name>`. Each service sets `POM_HOME=/data`,
  `POM_UI_DIST=/app/apps/frontend/dist`, `POM_DESENV=1` and the explicit path
  `POM_PLUGIN_HOST=/usr/local/bin/pom-plugin-host` (the UI would not be under
  the mounted data directory). `POM_DEV_PLUGINS_DIR` (default `..`, the
  workspace directory containing the sibling plugin repositories) mounts the
  local sources at `/opt/pom/dev-plugins`. The optional cluster overlay
  `pom-cluster-config/pom-jair/<host>/docker-compose.dev-plugins.yml` mounts
  those sources read-only, selects `dev-runtime` and keeps Cargo's writable
  cache in the project data volume. Rust artifacts are written to
  `<project_root>/.pom-dev-plugin-builds`.
  The downloaded plugin and `enterprise.db` live in the `/data` volume; there
  is no socket volume, since
  the IPC is through child-parent pipes. Configured nodes use the ports
  persisted in `pom.json`, hence the generator below. For the first boot without
  `/data/pom.json`, `node-cpu` passes `--port 42807 --gateway-port 8083`: these
  are fallback ports only for setup mode and avoid fighting over the default
  listeners of the first GPU node on the `host` network. Outside the `host`
  network (bridge with `-p 8080:8080`), setup mode is already reachable through
  the published port: it binds to `0.0.0.0` by default (`--setup-bind` changes
  the IP). After the save the configured gateway binds to `[::]`. The generator
  writes to `tests/fixtures/node-<N>`; the services keep mounting the
  directories named by hardware role to preserve the state already persisted.
  Do not change those paths when aligning service names; see
  `scripts/AGENTS.md`.
- **`scripts/gen-node-configs.py`**: generates one `pom.json` per node in
  `tests/fixtures/node-<N>` (node 1 = principal, keeps `connection_key` and
  `api_keys`; the others = node with bootstrap to `127.0.0.1:42777`). It refuses
  to generate without a `connection_key` (inherited from the `--base` config,
  it never generates the key), `--force` overwrites and `--advertise-addr` only
  updates the advertised address, even on existing configs. If the value comes
  as `HOST:PORT`, `HOST` is shared and each config keeps its `p2p_port`; four
  processes must not announce the same socket. The ports follow the series:
  gateway 8080+, p2p 42777+, inference 8181+ (node 1 inherits the historical
  8092).
- `.dockerignore` cuts `**/target/`, `target-*/`, `.fm-secondmate/`, `.git/`,
  `venv/`, `models*/`, `source-models*/`, `*.gguf`, `*.key`, `pom.json` and
  `docs/superpowers/` from the build context - that is why the commit arrives
  through a variable and the local configuration does not enter the image. The
  `llama.cpp/` directory stays in the context because
  `vendor/llama-cpp-sys-2/llama.cpp` points to that source during the image
  build, but its `.git/` does **not**: it was 396 MB on a development machine, changes on every
  fetch in the fork and nobody reads it (the builder image has no `git`, and
  llama.cpp's `cmake/git-vars.cmake` only runs git when `find_package(Git)`
  finds one). The fork key, including local changes, arrives through
  `LLAMA_CPP_CACHE_KEY`. A `.dockerignore` pattern matches the **whole** path
  and `*` stops at `/`: it is not `.gitignore`. That is why `*.log` alone only
  covered the root, and every artifact a build or a local run leaves deeper
  needs `**/` - `**/*.log` (the `infer_server_<pid>.log` next to
  `crates/node`), `**/*.tsbuildinfo`, `**/node_modules/` and the folders
  `npm run build` generates on the host (`apps/frontend/src/docs/`,
  `apps/frontend/src/brand/`, `apps/frontend/public/brand/`). Each one left out
  entered the context and invalidated the layer that copies its directory. With
  that, the context dropped from 605 MB to about 194 MB on that machine.
- **Unnamed GPU groups in the container.** The NVIDIA runtime adds to the
  processes the groups that own the host's devices (on the CUDA server, `video` 44 and
  `render` 992); 992 does not exist in the image's `/etc/group`. The `sudo` hint
  in Ubuntu's `/etc/bash.bashrc` runs `groups` in every interactive shell and
  printed `groups: cannot find name for group id 992` in the plugins' terminals
  (harness). The image replaces that call with `groups 2>/dev/null`; nothing
  else depends on the group name.

## No Python runtime

The supported runtime is **Rust** (`crates/node/src/inference/infer_server.rs`
is the inference server; `crates/node/src/models/splitter.rs` the slicer).
The old Python scripts (`inference_server.py`, `split_model.py`) and the
legacy launchers were removed from the repository together with the `old/`
folder. The ggml type table in `gguf.rs` is checked against `GGML_TRUTH` in the
file itself (a build test), not against any Python environment. The Python
scripts that remain in `scripts/` are operational tools (for example
`gen-node-configs.py`), not part of the build.

## Cluster scripts

The bring-up procedure is in `operations.md`; the node takes no model or layer
flags on the command line.

- `scripts/bench-rotas.sh` - generation measurement battery that compares four
  origins and five prompts, writes a stable CSV with the counters and the
  per-request MTP acceptance rate at the end of the existing columns and
  prints medians per cell. `POM_BENCH_API_KEY` must hold the active inference
  API key and is sent on each call, without being printed or written to the
  CSV. The model is omitted by default to use the network's active model;
  `POM_BENCH_MODEL` pins a model only when wanted. For the origin reached over SSH,
  export the password variable without writing the password to a file.

The conservative cycle detector is off by default in each process. The
operator turns it on by starting a node with `--cycle-detection`; the window
limits and the number of repetitions are runtime parameters persisted by the
deploy screen.

The local build script cited in that step's plan is not versioned in this
repository (not even in git history).

## Important environment variables

| Variable | Effect |
|---|---|
| `POM_HOME` | First step of `project_root()` resolution: config, `models/`, UI. Without it: a cwd that looks like the project, then walking up from the executable's directory. Two nodes on the same machine need distinct homes (the `node_id` identity also lives in the project root). |
| `POM_PORT`, `POM_BOOTSTRAP`, `POM_ADVERTISE_ADDR`, `POM_OLLAMA_URL`, `POM_GATEWAY_PORT`, `POM_SETUP_BIND`, `POM_CYCLE_DETECTION`, `POM_PKARR_PEERS`, `POM_PUBLIC_ADDR`, `POM_RELAY_PEERS`, `POM_SERVE_RELAY`, `POM_NO_DEFAULT_RELAYS`, `POM_NO_DISCOVERY`, `POM_NO_PKARR` | One per command line option (`--port`, `--bootstrap`, ...; the `ARG_ENV` table in `main.rs`). The flag beats the variable, which beats the default; the booleans accept `1/0`, `true/false`, `yes/no`, `on/off`. The variable beats `pom.json`: `POM_PORT` and `POM_GATEWAY_PORT` replace the stored port, unlike `--port`/`--gateway-port`, which only apply in setup mode; `POM_BOOTSTRAP` and `POM_RELAY_PEERS` add to the config lists, like the flags. |
| `POM_FORCE_SETUP=<token>` / `--setup` (alias `--wizard`) | Forces the first access wizard on an already configured machine, to test it: before opening the database, `pom.json`, the trust root and `pom.db` (with WAL/SHM) move to `<project_root>/setup-backups/<date-time>/` and the node starts in setup mode. The identity (`.pom-node.key`) stays. To go back, stop the node and move the files back. **Once per request**, so the restart after the save does not go back to the wizard: the flag uses the `POM_SETUP_FORCED` marker in the environment inherited by the restart; the variable keeps the last consumed token in `<project_root>/.pom-setup-forced`, because in Docker the restart recreates the container with the compose environment. The same token does not force again. `scripts/up.sh <cluster> --wizard`, in the `pom-cluster-config` repository, generates a new token per call. |
| `POM_RETURN_MODE` | `direct` or `ring` forces the gateway's reply return mode (planner and decision), to test a path the planner would not pick - for example direct delivery to a gateway behind NAT, which the ring would hide. Absent or `auto`, the planner picks. With `direct`, a missing return path becomes an inference failure. See `parallelism.md`. |
| `POM_SLICE_BENCH` | `0` turns off the slice speed test when the model loads (`infer_server/perf.rs`). |
| `POM_CONFIG` | Path of `pom.json` (default `<project_root>/pom.json`). |
| `POM_DB_DIR` | Directory of `pom.db` and its WAL/SHM files. If absent or empty, uses `<project_root>`; the directory is created at startup. |
| `POM_DEV` | The common Compose sends `1` for development configuration; it is not the flag that enables local rebuilds. |
| `POM_DESENV` | Only the exact value `1` enables the panel and the AdminOnly plugin rebuild/reinstall routes. The local Compose sends that value. |
| `POM_DEV_PLUGINS_DIR` | Host directory with known plugin sources (`..` by default, the workspace containing the sibling plugin repositories), mounted at `/opt/pom/dev-plugins` in the local Compose. |
| `POM_UI_DIST` | Where `webapp.rs` reads the UI bundle (default `<project_root>/apps/frontend/dist`). The Docker image sets it explicitly. |
| `POM_MODEL_DIRS` / `POM_SHARD_DIR` | Whole-model folders (several, separated like `PATH`: `:` on Unix, `;` on Windows) and the slice folder. They beat `model_dirs`/`shard_dir` in `pom.json` and are not persisted; empty counts as absent. The Storage screen locks the field set by one of them. |
| `POM_SLOT_SAVE_PATH` | KV cache snapshot directory (llama-server's `--slot-save-path`). Beats the `slot_save_path` field of `pom.json`. Per process, not persisted. With neither set, the default is `~/.pom/kv-slots`; an empty value turns off `POST /v1/session/slot` and `/admin/slots`. See `inference.md` §3.1.1. |
| `POM_SLOT_PERSIST_PREFIX_CACHE` | Persistent prefix cache (the gateway writes/restores the retained KV across restarts): `1`/`true`/`yes`/`on` turns it on, any other value turns it off. Beats `slot_persist_prefix_cache` in `pom.json`; requires `slot_save_path` on every hop. On by default. See `inference.md` §3.1.1. |
| `POM_RAG` | `0` turns transparent retrieval off on this gateway. `POM_RAG_TIMEOUT_MS` (default 300), `POM_RAG_PASSAGES` (default 4) and `POM_RAG_MIN_SCORE` (default 0.8) tune it. See `plugins-ui.md`, "Transparent retrieval (RAG)". |
| `POM_CACHE_RAM` | Host-memory KV cache in MiB (llama-server's `--cache-ram`): a positive ceiling, `0` for off, `-1` or unset for as much as the machine allows. Beats `cache_ram_mib` in `pom.json`. Half of the host RAM always stays available to the user and a host with less than 12 GiB caches nothing. See `inference.md` §3.1.2. |
| `POM_SLOT_AUTO_TTL_SECS` | TTL of an unused automatic prefix snapshot, in seconds (default 7 days). Beats `slot_auto_ttl_secs` in `pom.json`. Read at gateway startup. |
| `POM_DEV_AUTO_ACCEPT_TRANSFERS` | `=1` makes the **receiving** node accept every transfer offer without a click. Development only; read once at startup. The sender cannot ask for it. |
| `POM_DEV_UPDATE` | `=1` turns on the "Update and restart" gate on the peer card (`devupdate.rs`). Per process, never in the config. |
| `POM_BUILD_FEATURES` | Features the machine builds with in devupdate (`cuda`/`metal`); without it a CUDA machine loses the GPU on the first click. |
| `POM_CUDA_GRAPHS` | `=1`, `true`, `yes` or `on` enables CUDA Graphs in the CUDA build. Off by default to reduce the risk of OOM at graph launch. It is a compile option, so it requires rebuilding the binary. |
| `POM_BUILD_COMMIT` | Commit shown in `/api/ui/node`; in Docker it is passed by `build-docker.sh` as a runtime variable, outside the Cargo/npm cache. Without it, uses the fallback compiled by `build.rs`. |
| `LLAMA_SOURCE` | An already prepared local `llama.cpp` checkout; must contain `ggml`, is normalized to an absolute path and avoids copying the fork into the worktree. |
| `POM_LLAMA_CPP_REPOSITORY` / `POM_LLAMA_CPP_REF` | Remote and branch used by the automatic preparation when the fork is missing. The defaults are the public fork and `pom-tensor-parallel`. |
| `POM_LLAMA_CPP_AUTOCLONE` | Use `=0` to block the automatic download and only get the message with the manual procedure. |
| `VITE_API_TARGET` | Proxy target of the UI dev server (default `http://127.0.0.1:8080`). |

## Release through GitHub Actions

Three workflows, one per system, all manual only (`workflow_dispatch`):

| File | Target | Runs on | Inputs |
|---|---|---|---|
| `.github/workflows/release-linux.yml` | Linux x86_64 (CPU, and optional CUDA) | self-hosted Linux runner (label `ksia`) | `version`, `cuda`, `prerelease` |
| `.github/workflows/release-macos.yml` | macOS arm64, Metal | self-hosted macOS runner (label `macbook`) | `version`, `prerelease` |
| `.github/workflows/release-windows.yml` | Windows x86_64, CUDA + Vulkan and/or CPU | hosted (`windows-2022`) | `version`, `variantes` (`gpu`/`cpu`/`ambos` - "both"), `prerelease` |

The packages are uploaded as artifacts only to pass between jobs of the same
run, with a `retention-days` of 1 or 2 days; the final ones are attached to
the release. The 90-day default once exhausted the account's artifact storage
quota and blocked new releases.

**They are separate so they can run alone.** Publishing only macOS should not
cost a Windows build, and a target that breaks does not take the others down.
Each workflow publishes its own package **on the same tag**: the first to run
creates the release, the following ones attach their file. Rerunning only the
Linux one does not delete the macOS package that was already there.

### The llama.cpp fork is the step that cannot be missing

`vendor/llama-cpp-sys-2/llama.cpp` is a **versioned symlink** to the
repository root, and the folder it points to (`/llama.cpp/`) is in
`.gitignore`. On a clean checkout, `llama-cpp-sys-2`'s own `build.rs` detects
the broken symlink and automatically clones the public fork
`pereirajair/llama.cpp`, branch `pom-tensor-parallel`, before walking the
native files. So CI, releases and local `cargo build` follow the same path,
without pinning a fork commit.

That branch also carries the local fix to `common_fit_params`: free memory is
captured before the dry context, because the recurrent state buffers are real
even with `no_alloc=true`. The project does not version the content the symlink
points to, so updating the fork is part of updating the dependency.

On Windows the checkout can materialize the symlink as a text file or a
reparse point. `build.rs` does not follow that entry: it removes the
placeholder and installs the clone directly at the path Cargo reads
(`<manifest_dir>/llama.cpp`). That leaves a real directory for `WalkDir` and
avoids `PermissionDenied` on the depth-zero entry. On Unix, the clone is
installed at the symlink's relative target, keeping the versioned layout. If
the clone cannot be downloaded, the error lists the full command for manual
preparation, instead of leaving `walkdir`'s raw `NotFound`.

Before the native build, `release-windows.yml` compiles and runs
`vendor/llama-cpp-sys-2/src/build_support.rs` with the runner's `rustc`. That
smoke test confirms on Windows itself that a versioned symlink does not become
the root walked by `WalkDir` and that the Unix target stays preserved.

### Preparing a local worktree

The versioned symlink at `vendor/llama-cpp-sys-2/llama.cpp` resolves to
`<worktree>/llama.cpp`, but the root's content is ignored by git. The first
`cargo build -p node` already triggers the automatic preparation. To reuse a
local checkout of the fork without copying it into the worktree, set
`LLAMA_SOURCE`:

```bash
LLAMA_SOURCE=/path/to/llama.cpp
test -d "$LLAMA_SOURCE/ggml"
cargo build -p node
```

If `llama.cpp` already exists at the worktree root, confirm it is the fork's
checkout and contains `ggml`; an existing directory without `ggml` is not
overwritten automatically. `POM_LLAMA_CPP_REPOSITORY` and `POM_LLAMA_CPP_REF`
can point the automatic preparation to another remote or branch during tests,
without making that reference a fixed project dependency.

### The UI must go in the package

The UI is read from disk at runtime (`config::ui_dist_dir`), not embedded in
the binary: without `apps/frontend/dist` in the package, the node starts and
answers 500 on the screen.

In the two self-hosted workflows it is built in the job itself, because there
is no minute multiplier there and splitting it would only add an artifact
round trip between two jobs that would run on the same machine anyway - the
runner takes one job at a time. In the Windows one, which is hosted, the `ui`
job stays on `ubuntu-latest`: there a Windows minute counts 2x and building the
same UI there would cost double for the same `dist`.

The two self-hosted workflows pin Node with `actions/setup-node` instead of
using the system one. The runner records a **snapshot of the PATH of whoever
configured it**, and that snapshot ages: the Linux runner has Node 18, and on macOS Node
comes from nvm, whose path changes when the default version changes.

### Self-hosted runners

Linux and macOS run on our own machines, so they **use no quota**:

| Runner | Machine | Labels | Service |
|---|---|---|---|
| Linux (CUDA build server) | Ubuntu 24.04 x86_64, 8 cores, 46 GB | `self-hosted, Linux, X64, ksia, cuda` | systemd, user `github-runner` |
| macOS (Apple Silicon laptop) | macOS arm64 | `self-hosted, macOS, ARM64, macbook, metal` | LaunchAgent, at the user's login |

**The Linux runner runs as `github-runner`, without sudo, and that is
deliberate.** A self-hosted runner executes whatever is in the workflow of any
branch of the repository; as root, anyone able to open a branch here would get
root on the machine. Without sudo, the damage is limited to the runner's user.
For the same reason dependencies are provisioned **once on the machine** and the
workflow only checks they are there, instead of installing on every run.

On macOS the runner runs in the user's own account - inherent to self-hosted on
a personal machine: a workflow has the same access as the logged-in user. That
holds as long as whoever can push a branch to this repository is trusted.

Maintenance commands:

```bash
# Linux runner
ssh root@<linux-runner> '~github-runner/actions-runner/svc.sh status'
ssh root@<linux-runner> '~github-runner/actions-runner/svc.sh stop'
# macOS runner
~/actions-runner/svc.sh status
```

If the machine is off, the job **stays queued** instead of failing.

To remove a runner: `./svc.sh uninstall` and then `./config.sh remove
--token <removal token>` (`gh api --method POST
/repos/<owner>/<repo>/actions/runners/remove-token`).

### Quota (Windows workflow only)

Actions is free and unlimited on a **public** repository. On this one, which is
private, the plan's monthly quota applies (2,000 minutes on Free) with a
multiplier per system: Linux 1x, Windows 2x, macOS 10x. Since Linux and macOS
moved to our own runners, only the Windows one uses quota - and it is the only
one still worth watching.

A clean build from scratch - including native llama.cpp - was measured at
**3m33s** on Apple Silicon; a `target/` of this project measures **~9 GB**,
which matters on the Linux runner, where disk is the tight resource. The Linux workflow
warns (without failing) when less than 15 GB is left.

### GPU architectures: wide for release, narrow for development

llama.cpp's CMake compiles **each `.cu` file for ten architectures**
(`compute_50` to `121a`). Measured on the CUDA build server: 186 objects at ~3/min, more than
twenty minutes on that alone - and nine of the ten architectures are for cards
that machine does not have.

The two situations want opposite things, so they are configured in different
places:

- **Release** (these workflows): the wide list, because the binary will run on
  machines other than the one that built it. The Linux workflow's build step
  forces `CUDAARCHS: ''` - **this is not an oversight**. The job runs on a
  self-hosted runner, and a development machine may have `CUDAARCHS` in its
  environment to shorten the local build; inheriting that value would silently
  produce a release that only works on the builder's GPU. `pom-release.sh`
  applies the same rule to the `pom_core` build: it also removes an inherited
  `CMAKE_CUDA_ARCHITECTURES` and runs Cargo with `CUDAARCHS=''`. CMake treats
  the empty value as unset, so the vendored `CMakeLists.txt` takes the wide
  list path. The Enterprise cdylib build does not get this cleanup because it
  does not compile CUDA.
- **Development** (the operator's local build script, outside the repository): only the machine's
  architecture, through `CUDAARCHS`. CMake >= 3.20 initializes
  `CMAKE_CUDA_ARCHITECTURES` from that environment variable, so `build.rs` does
  not need to be touched.

Note the `a` suffix of `120a`/`121a` in llama.cpp's list: it enables
architecture-specific instructions. CMake only accepts that suffix from 3.30
on, so on a machine with an older CMake the possible value is `120`, without
the `a`.

**What the wide list costs.** Measured on the CUDA build server on 2026-08-25, same code, same
machine, ~141 CUDA objects generated once per architecture:

| Configuration | Time |
|---|---|
| ten architectures (no `CUDAARCHS`) | 24+ min, still not finished |
| `CUDAARCHS=120` | 10m29s |
| `CUDAARCHS=120` with ccache already populated | 6m00s |

In other words: **building without `CUDAARCHS` is almost always a waste**. The
wide list is only justified for a binary that will be distributed to unknown
hardware - the release case.

Architecture of each cluster machine:

| Machine | GPU | `CUDAARCHS` |
|---|---|---|
| CUDA build server | RTX 5060 Ti | `120` |
| mixed-GPU machine | two architectures | `"120;86"` |

The CUDA build server is `compute_120` and nothing else. The mixed-GPU machine
has **two** architectures and
needs both together - `86` alone does not work there. For validation and local
development - the common case - use the value of the machine you are building
on.

`ccache` is installed on the CUDA build server, with the cache in `/data/ccache`, and llama.cpp's
build system detects it on its own. It does **not** speed up the Rust part;
only C/C++/CUDA, which is exactly the expensive step.

**Gotcha** (also noted in the operator's local build script, outside the
repository): changing the `CUDAARCHS` value invalidates the previous native
build, and cargo alone does not notice the change - llama.cpp's native build
has to be redone by hand.

### Release on the self-hosted Linux runner (`release-ksia.yml`)

A separate, manual workflow (`workflow_dispatch`) that builds **everything on
the self-hosted Linux runner** and uses no GitHub minutes or storage. The workflows that build on GitHub
still exist; this is the quota-free alternative. Inputs: `version` (tag, e.g.
`v0.4.0`), `linux_cpu`, `linux_cuda`, `windows_cpu` (each turns on one job, all
`true` by default) and `prerelease`.

| Job | Package | Timeout |
|---|---|---|
| `linux-cpu` | `pom-<v>-x86_64-unknown-linux-gnu-cpu.tar.gz` | 4 h |
| `linux-cuda` | `pom-<v>-x86_64-unknown-linux-gnu-cuda.tar.gz` | 24 h |
| `windows-cpu` | `pom-<v>-x86_64-pc-windows-msvc.zip` | 4 h |

All three run on `[self-hosted, Linux, X64, ksia]` and in sequence
(`concurrency: release-ksia`, a single runner). Each job calls
`scripts/release-ksia.sh --variant ... --version ...` and publishes its own file
with `softprops/action-gh-release` on the same tag; the jobs are independent, so
one that fails does not stop the others. **There is no `upload-artifact`,
`actions/cache` or `setup-node` npm cache**: all of that lives in GitHub storage,
which has a quota. The only traffic to GitHub is the checkout and the upload of
the final package to the release. `scripts/test-release-ksia.sh` checks that and
the dry run of each variant.

- **linux-cuda** builds with CUDA 12.9 (`POM_RELEASE_CUDA_HOME`, default
  `/usr/local/cuda-12.9`), which is the installed toolkit with the widest list
  of architectures, and with `CUDAARCHS=''` and `CMAKE_CUDA_ARCHITECTURES`
  removed - llama.cpp's full list (see "GPU architectures"). Hence the 24 h
  timeout.
- **windows-cpu** is a **cross-compilation** from Linux: `cargo xwin` downloads
  the Windows SDK and the MSVC CRT (`XWIN_ACCEPT_LICENSE=1`), and llama.cpp's
  CMake uses `clang-cl`/`lld-link`/`llvm-lib`/`llvm-rc` from LLVM 20
  (`POM_RELEASE_LLVM_BIN`, default `/usr/lib/llvm-20/bin`). **It must be Clang
  19 or newer**: the MSVC STL xwin downloads (14.44) refuses older versions with
  `error STL1000: Unexpected compiler version, expected Clang 19.0.0 or newer`,
  and Ubuntu 24.04's default LLVM 18 hit that. The script checks the version
  before building and puts that LLVM first in `PATH` through symlinks in
  `tools/msvc-shims`, because Ubuntu only installs `clang-cl` and `lld-link`
  inside `/usr/lib/llvm-N/bin`. CUDA on Windows needs the Windows toolkit and
  cannot be built from here: this package is CPU only. **It has not been run
  end to end yet**; the first run may need script adjustments.

**State that survives between runs** lives in `POM_RELEASE_CACHE` (default
`~/.cache/pom-release` of the `github-runner` user): a shallow clone of the
llama.cpp fork (redone when the remote sha of `pom-tensor-parallel` changes), a
`target-<variant>` per variant, the Windows tools (`cargo-xwin`, `ninja`,
shims), the `xwin` cache and the `ccache` cache (`CCACHE_DIR` is forced to
`ccache/` in there: a `cache_dir` in `ccache.conf` the user cannot write made
every native object fail with `Permission denied`). Count **~10 GB per variant**
plus ~3 GB for the SDK; the Linux jobs warn when the disk has less than 15 GB.
To free space, delete the whole folder - the next run rebuilds from scratch.

**One-time provisioning (root).** Already installed on the Linux runner: `clang`, `mold`,
`cmake`, `zip`, CUDA 12.9 and LLVM 20 (`apt install clang-20 lld-20 llvm-20`,
from Ubuntu 24.04's default repository, installed on 2026-09-28). The rest
(`cargo-xwin`, `ninja`, SDK) the script downloads into the user's space. If
something is missing, the script stops with the list of what is missing, before
building.

Locally, without Actions: `scripts/release-ksia.sh --variant linux-cpu
--version v0.4.0 --dry-run` shows the commands; without `--dry-run` it builds
and writes to `dist-release-ksia/` (ignored by Git). `POM_RELEASE_SKIP_UI=1`
skips the UI build.

`--debug-symbols` (`windows-cpu` only) produces a diagnostic build with
`pom.pdb` in the zip: full debuginfo, `/Z7` in C/C++ so the PDB covers
llama.cpp, LTO off and 16 codegen-units (`fat` LTO with full symbols blows the
linker's memory). It uses `target-windows-cpu-debug`, separate from the
optimized target. Cross-built from Linux, the `.exe` comes out **without an
icon** (`embed_windows_icon` depends on the host's `cfg(windows)`).

### The package

The binary sits at the package root with `apps/` next to it, so
`project_root()` finds the UI from the executable's directory - the unpacked
folder works anywhere, without `POM_HOME`. `pom-plugin-host`
(`pom-plugin-host.exe` on Windows) goes **next to the binary** in every package:
the workspace's `default-members` is only `crates/node`, so every release build
asks for `-p node -p pom-plugin-host` explicitly, and pom only starts plugins
through that sibling (`plugin_host::host_executable`, or `$POM_PLUGIN_HOST`). A
package without it refuses to install any plugin with "pom-plugin-host not
found next to ...". On top of that, per system: macOS ships the `POM.app` bundle
(with a second copy of `dist` inside, so the `.app` survives being dragged into
`/Applications`), Linux ships `share/hicolor` and the `.desktop`, and Windows
ships nothing extra because the icon is already inside the `.exe`. Unix packs
with `tar czf`, Windows with `7z`.

**Windows is the least exercised target.** It fails on its own, without
affecting the other two. `release-windows.yml` has two variants (input
`variantes`), each a matrix job and a package:

- **`gpu`** (default): `--features cuda,vulkan` in the same `pom.exe`, package
  `pom-<v>-x86_64-pc-windows-msvc-cuda-vulkan.zip`. CUDA for NVIDIA; Vulkan
  covers NVIDIA, AMD (e.g. RX 7800 XT) and Intel. The job installs only the CUDA
  components in use (`nvcc`, `cudart`, `cccl`, `cublas`...), downloaded from
  NVIDIA's "redist" archives through the version manifest (`CUDA_VERSION`), and
  LunarG's Vulkan SDK (`VULKAN_VERSION`). It builds with Ninja inside the MSVC
  environment (`ilammy/msvc-dev-cmd`), because CUDA with the Visual Studio
  generator requires the MSBuild integration. Architectures: a **reduced list**
  `75;80;86;89;120` (`POM_WINDOWS_CUDAARCHS`) while the repository is private -
  a Windows minute counts 2x and a hosted job dies at 6 h. It includes 86
  (RTX 3060) and 120 (RTX 5060 Ti), the cluster's cards; when the project goes public,
  switch to `''` for the full list. On Windows CUDA is linked **dynamically**
  (`llama-cpp-sys-2/build.rs`), so the package ships `cudart64_*.dll`,
  `cublas64_*.dll` and `cublasLt64_*.dll` next to `pom.exe` (redistributable
  under NVIDIA's license); `vulkan-1.dll` comes with the video driver. The own
  MoE kernels (`crates/llama-engine/build.rs`) come out as `pom_moe_cuda.lib`
  through `lib.exe` with `/MD` on MSVC, and as `.a` through `ar` elsewhere.
- **`cpu`**: no features, package `pom-<v>-x86_64-pc-windows-msvc.zip`.

The GitHub build no longer produces `pom.pdb`; to diagnose a crash on Windows,
use `scripts/release-ksia.sh --variant windows-cpu --debug-symbols` (see
"Release on the self-hosted Linux runner"). **The `gpu` variant has not been run yet**.

## Defects noted

- DEFECT NOTED: `routing-base::plan` (its only real logic) is called by nobody -
  the node's planner (`crates/node/src/routing/pipeline.rs::plan_pipeline`)
  makes its own greedy plan. `routing-base` is dead code kept alive only by its
  own tests. Postponed until the open-core decision (see `TODO.md`).
- DEFECT NOTED: the `pom_core` update (`pom-release.sh` +
  `pom_core_update.rs`) publishes and swaps **only** the `pom` binary. An
  installation born without `pom-plugin-host` next to it stays without it
  after updating, and a change to the host's IPC protocol does not arrive
  through that path. Today the host only arrives through the full package
  (zip/tar.gz) or the Docker image.
