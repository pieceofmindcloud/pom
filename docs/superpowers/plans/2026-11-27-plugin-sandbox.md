# Plugin sandbox - plan

Spec: `docs/superpowers/specs/2026-11-27-plugin-sandbox-design.md`.

## Phase 0 - spikes (no product code)

- Linux: a test program that does the spec's namespace + Landlock + seccomp
  sequence, run on a stock Ubuntu 24 desktop, inside the published Docker
  image, and on the cluster machines; record the kernel's Landlock ABI and
  whether user namespaces work. Run vLLM and ComfyUI under it with the GPU.
- macOS: the SBPL profile around the llama.cpp and ComfyUI plugins with Metal.
- Windows: AppContainer start with ACL grants, loopback to the node, CUDA.
- Output: `docs/reports/2026-12-xx-plugin-sandbox-spikes.md` and the answers
  to the spec's open questions.
- [x] Linux probe on the host and in Docker, GPU included:
  `docs/reports/2026-11-27-plugin-sandbox-linux-spike.md`.

## Phase 1 - contract

Delivered 2026-11-27, with backward compatibility (spec decision 1: a release
without `sandbox` is a legacy plugin and runs as before).

- [x] `crates/enterprise-protocol`: `PluginManifest.sandbox` (optional,
  omitted when absent), validation, defaults, unknown values read as the
  narrowest, `sandbox_widens`. The GitHub manifests ignore unknown fields.
- [x] The node keeps each install path's declaration; `PluginStatus.sandbox`
  and `sandbox_enforced` (false until phase 2).
- [x] Plugins screen: Not sandboxed / Sandbox declared / Sandbox badge,
  permissions in the install dialog, an update that asks for more marked in
  the dialog and in the updates list; i18n and the screen guide.
- [x] `host_api`: `POST /api/plugin-host/message`, `POST /api/plugin-host/notify`,
  native `plugin.message` event, browser event, `plugin.notice` in the bell.
- [x] `host_api` over a Unix socket (`host_api.socket_path`), for plugins in
  their own network namespace.
- [x] The plugin template's package metadata carries `sandbox`.
- [ ] The license server must carry `sandbox` from the package metadata into
  the signed `PluginManifest` (outside this repository); until then a
  licensed release reads as legacy.

## Phase 2 - Linux

- `crates/plugin-host/src/sandbox/linux.rs`; the node passes the policy
  (paths, ports, GPU) to the host as a JSON argument it builds from the
  manifest and `config`.
- Refusal path `sandbox_unavailable:<why>` up to the panel.
- Tests: a fixture plugin (`crates/enterprise-plugin-fixture`) that tries to
  list processes, signal the node, read `pom.json` and another plugin's home,
  and connect out; each must fail, workspace writes must pass.

## Phase 3 - macOS

- `sandbox/macos.rs` (SBPL generation + `sandbox_init`), same fixture tests.

## Phase 4 - Windows

- AppContainer profile per plugin, ACL grants and revocation on uninstall,
  loopback solution chosen in phase 0, started from
  `crates/node/src/plugin_host.rs`; same fixture tests.

## Phase 5 - plugins

- Every `pom-plugin-*` gets its `sandbox` section and a release; the
  `creating-pom-plugins` skill template and reference document it.

## Phase 6 - release

- Release notes with the breaking change; `enterprise-licensing.md`,
  `plugins-ui.md`, `security-model.md`, `crates/plugin-host/AGENTS.md` and the
  Plugins screen guide updated.
