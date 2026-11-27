# Plugin sandbox - design

Date: 2026-11-27. Status: proposed, waiting for the operator's approval.
Plan: `docs/superpowers/plans/2026-11-27-plugin-sandbox.md`.

## Goal

Every POM plugin runs in its own sandbox, on Linux, macOS and Windows. A
plugin, and every process it starts, cannot see, signal, debug or read the
memory of any other process of the user's machine - other plugins and the POM
node included. Each plugin has its own sandbox, never one shared by all.

What a plugin keeps:

- read and write in the user's workspace (`workspace_root`, default
  `~/pom_workspace`), which every plugin shares - it is how plugins exchange
  files;
- its own home (`<pom data dir>/<plugin_code>/`) and database
  (`POM_PLUGIN_DB`), visible to no other plugin;
- the POM node's API on loopback (`host_api`, the `/v1` gateway), which is how
  a plugin sends a notification or a message to another plugin;
- what its manifest declares and the operator saw at install: network, GPU,
  inbound listening.

## Decisions

1. **Mandatory, at once.** No plugin runs outside a sandbox once this ships.
   It is a breaking change, written in the release notes: an installed plugin
   whose release has no `sandbox` section runs with the most restrictive
   default (below) and may stop working until its new release is installed.
   The `pom-plugin-*` repositories ship their new releases in the same window.
   Operator decision, 2026-11-27.
2. **Network is declared in the manifest** (operator decision, option 1).
   The operator's first idea was that plugins only reach `localhost` and that
   every outside access comes through the POM API; the verification of the
   repositories on 2026-11-27 found it true for inbound and false for
   outbound:
   - Inbound: every plugin server binds `127.0.0.1` (code-server, ComfyUI,
     vLLM, Strata, Splash, train, jev, harness) except **Qdrant**, which binds
     `0.0.0.0` on purpose so the network's heads reach it
     (`pom-plugin-qdrant/src/supervisor.rs`, `QDRANT__SERVICE__HOST`).
   - Outbound: ComfyUI's installer, Qdrant, Strata, code-server, harness and
     others download runtimes or call HTTPS services themselves; only
     `hugging-face`, `llama.cpp`, `splash`, `vllm` (at run time) and `base`
     need no internet.
   So "loopback only" would break Qdrant and every installer; the manifest
   declares instead.
3. **The sandbox is applied by `pom-plugin-host`, before it loads the
   `cdylib`.** The host is already the process boundary (one process per
   plugin, `crates/plugin-host`); everything the plugin starts is its
   descendant and inherits the sandbox on all three systems (Linux namespaces,
   Landlock and seccomp are inherited; a Seatbelt profile is inherited; an
   AppContainer token is inherited by children). The node never loads plugin
   code, so it stays outside.
4. **A machine that cannot sandbox does not run plugins.** (Amended
   2026-11-27 after the Linux spike: the minimum on Linux is Landlock ABI >= 6
   + seccomp; the PID namespace is an extra layer, used where the system
   allows it and shown in the panel, not a requirement. See
   `docs/reports/2026-11-27-plugin-sandbox-linux-spike.md`.) If the host cannot
   establish the minimum isolation of its system (below), it refuses to load
   the plugin and the panel names the reason (`sandbox_unavailable:<why>`).
   There is no "run without sandbox" switch in a release build;
   `POM_DEV_MODE=1` alone may add `POM_PLUGIN_SANDBOX=off` for debugging.

## Manifest: the `sandbox` section

Optional field of the root manifest, omitted when absent (keeps the signed
canonicalization of older manifests, per the compatibility policy):

```json
"sandbox": {
  "network": "loopback",        // "loopback" | "internet"
  "listen": "loopback",         // "loopback" | "network" (Qdrant)
  "gpu": false,                 // NVIDIA/AMD/Intel/Metal device access
  "read_paths": []              // extra read-only paths, absolute or ~/..., shown at install
}
```

Absent section = `{network: loopback, listen: loopback, gpu: false,
read_paths: []}`. `loopback` always includes the node's gateway port, so
`host_api` and `/v1` work with every value. The Plugins screen shows the
declared permissions before install and update, and an update that asks for
more permissions asks the operator again.

## What each sandbox allows

| | Allowed | Denied |
|---|---|---|
| Processes | its own descendants | seeing, signalling, `ptrace`, reading memory of any other process |
| Files, read-write | plugin home, its `.db`, workspace, a private temp dir | everything else, other plugins' homes, `.pom-node.key`, `pom.json`, `pom.db` |
| Files, read-only | its install dir, system libraries and runtimes, `read_paths` | user's home outside the workspace |
| Network out | loopback (node port + own ports); internet if declared | everything else |
| Network in | loopback; all interfaces if `listen: network` | |
| Devices | GPU nodes if `gpu: true` | other devices |

## Per system

### Linux

The host, before `dlopen`:

1. **Namespaces** (`unshare`): user + PID + mount + IPC. In the PID namespace
   the plugin sees only its own tree; `/proc` is remounted for the new
   namespace. The mount namespace binds the allowed paths. Network namespace
   is **not** used: it would hide the node's loopback.
2. **Landlock**: filesystem rules from the table; on ABI >= 4 TCP bind and
   connect rules for `network: loopback`; on ABI >= 6 the signal and abstract
   socket scopes.
3. **seccomp** filter: denies `ptrace`, `process_vm_readv/writev`,
   `kcmp`, `bpf`, `perf_event_open`, kernel module and keyring calls, and new
   user namespaces from inside.
4. `PR_SET_NO_NEW_PRIVS`, then load the plugin.

Minimum to run: (PID namespace available) **or** (Landlock ABI >= 6 +
seccomp). Risk: Docker's default seccomp profile and Ubuntu 24's
`apparmor_restrict_unprivileged_userns` block unprivileged user namespaces.
The published image needs a seccomp profile that allows `unshare` for the host
or relies on the Landlock path; phase 0 measures both on the cluster machines
and on a stock Ubuntu desktop. GPU: bind `/dev/nvidia*`, `/dev/nvidia-uvm*`,
`/dev/dri/*`, `/dev/kfd` and the driver libraries read-only.

### macOS

The host calls `sandbox_init` with a Seatbelt (SBPL) profile generated from
the manifest: `(deny default)`, file rules from the table, `process-info*` and
`signal` only for `(target self)` and children, `network-outbound` limited to
`localhost` unless `internet`, `network-inbound` per `listen`, `mach-lookup`
to the services a process needs, IOKit GPU classes when `gpu`. The API is
deprecated but is what Chromium and Bazel use; it needs no entitlement and no
signing change. Today a process that detaches escapes the host's tree on macOS
(`crates/plugin-host/AGENTS.md`); the sandbox does not change that walk, the
process stays sandboxed anyway.

### Windows

The node starts the host in an **AppContainer** of its own, one per
`plugin_code` (profile name derived from it), inside the kill-on-close Job
Object it already uses:

- file ACLs granted to that AppContainer SID on the plugin home, its `.db`,
  its install dir and the workspace (granted, not relabelled: other programs
  keep their access);
- capability `internetClient` only for `network: internet`,
  `internetClientServer`/`privateNetworkClientServer` for `listen: network`;
- AppContainers cannot open processes outside them, which gives the process
  isolation.

Risks, measured in phase 0: AppContainer blocks loopback to processes outside
it, which is the node's API - it needs the loopback exemption
(`NetworkIsolationSetAppContainerConfig`, one elevated call at install) or the
`host_api` moving into the IPC pipes; CUDA inside an AppContainer must be
proven on a real GPU.

## Plugin to plugin

Files: the shared workspace, read-write for every plugin.

Messages, new `host_api` routes (loopback, `x-pom-plugin-host-token`; the
sender is the token's plugin, never a field the caller fills):

| Route | Body | Effect |
|---|---|---|
| `POST /api/plugin-host/message` | `{target, type, payload}` (payload <= 64 KiB) | delivered to `target` as the native event `plugin.message {from, type, payload, id}` and in the browser through `pom-plugin-events/v1`; `404 not_installed`, `409 not_running` |
| `POST /api/plugin-host/notify` | `{title, body?, level?, scope}` | the existing `notification.notify`, now callable from the plugin's process and not only its screen |

A plugin receives messages from any plugin of the same machine; it may answer
to `from`. The `/api/plugin-host/plugins` listing already tells who is
installed and running.

## Out of scope

Limits on CPU, memory or GPU share (the sandbox isolates, it does not
ration); sandboxing the POM node itself; plugins on other machines (they talk
through `peer.<op>`, unchanged).

## Open questions for phase 0

- Docker: seccomp profile for the host versus the Landlock-only path.
- Windows: loopback exemption at install versus `host_api` over IPC.
- CUDA under AppContainer and under the Linux mount namespace (vLLM, Strata,
  ComfyUI).
