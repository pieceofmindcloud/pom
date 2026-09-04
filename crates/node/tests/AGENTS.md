# tests/ - testes de integração do node

- `mtp_arch.rs` inclui o extrator usado por `crates/llama-engine/build.rs` para
  que as regras de tokenização, mapeamento e falha ruidosa também rodem no
  harness normal do Cargo. O teste não lê nem altera o fork do `llama.cpp`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
