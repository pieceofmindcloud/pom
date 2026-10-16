# AGENTS.md - crates/routing-base/

## What this folder is

Library crate `routing-base` (depends only on `routing-contract`). The
**deterministic open-source routing provider**: given a `ClusterSnapshot`, it
produces a complete ordered `RoutingPlan`. Transport and product-specific policy
can swap in a different provider without touching the contract.

## Files

- `Cargo.toml` - deps: `routing-contract` (path).
- `src/lib.rs` - one function `plan()` + unit tests.

## Key logic

- `plan(&RoutingRequest) -> Result<RoutingPlan, RoutingError>` (`lib.rs:12`) -
  greedy forward sweep from layer 0 to `total_layers`:
  - validates the request (api version **and** the peer ranges in the snapshot,
    via `routing_contract::RoutingRequest::validate`) and the resulting plan
    (`routing_contract::RoutingPlan::validate`),
  - at each expected start, picks the peer whose range begins exactly there,
    preferring the **longest** range; ties broken by lexicographically smaller
    `peer_id` (`lib.rs:20-31`),
  - if no peer starts at the expected position, returns `Gap` reporting the next
    actual start (`lib.rs:33-46`),
  - guarantees full contiguous coverage or no plan at all.

## Links

- Input/output types come from `routing-contract` (`routing-base/src/lib.rs:3`).
- **Not consumed by the node binary.** The node's production planner
  `crates/node/src/routing/pipeline.rs::plan_pipeline` does its own greedy
  planning with load scoring (`layers * 100 - load * 10`) and only borrows the
  contract types. `routing-base::plan` is exercised solely by its inline tests.
- Siblings: see `crates/AGENTS.md`.

## Open items

The open provider is neither called by the binary nor compared against the
served planner; see `TODO.md`, item 5.
