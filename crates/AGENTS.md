# AGENTS.md - crates/

## What this folder is

Workspace root of the Rust crates. Defined in `/Cargo.toml` (workspace members,
shared `[workspace.package]` metadata, shared `[workspace.dependencies]` for
`anyhow`, `serde`, `serde_json`). Five members, one binary:

- `config` - configuration schemas/persistence (currently empty scaffolding)
- `types` - infrastructure-free shared domain types (currently empty scaffolding)
- `routing-contract` - versioned data contract for routing providers
- `routing-base` - deterministic open-source routing provider
- `node` - the only binary (`rdma-p2p`), the whole product

`default-members = ["crates/node"]`, so `cargo build`/`cargo test` at the root
builds and tests the node crate. `resolver = "2"`.

## Dependency graph (verified by reading Cargo.tomls + grepping `use`)

- `node` -> `routing-contract` (path dep, `crates/node/Cargo.toml:49`). Uses
  `routing_contract` types in `src/network/network.rs:4` and
  `src/routing/pipeline.rs:8`.
- `routing-base` -> `routing-contract` (`crates/routing-base/Cargo.toml:7`).
- `config`, `types` -> no dependencies, consumed by nothing.

Note: **node does NOT depend on config, types, or routing-base.** The `crate::config`
references inside node (`main.rs`, `backend/*`, `network/identity.rs`, etc.) are its
own module `crates/node/src/utils/config.rs`, not the `config` crate. The
`config`/`types` crates are declared in the workspace but nothing links them yet.

## Intent vs reality

`docs/superpowers/plans/2026-08-11-open-core-workspace-foundation.md` describes an
incremental extraction: keep node's current modules working while public crates are
carved out. Only `routing-contract` (and the standalone `routing-base`) have real
content so far; `config` and `types` are placeholders awaiting migration.

## Links

- `node` is the product: web gateway, P2P network, inference pipeline (see
  `crates/node/`).
- `routing-contract` is the seam a future private/closed routing provider would
  implement against; `routing-base` is the open one.

## Defeitos anotados

- `routing-base::plan` (its only real logic) is **not called anywhere** - node's
  planner `crates/node/src/routing/pipeline.rs::plan_pipeline` builds a
  `RoutingRequest` from the contract types for validation but does its own greedy
  planning with load scoring. `routing-base` is currently dead code kept alive
  only by its own unit tests.
- The doc in this file and the plan doc describe `node` depending on `config` and
  `types`; the code does not. Not a bug, but the workspace graph diverges from the
  stated architecture until the extraction lands.
