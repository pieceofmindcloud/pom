# AGENTS.md - crates/types/

## What this folder is

Library crate `types` (no dependencies). Intended home for infrastructure-free
domain types shared by the public crates, so they can be reused without pulling
in the node binary's heavy dependencies.

## Files

- `Cargo.toml` - empty crate, version/edition from `workspace.package`, no deps.
- `src/lib.rs` - defines only `pub struct WorkspaceMarker` plus a test asserting
  it is public/constructible. Everything else is a doc comment saying concrete
  shared domain types "land here incrementally".

## Key logic

None of consequence. `WorkspaceMarker` (`crates/types/src/lib.rs:5`) is a proof
that the workspace exposes a library crate; it carries no behavior.

## Links

- Consumed by nothing in the workspace today.
- Sibling crates that may eventually consume it: `routing-contract` and the
  `node` crate (see `crates/AGENTS.md`).

## Open items

The extraction of shared domain types has not been completed yet; see
`TODO.md`, item 25.
