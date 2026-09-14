# Windows Crash Loop and Relaunch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a Windows launch crash survivable: persist deployment attempts across process deaths, expose a safe undeploy path, relaunch only after an unexpected exit, and destroy the native WebView shell in a deterministic order.

**Architecture:** Keep the deployment breaker beside the existing `.last-slice.json` journal because it already survives process death and is cleared by the runtime's manual `stop`. A Windows parent process supervises a child that owns Tao/Wry; clean exit from the tray is status 0, while a crash or controlled restart returns nonzero so the parent can relaunch. The shell explicitly unregisters its menu handler and drops tray, WebView, and window before returning or exiting.

**Tech Stack:** Rust, Tokio, Tao, Wry, tray-icon, serde JSON, Cargo unit tests.

**Spec:** Captain launch brief in the task conversation; the existing deployment lifecycle is documented in `docs/subsystems/implantacao.md` and `crates/node/src/inference/AGENTS.md`.

## Global Constraints

- Do not use the Windows GitHub Actions workflow; validate with local tests and source-level review.
- Do not treat `/health model_loaded` as a mere log message: it is the only success transition that clears the pending deployment breaker.
- Manual undeploy must clear persisted restart state before the next process start.
- The node-facing `/api/ui/leave` path must work without model health, readiness, or admin authority; it stops local inference and detaches the node.
- Never edit generated frontend documentation or changelogs.
- Keep all prose in touched project documentation in pt-BR and use a plain hyphen, never an em dash.

### Task 1: Persisted deployment attempt journal

**Files:**
- Modify: `crates/node/src/models/lastslice.rs`
- Modify: `crates/node/src/inference/runtime.rs`
- Modify: `crates/node/src/main.rs`
- Test: unit tests in `crates/node/src/models/lastslice.rs` and `crates/node/src/inference/runtime.rs`

**Interfaces:**
- `lastslice` stores `attempts` in the existing JSON annotation and exposes a startup decision that preserves `Pending` until success or explicit suppression.
- `NodeRuntime::apply` records a fresh pending attempt before announcing or starting the loader; the startup resume path increments the persisted attempt without resetting it.
- `main` suppresses a pending deployment after the existing runtime restart budget is exhausted and reports the model/range through the node status.

- [x] **Step 1: Write the failing journal tests.** Add tests proving that a pending annotation survives a read, increments across simulated starts, becomes resumable below the established restart budget, and is cleared after manual stop or explicit suppression.
- [x] **Step 2: Run the focused Rust tests and verify they fail for the missing attempt state.**

```bash
rtk cargo test -p node lastslice -- --nocapture
```

- [x] **Step 3: Implement the smallest persistent journal change.** Add a serde-defaulted attempt counter, explicit `begin_attempt`, `resume_attempt`, `mark_serving`, and `clear` operations, retaining backward compatibility for annotations without the new field.
- [x] **Step 4: Add the runtime integration regression.** Assert that a newly applied configuration writes `Pending` before the network announce and that the health watcher is the only path that writes `Serving`.
- [x] **Step 5: Run the focused runtime tests and the full node test suite.**

```bash
rtk cargo test -p node runtime -- --nocapture
rtk cargo test -p node
```

### Task 2: Native Windows shell teardown and crash supervisor

**Files:**
- Modify: `crates/node/src/windows_status.rs`
- Modify: `crates/node/src/utils/respawn.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/AGENTS.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/subsystems/build-execucao.md`
- Test: unit/source-contract tests in `crates/node/src/windows_status.rs` and `crates/node/src/utils/respawn.rs`

**Interfaces:**
- `windows_status::run` starts a supervisor parent unless the private child marker is present; the child owns the GUI/service and returns status 0 only for a user-requested exit or a normal one-shot command.
- The parent restarts a child with the original argv after an abnormal exit, while a controlled `respawn_self` on a supervised Windows child exits with the restart status and leaves relaunching to the parent.
- Shell teardown unregisters the tray callback and drops tray, WebView, and Tao window in that order before process exit.

- [x] **Step 1: Write failing lifecycle tests.** Cover the exit-code distinction, child marker, teardown ordering contract, and the fact that a tray quit does not request a relaunch.
- [x] **Step 2: Run the focused tests and verify the new contracts fail before production changes.**
- [x] **Step 3: Implement deterministic teardown in the native shell.** Make all event-loop exit paths unregister the menu handler and explicitly drop native resources before propagating a service error or honoring user quit.
- [x] **Step 4: Implement the lightweight Windows parent/child supervisor.** Preserve argv and environment, launch the child without a console, wait for its status, and relaunch abnormal exits. Keep Unix/macOS behavior unchanged.
- [x] **Step 5: Update the authoritative node and UI/build documentation.** Document that the Windows shell has a parent supervisor, user tray quit is terminal, and native Windows verification is still required.
- [x] **Step 6: Run focused tests, all node tests, and a normal build.**

```bash
rtk cargo test -p node windows_status respawn -- --nocapture
rtk cargo test -p node
rtk cargo build
```

### Task 3: Field validation handoff and delivery

**Files:**
- Modify: `docs/subsystems/implantacao.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `crates/node/src/models/AGENTS.md`
- Modify: `docs/AGENTS.md` only if the delivered behavior adds durable cross-session knowledge

- [x] **Step 1: Add the verified persistent-breaker and Windows validation notes in pt-BR.** Include the exact local commands and the remaining captain check for Windows Event Viewer, crash dumps, and WebView2 teardown.
- [x] **Step 2: Re-run the complete verification commands and inspect the final diff for generated files, unrelated changes, and documentation consistency.**
- [x] **Step 3: Rebase onto the current `origin/feat/implantar3`, commit the finished branch, and report the final local SHA according to the delivery contract.**
