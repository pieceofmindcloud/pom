# AGENTS.md - crates/routing-contract/

## What this folder is

Library crate `routing-contract` (depends on `serde`). The **versioned data
contract** between the node and any routing provider: what a peer announces, what
a planner is asked, and what a valid plan looks like. Deliberately transport-free
and runtime-free so a closed/private provider could implement against it without
touching node internals.

## Files

- `Cargo.toml` - deps: `serde` (workspace); dev-deps: `serde_json` for
  round-trip tests.
- `src/lib.rs` - all contract types + validation logic + unit tests.

## Types (all in `src/lib.rs`, all `Serialize`/`Deserialize`)

- `ROUTING_API_VERSION: u16 = 1` (`lib.rs:6`) - version negotiated by a node and
  any routing provider; every request/plan carries it.
- `ModelCapability` (`lib.rs:14`) - one layer range a peer can serve for one
  model revision. `model_id` is an opaque stable revision id (not a UI label);
  `total_layers` is `Option` for interop with the first inventory format.
- `ModelInventory` (`lib.rs:29`) - the full capability list exposed by one peer.
- `PeerCapacity` (`lib.rs:37`) - the per-peer subset a planner sees for one
  decision (peer_id + layer range + supports_inference).
- `ClusterSnapshot` (`lib.rs:46`) - `total_layers` + `peers` for one planning
  decision.
- `RoutingRequest` (`lib.rs:53`) - versioned planner input.
- `RoutingStep` (`lib.rs:71`) - one ordered peer/range in a plan.
- `RoutingPlan` (`lib.rs:79`) - ordered, full-coverage plan output.
- `RoutingError` (`lib.rs:124`) - `UnsupportedApiVersion`, `EmptyModel`, `Gap`,
  `InvalidRange`, `Incomplete`.

## Key logic

- `RoutingRequest::validate` - rejects a mismatched `api_version` **and** a
  malformed snapshot: every `PeerCapacity` must have `end_layer >= start_layer`
  and stay inside `total_layers`. This is the entry gate every provider is
  expected to call first, so a lying peer is named on the way in instead of
  surfacing later as a bad plan. `total_layers == 0` means the cluster does not
  know the total yet (see `catalog.rs`), so only the ordering is checked then.
- `PeerCapacity::validate` / `ModelCapability::validate` - the same range check
  for one entry; the capability one uses the total it declares itself.
- `RoutingPlan::validate` (`lib.rs:85`) - checks a plan is a **contiguous,
  gap-free, exactly-covering** walk of `0..total_layers`: contiguous start, in
  range, no holes (`Gap`), complete coverage (`Incomplete`). This is the shared
  notion of a correct plan.

## Links

- Consumers: `crates/routing-base` (`use` at `routing-base/src/lib.rs:3`) and the
  `node` crate (`network/network.rs:4` for `ModelCapability`, `routing/pipeline.rs:8`
  for `ClusterSnapshot`/`PeerCapacity`/`RoutingRequest`).
- `node/src/routing/pipeline.rs::routing_request_from_peers` builds a
  `RoutingRequest` from live peers and asserts `api_version` in tests; the node
  planner validates against this contract but does its own planning.
- Siblings: see `crates/AGENTS.md`.

## Defeitos anotados

- Contract says `end_layer` is inclusive; an "exclusive end" future wire version
  is flagged in a comment but not implemented.
