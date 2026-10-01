# Delivery report: Hugging Face provider

Date: 2026-08-24
Branch: `fm/rdma-hf-warnings`
Specification: kept outside this repository

## 1. Implemented architecture

The new subsystem separates the `ModelProvider` contract from the
`HuggingFaceProvider` implementation. The contract covers discovery,
inspection, license, authentication, download, progress, manifest and storage,
without mentioning the CLI.

`hf_cli.rs` isolates argument building, async execution with separate
stdout/stderr, process cancellation, JSON parsing, the final download path and
error classification. `huggingface.rs` adapts those results to the contract,
validates inputs and computes the local manifest. `LocalModelStorage` reuses
the SHA-256 hash already used by `models/pom.rs`.

The backend uses `check_authentication` in
`GET /api/ui/model-provider/status`, returning the structured state of the
CLI, version, authentication and user without exposing a token. The remote
detail calls `get_license` before the UI offers the download, keeps the
`gated` marker and shows the detected or unknown license.

The result is storage, enumeration and hashing only. No downloaded content is
executed, no token is accepted as an argument and no remote execution option
is sent.

## 2. Files created

- `crates/node/src/models/provider.rs`
- `crates/node/src/models/hf_cli.rs`
- `crates/node/src/models/huggingface.rs`
- `docs/reports/hf-provider-reuse.md`
- `docs/reports/hf-provider.md`
- `docs/superpowers/specs/2026-08-24-rdma-hf-provider-design.md`
- `docs/superpowers/plans/2026-08-24-rdma-hf-provider.md`

## 3. Files changed

- `crates/node/src/main.rs`: declaration of the three new modules.
- `crates/node/src/models/AGENTS.md`: responsibilities and limits of the new
  modules.
- `crates/node/AGENTS.md`: updated map of the `models` subsystem.
- `docs/AGENTS.md`: pointers to the survey and this report.

## 4. Important technical decisions

- The CLI is a replaceable implementation of `ModelProvider`, not a UI
  dependency.
- The runner accepts a `CommandSpec` with `Vec<String>` and uses `.args(...)`;
  there is no shell and no concatenation of user input.
- `hf` manages the credential. POM only queries `hf auth whoami` and never
  calls `hf auth token`, logs a token, sends a token to telemetry or includes
  raw output in an error.
- `repo_id`, revision, file, filters and paths are validated before building
  commands or manifests. Revisions accept inner slashes for branches but
  refuse traversal components.
- Structured queries prefer `--format json`. The download uses only the final
  path printed by the CLI and treats textual progress as optional data.
- GGUF reuses `catalog::parse_quant`; quantization, size and base name stay
  absent when the name does not allow a safe determination. An unknown license
  returns `detected: false`.
- Telemetry uses `tracing` with provider, repository, revision, bytes,
  duration, status and error kind, without stdout, stderr or credentials.
- The HTTP route and the screen consume only `ModelProvider`: authenticated
  CLI status, search, details with license and structured downloads.
  `with_runner` is restricted to tests to inject the fake executor; the real
  process is only created by the production runner.

## 5. Commands for manual testing

With the full `llama.cpp` checkout available at the expected path:

```bash
hf --version
hf auth whoami
hf models ls --search qwen --format json
hf models info owner/model --format json
hf models ls owner/model --revision main --format json
hf models card owner/model --metadata --format json
hf download owner/model model-Q4_K_M.gguf --revision main --cache-dir ./hf-cache
```

The POM domain commands are:

```bash
rtk cargo test -p node hf_cli::tests -- --nocapture
rtk cargo test -p node huggingface::tests -- --nocapture
```

## 6. Tests added

- Validation of `repo_id`, revision and file name against traversal.
- GGUF quantization detection and no invented metadata.
- Unknown license with `detected: false`.
- Local manifest with size and SHA-256.
- Parsing of the CLI's version, authentication, search, files, license, final
  path and progress.
- Classification of missing authentication, nonexistent repository, rate
  limit and download failure.
- Fake runner for missing CLI, search, GGUF, success, failure and
  cancellation.
- Structured status for missing CLI, session without login and authenticated
  user; a failure of the `card` process keeps the fallback to `models info`.
- Check of separate arguments and no token in errors/debug.

Verifiable result of this run:

- `rtk cargo build -p node`: passed without the Hugging Face provider
  warnings.
- `rtk cargo test -p node`: 752 passed, 5 ignored.
- `rtk cargo fmt --check`: passed.
- `rtk cargo clippy -p node`: exit code zero; pre-existing warnings remain in
  other subsystems, none in the changed files.
- `rtk npm test`: 50 files and 291 tests passed.
- `rtk npm run build`: TypeScript and Vite passed; Vite keeps the informative
  warning about a chunk above 500 kB.

## 7. Known limitations

- Progress depends on the text lines printed by the CLI; total bytes, current
  file and speed stay optional when they cannot be determined safely.
- The main UI chunk is above 500 kB minified; the build passes, but a future
  split by route could reduce the initial load.

## 8. Next steps

1. Register the `ModelManifest` in the local catalog and connect it to the
   slicing/deployment flow, without distributing weights automatically in this
   phase.
2. Add fixtures for CLI versions and also test Windows/macOS in CI once the
   `node` crate can be built on those platforms.
