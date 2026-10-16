# tests/ - node integration tests

- `mtp_arch.rs` includes the extractor used by `crates/llama-engine/build.rs` so
  that the tokenization, mapping and loud-failure rules also run in the normal
  Cargo harness. The test neither reads nor alters the `llama.cpp` fork.
- `startup_config.rs` runs the `pom` binary with a temporary `POM_HOME` inside
  the worktree and proves that an invalid configuration reaches the Rust `main`
  with a message and a non-zero status, including on the macOS AppKit loop.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file, command, or test over copying implementation detail.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.