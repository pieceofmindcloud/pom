# AGENTS.md - crates/

## What this folder is

Workspace root of the Rust crates. Defined in `/Cargo.toml` (workspace members,
shared `[workspace.package]` metadata, shared `[workspace.dependencies]` for
`anyhow`, `serde`, `serde_json`). Six members, one binary:

- `brand` - the product identity from `/brand.json`, embedded with `include_str!`
  (was `node/src/utils/brand.rs`). Consumed by `node` and `llama-engine`.
- `llama-engine` - **all** the code that talks to `llama-cpp-sys-2`: the FFI
  wrapper (`llm`), MTP (`mtp`), the in-house MoE executor (`moe`), the TP reduce
  barrier (`collective`), GGUF header parsing (`gguf`/`multipart`), KV budget
  (`kv_budget`), and system/GPU monitoring (`sysmon`). Extracted from `node` so
  editing backend/network/routing code does not rebuild it. Owns the C/CUDA
  shims and the `mtp_arch` build step. Features `cuda`/`metal`/`vulkan` forward
  to `llama-cpp-sys-2`.
- `types` - infrastructure-free shared domain types (currently empty scaffolding)
- `routing-contract` - versioned data contract for routing providers
- `routing-base` - deterministic open-source routing provider
- `node` - the only binary (`pom`), the whole product

`default-members = ["crates/node"]`, so `cargo build`/`cargo test` at the root
builds and tests the node crate. `resolver = "2"`.

## Dependency graph (verified by reading Cargo.tomls + grepping `use`)

- `node` -> `routing-contract`, `llama-engine`, `brand` (all path deps in
  `crates/node/Cargo.toml`).
- `llama-engine` -> `brand`, `llama-cpp-sys-2` (patched to `vendor/`).
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

## Pendências

- O planner open-core ainda não está ligado ao caminho de produção; ver `TODO.md`,
  item 5.
- A extração de `config` e tipos compartilhados ainda não corresponde ao grafo
  atual; ver `TODO.md`, item 6.

Os crates `enterprise-protocol`, `enterprise-plugin-fixture` e `plugin-host`
formam a fronteira de licenciamento: o primeiro define DTOs e ABI C v1, o
segundo é uma `cdylib` de teste e o terceiro é o único carregador de plugins.
O licenciamento é por `plugin_code` independente, não de um produto único -
`pom-plugin-host` sobe uma instância por plugin ativo, e os nomes com
"enterprise" ficaram por história de git (o plugin de logs agora é `"logs"`). O contrato e as lacunas de extração
estão em `docs/subsystems/enterprise-licensing.md`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
