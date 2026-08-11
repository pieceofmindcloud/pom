# Open-core workspace foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the current single Rust package into an open-core workspace foundation without changing node behavior, and add the stable routing contract used by the future private plugin.

**Architecture:** Keep `node` as the only binary and retain the existing source modules temporarily. Create focused public crates with no project-name prefix: `types`, `config`, `routing-contract`, and `routing-base`. The existing routing implementation remains the production path while an adapter conversion proves the extracted data contract does not change its observable plan.

**Tech Stack:** Rust 2021 workspace, serde, anyhow, Cargo features, existing unit-test framework.

## Global Constraints

- Do not change existing routing, inference, network, HTTP, configuration, or UI behavior.
- Do not include private routing source, dependency names, features, assets, or artifacts in the open-source workspace.
- Crate and folder names must not start with `rdma`.
- Preserve `rdma-p2p` as the shipped binary name and preserve `cargo build`, `cargo test`, and `--features cuda|metal` at the root.
- This work occurs only in the current MacBook worktree and must not contact ksia.
- Every code addition follows red, green, refactor with an observed failing test.

---

### Task 1: Introduce a workspace without changing the binary boundary

**Files:**
- Create: `crates/types/Cargo.toml`
- Create: `crates/types/src/lib.rs`
- Create: `crates/config/Cargo.toml`
- Create: `crates/config/src/lib.rs`
- Modify: `Cargo.toml:1-57`
- Test: `crates/types/src/lib.rs`

**Interfaces:**
- Produces: public `types` and `config` library crates with no dependency on node infrastructure.
- Consumes: workspace-level `serde` and `anyhow` dependency declarations.

- [ ] **Step 1: Write the failing workspace membership test**

Add an inline `types` test that imports `crate::WorkspaceMarker` and asserts it is constructible. The type does not exist yet.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p types workspace_marker_is_public`

Expected: compilation fails because crate `types` or `WorkspaceMarker` is unavailable.

- [ ] **Step 3: Create minimal public library crates and workspace manifest**

Add `[workspace]` with `members = [".", "crates/types", "crates/config", "crates/routing-contract", "crates/routing-base"]`, `resolver = "2"`, shared package metadata and shared `serde`/`anyhow` dependencies. Keep the root `[package]`, `[[bin]]`, feature names and direct dependencies. Implement only `pub struct WorkspaceMarker;` in `types` and a minimal `config` crate module.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p types workspace_marker_is_public`

Expected: PASS.

- [ ] **Step 5: Run root regression verification**

Run: `cargo test --workspace`

Expected: all existing tests plus the new test pass.

### Task 2: Define the versioned, infrastructure-free routing contract

**Files:**
- Create: `crates/routing-contract/Cargo.toml`
- Create: `crates/routing-contract/src/lib.rs`
- Test: `crates/routing-contract/src/lib.rs`

**Interfaces:**
- Produces: `ROUTING_API_VERSION`, `ClusterSnapshot`, `PeerCapacity`, `RoutingRequest`, `RoutingStep`, `RoutingPlan`, and `RoutingError`.
- Consumes: serde from the workspace and no node crate.

- [ ] **Step 1: Write failing serialization and validation tests**

Add tests asserting a contiguous two-step plan validates, a plan with a gap returns `RoutingError::Gap`, and JSON round-trips without losing `api_version`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p routing-contract`

Expected: compilation fails because the contract types do not exist.

- [ ] **Step 3: Implement the minimal serde contract**

Define only JSON-safe scalar and collection fields. `RoutingPlan::validate(total_layers)` verifies first layer 0, adjacent ranges, inclusive end layers, and full coverage. It must not import `PeerInfo`, reqwest, axum, filesystem paths or plugin code.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p routing-contract`

Expected: PASS.

### Task 3: Extract a deterministic open routing planner

**Files:**
- Create: `crates/routing-base/Cargo.toml`
- Create: `crates/routing-base/src/lib.rs`
- Test: `crates/routing-base/src/lib.rs`

**Interfaces:**
- Consumes: `routing_contract::{RoutingRequest, RoutingPlan, RoutingError}`.
- Produces: `pub fn plan(request: &RoutingRequest) -> Result<RoutingPlan, RoutingError>`.

- [ ] **Step 1: Write failing planner fixture tests**

Add tests for a complete two-peer contiguous snapshot, a missing layer, and deterministic tie handling. The expected output is a `RoutingPlan` ordered by layer range.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p routing-base`

Expected: compilation fails because `plan` does not exist.

- [ ] **Step 3: Implement the minimal planner**

Order candidates by start layer, choose the highest capacity then lexical peer id only for an equal range, and delegate coverage checks to `RoutingPlan::validate`. Do not change the old planner yet.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p routing-base`

Expected: PASS.

### Task 4: Add a node-side conversion seam, with no routing behavior change

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/pipeline.rs`
- Test: `src/pipeline.rs`

**Interfaces:**
- Consumes: `routing_contract` types and existing `network::PeerInfo`.
- Produces: crate-private `routing_request_from_peers(total_layers, peers)` used only by tests initially.

- [ ] **Step 1: Write a failing conversion test**

Add a `pipeline.rs` test that makes two existing `PeerInfo` values, converts them, and asserts peer id, start/end layers, inference endpoint capability and total layers remain equal.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test pipeline::tests::routing_request_from_peers_preserves_peer_ranges`

Expected: compilation fails because the conversion function does not exist.

- [ ] **Step 3: Implement the minimal conversion**

Add the `routing-contract` path dependency to the root package and convert only values already present in `PeerInfo`. Do not call `routing-base::plan` from production code in this task.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test pipeline::tests::routing_request_from_peers_preserves_peer_ranges`

Expected: PASS.

- [ ] **Step 5: Run complete verification**

Run: `cargo test --workspace && cargo build --release --features release-bundle`

Expected: both commands exit zero. If `web-ui/dist-release` is intentionally absent, run `cargo build --release` instead and record that release-bundle was not testable in the task report.

### Task 5: Document the private plugin boundary without shipping private code

**Files:**
- Create: `docs/architecture/routing-plugin-contract.md`
- Modify: `README.md`
- Test: `crates/routing-contract/src/lib.rs`

**Interfaces:**
- Produces: documented local IPC protocol ownership, compatibility rules and fallback behavior.
- Consumes: the public routing contract API version.

- [ ] **Step 1: Write a failing API-version compatibility test**

Add a `routing-contract` test asserting a request with an unsupported API version returns `RoutingError::UnsupportedApiVersion`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p routing-contract unsupported_api_version_is_rejected`

Expected: FAIL because the API version is not checked.

- [ ] **Step 3: Implement version checking and docs**

Validate API version in `RoutingRequest::validate`. Document that only a future private executable may implement the local RPC, and the public node always falls back to `routing-base` when no compatible plugin is available.

- [ ] **Step 4: Run verification**

Run: `cargo test --workspace && git diff --check`

Expected: both commands exit zero.

### Task 6: Isolate Python compatibility utilities from the supported runtime

**Files:**
- Move: `inference_server.py` to `old/python-inference/inference_server.py`
- Move: `split_model.py` to `old/python-inference/split_model.py`
- Move: `split.py` to `old/python-inference/split.py`
- Move: `inspect_model.py` to `old/python-inference/inspect_model.py`
- Move: `inspect_gguf.py` to `old/python-inference/inspect_gguf.py`
- Modify: `run-inf.sh`
- Create: `old/python-inference/README.md`

**Interfaces:**
- Produces: an explicit compatibility-only location for Python adapters.
- Preserves: `run-inf.sh` as the manual legacy launcher.

- [ ] **Step 1: Move only legacy Python scripts**

Move the five scripts to `old/python-inference/`; do not change their method bodies or Python protocol.

- [ ] **Step 2: Retarget the compatibility launcher**

Change only the Python file path in `run-inf.sh` to `old/python-inference/inference_server.py`.

- [ ] **Step 3: Verify syntax and launcher reference**

Run: `python3 -m py_compile old/python-inference/*.py && rg -n 'old/python-inference/inference_server.py' run-inf.sh`

Expected: Python compilation succeeds and the launcher reports exactly the relocated path.
