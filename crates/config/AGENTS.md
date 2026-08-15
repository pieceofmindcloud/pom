# AGENTS.md - crates/config/

## What this folder is

Library crate `config` (no dependencies). Intended home for configuration
schemas and persistence adapters shared across node components.

## Files

- `Cargo.toml` - empty crate, version/edition from `workspace.package`, no deps.
- `src/lib.rs` - only a doc comment; **the crate has no code**. It states the
  existing node configuration "remains in the binary until a later,
  behavior-preserving extraction migrates it here".

## Key logic

None. No types, functions, or tests exist in this crate.

## Links

- The real configuration lives in the node crate's own module:
  `crates/node/src/utils/config.rs` (392 lines: `Role`, `ApiKey`, `Config`,
  `project_root()`, `ui_dist_dir()`, `default_shard_dir()`, etc.). Referenced as
  `crate::config` throughout node; do not confuse it with this crate.
- Nothing in the workspace imports this crate.

## Defeitos anotados

- Empty scaffolding crate: declared in the workspace but dead weight until the
  extraction from `crates/node/src/utils/config.rs` happens. Anyone touching node
  config today must edit the node module, not this crate.
