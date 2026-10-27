# AGENTS.md - crates/

## What this folder is

Workspace root of the Rust crates. Defined in `/Cargo.toml` (workspace members,
shared `[workspace.package]` metadata, shared `[workspace.dependencies]` for
`anyhow`, `serde`, `serde_json`). Six members, one binary:

- `brand` - the product identity from `/brand.json`, embedded with `include_str!`
  (was `node/src/utils/brand.rs`). Consumed by `node` and `llama-engine`.
- `llama-engine` is **not in this workspace**: it lives in the repository
  `pom-plugin-llama.cpp` (private), pinned by `rev` in `crates/node/Cargo.toml`.
  It holds **all** the code that talks to `llama-cpp-sys-2`: the FFI wrapper
  (`llm`), MTP (`mtp`), the in-house MoE executor (`moe`), the TP reduce barrier
  (`collective`), GGUF header parsing (`gguf`/`multipart`), KV budget
  (`kv_budget`), and system/GPU monitoring (`sysmon`), plus the C/CUDA shims and
  the `mtp_arch` build step. `llama-cpp-sys-2` (the binding) lives there too and
  is patched into the workspace from the same `rev`. The features
  `cuda`/`metal`/`vulkan` forward to `llama-engine`.
- `types` - infrastructure-free shared domain types (currently empty scaffolding)
- `routing-contract` - versioned data contract for routing providers
- `routing-base` - deterministic open-source routing provider
- `node` - the only binary (`pom`), the whole product

`default-members = ["crates/node"]`, so `cargo build`/`cargo test` at the root
builds and tests the node crate. `resolver = "2"`.

## Dependency graph (verified by reading Cargo.tomls + grepping `use`)

- `node` -> `routing-contract`, `brand` (path deps) and `llama-engine` (git
  dependency on `pom-plugin-llama.cpp`, see above).
- `llama-engine` (external) -> `llama-cpp-sys-2` (external, patched from the same
  repository). It no longer depends on `brand`: its GGUF key prefix is the
  constant `llama_engine::GGUF_PREFIX`, which `node`'s test
  `llama_engine_contract` checks against `brand::slug()`.
- `routing-base` -> `routing-contract` (`crates/routing-base/Cargo.toml:7`).
- `types` -> no dependencies, consumed by nothing.

`node/src/main.rs` re-exports the `llama-engine` modules (`pub use
llama_engine::{llm, mtp, moe, gguf, wire, sysmon, ...}`) and aliases `brand`, so
`crate::llm::…` / `crate::brand::…` keep resolving across `node` unchanged. Three
small items moved with the engine and are re-exported by their old home:
`KvCacheType` (from `config.rs`), `DEFAULT_FIT_MEMORY_MARGIN_MB` (from
`runtime.rs`). `inference/tp.rs` stays in `node` (the collective is networked);
`llm` reaches its decode-time state through `llama_engine::tp_probe`, which
`node` fills in at startup (`tp::install_probe`). `sysmon` gets its disk root
the same way (`sysmon::set_disk_root`), which is the project root and can
legitimately be a network path (a shared folder holding config/models). On
Windows, `sample_disk_at` refuses to measure a network path directly (UNC or a
mapped drive letter, detected via `GetDriveTypeW`) and falls back to the local
`%SystemDrive%` instead - otherwise every peer running its binary from the same
share reported the disk of whichever machine hosts it, identical across the
whole cluster.

Note: **node does NOT depend on types or routing-base.** The `crate::config`
references inside node (`main.rs`, `backend/*`, `network/identity.rs`, etc.) are its
own module `crates/node/src/utils/config.rs`, not a separate crate.
The `types` crate is declared in the workspace but nothing links it yet.
The `config` crate was removed as empty scaffolding; the real config lives in
`crates/node/src/utils/config.rs` and awaits the modular-architecture decision
before extraction.

## Intent vs reality

`docs/superpowers/plans/2026-08-11-open-core-workspace-foundation.md` describes an
incremental extraction: keep node's current modules working while public crates are
carved out. Only `routing-contract` (and the standalone `routing-base`) have real
content so far; `types` is a placeholder awaiting migration.

## Links

- `node` is the product: web gateway, P2P network, inference pipeline (see
  `crates/node/`).
- `routing-contract` is the seam a future private/closed routing provider would
  implement against; `routing-base` is the open one.

## Open items

- The open-core planner (`routing-base`) is not wired into the production path
  yet, and no test compares it with the node's planner.

The crates `enterprise-protocol`, `enterprise-plugin-fixture` and `plugin-host`
form the licensing boundary: the first defines the DTOs and the C ABI v1, the
second is a test `cdylib`, and the third is the only plugin loader. Licensing is
per independent `plugin_code`, not for a single product - `pom-plugin-host`
brings up one instance per active plugin, and the "enterprise" names remain only
as git history (the logs plugin is now `"logs"`). The contract and the
extraction gaps are in `docs/subsystems/enterprise-licensing.md`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
