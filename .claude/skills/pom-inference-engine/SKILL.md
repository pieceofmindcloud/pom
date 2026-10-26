---
name: pom-inference-engine
description: Use when adding a new inference engine to the POM from a GitHub repository URL (a pom-plugin-<engine> that runs models on that engine), when implementing in an engine fork what the POM needs (generation contract, sessions, decisions, vision, layer slices), or when proposing those engine branches to the official repository.
---

# Adding an inference engine to the POM

## Overview

A new engine is two repositories: the engine **fork** (branch `pom`, built
from neutral `feature/*` and `fix/*` branches) and a **plugin**
`pom-plugin-<engine>` that supervises the engine and speaks the POM's
`engine.*` contract. POM glue lives in the plugin, never in the engine tree;
engine capabilities live in the fork, proposable upstream.

**REQUIRED SUB-SKILL:** creating-pom-plugins (the plugin scaffold and contract).

## Input

A GitHub URL of the engine fork (for example `https://github.com/<org>/<engine>`).
If it is not a fork, ask for the official repository.

## Workflow

1. **Intake.** `bash .claude/skills/pom-inference-engine/scripts/engine-intake.sh <url>`
   clones next to the other repositories, sets `upstream`, creates `pom`.
2. **Assess.** Read the engine (README, server, API, build, backends). Fill a
   table of `feature-levels.md` L0 to L6: native, bridge-only, or engine
   branch needed. Read `fork-analysis.md` for how llama.cpp and Strata did it.
3. **Scaffold the plugin.**
   ```bash
   bash .claude/skills/pom-inference-engine/scripts/new-engine-plugin.sh \
     ../pom-plugin-<engine> --code <code> --name "<Engine>" \
     --engine-repo <fork git URL> --upstream <official URL> --engine-commit <pom sha>
   ```
   Adapt `src/launch.rs` (program, flags, health path, `ARCHITECTURES`,
   `VISION`, `STAGES`, `POM_SETTINGS`), `src/settings.rs` and
   `scripts/build-runtime.sh`. `git init -b main`.
4. **Implement level by level** (`feature-levels.md`). Engine work: one
   `feature/<capability>` or `fix/<defect>` branch each, from
   `upstream/<default>`, neutral (`scripts/check-neutral.sh`), merged into
   `pom` (`branches-and-upstream.md`); then move `engine.lock`. Plugin work:
   the bridge, the service flags and tests next to the code.
5. **Verify** after each level:
   ```bash
   scripts/build-ui.sh && POM_UI_REQUIRE_DIST=1 cargo test --locked
   cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings
   scripts/build-runtime.sh && cargo build --release --locked
   python3 .claude/skills/pom-inference-engine/scripts/verify-engine-plugin.py \
     --host <pom-plugin-host> --lib target/release/libpom_plugin_<engine>.so \
     --code <code> --model <model file>
   ```
   Without the engine yet, `--runtime <dir>` whose `bin/server` is
   `scripts/mock-openai-server.py` checks the plugin alone. Then deploy on a
   dev node (on the right hardware) and run a chat, a raw completion and the
   levels claimed.
6. **Upstream, only when asked.** `branches-and-upstream.md`, last section.

## References

- `engine-contract.md` - the POM side: service, `engine.*`, `/pom/generate`,
  probe, sessions, prefill/decode, stages, health.
- `feature-levels.md` - what to build, where, and how to check it.
- `fork-analysis.md` - every change the llama.cpp and Strata forks and
  pom-plugin-strata carry, as features, characteristics and fixes.
- `branches-and-upstream.md` - fork branches, integration, PRs.

## Common mistakes

| Mistake | Fix |
|---|---|
| POM names or paths in an engine branch | Generic option; glue in the bridge; `check-neutral.sh` |
| Feature branch cut from `pom` | Cut from `upstream/<default>`, merge into `pom` |
| Runtime committed or embedded | `build/runtime` (gitignored), pinned release asset |
| Engine server on `0.0.0.0` or with the key on the command line | Loopback; the bridge holds the token |
| Reasoning flattened into `<think>` text | `r` lines from `reasoning_content` |
| `ready` before the model loaded | Health route that waits for the load |
| Unsupported request silently answered | `{"error": "this engine does not ..."}` |
| Rewriting `pom` after an upstream move | Merge upstream into `pom`; tag pinned commits |
