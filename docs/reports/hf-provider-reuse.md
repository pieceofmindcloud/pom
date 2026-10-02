# Reuse survey for the Hugging Face provider

Date: 2026-08-24

This survey was done before implementing the integration with the `hf` CLI,
following the operator's specification. The design rule is to keep Hugging Face
behind a provider abstraction: the application knows `ModelProvider`, not the
`hf` command.

## Reused modules and services

- `crates/node/src/models/` will be the boundary of the new subsystem. The
  catalog in `catalog.rs` already discovers GGUF files locally and exposes
  `parse_quant`, which will be reused for quantization without duplicating the
  parser. `model.rs` stays responsible for inference profiles; it will not be
  coupled to the provider.
- `models/pom.rs` already defines the manifest, SHA-256 hashing (`hash_file`)
  and content-based file identity. The remote download manifest will be
  specific to `ModelProvider`, but will follow the same pattern of complete
  file, optional hash and verifiable local path.
- `models/storage.rs` already concentrates storage operations and retention of
  sliced models. The provider will not duplicate that policy nor mix the
  Hugging Face cache with POM shard retention; a small `ModelStorage`
  abstraction will be ready for a future POM cache policy.

## Running external processes

- `utils/sysmon.rs` uses `spawn_blocking` with `std::process::Command` for
  short utilities and avoids installing Tokio's global process reaper.
- `inference/runtime.rs` uses `tokio::process::Command`, separate pipes and
  cancellation for long supervised processes.
- `backend/devupdate.rs` uses separate arguments, null `stdin` and separate
  output capture for administrative commands.

The provider will follow the `Command::new(...).args(...)` pattern, never a
shell or a concatenated string. For long downloads it will use the async
pattern of `inference/runtime.rs`, with separate stdout and stderr,
incremental progress observation, a cancellable process and a runner that can
be injected in tests.

## Events, jobs and progress

`backend/webui.rs` keeps jobs in `RwLock<HashMap<...>>`, updates progress
monotonically in `set_job_progress` and runs long work in Tokio tasks. The
provider will not duplicate that global storage because the requested API is
internal: it exposes `DownloadProgress` through an optional channel, so the
upper layer can wire the events into the existing job when the UI is
integrated. The callback does not block the main thread.

## Logging and telemetry

The runtime uses `tracing` directly (`info!`, `warn!`, `error!`) and there is
no metrics facade or separate exporter for downloads. The provider will emit
`tracing` events with `provider`, `repo_id`, `revision`, duration, bytes,
status and error kind. No stdout, stderr, token, header or private content
will be logged. Error messages will go through redaction of credential
patterns.

## Configuration and credentials

`utils/config.rs` is the source of the node's persistent configuration and
`brand.rs` derives product names and variables. This first version will not
add a Hugging Face token to `pom.json`: `hf auth login` itself manages the
credential. The provider only queries `hf auth whoami` and returns a
structured state.

## Error handling

The current code uses `anyhow::Result` for internal operations, but contract
validations and the provider boundary must be distinguishable by the UI. The
new module will have a typed `HfError` for missing CLI, missing
authentication, nonexistent repository, access denied, rate limit, invalid
response, download failure, cancellation and invalid input. The message
stored in each variant will be sanitized and will never carry raw process
output.

## Testability and compatibility

There is no equivalent provider in the project, so `OllamaClient` will not be
adapted: Ollama remains the HTTP integration used for profile and health
resolution. The external process runner will be an injectable trait, and every
test will use an in-memory mock. The implementation will use portable
filesystem and process APIs, without assuming Unix paths, and will never
execute a downloaded file.

## External reference checked

The current Hugging Face documentation confirms the commands
`hf models ls`, `hf models info`, `hf models card --metadata --format json`,
`hf auth whoami` and `hf download` with `--revision`, `--cache-dir`,
`--local-dir`, `--include` and `--exclude`:
[Hugging Face CLI](https://huggingface.co/docs/huggingface_hub/en/guides/cli).
The parser will stay isolated because progress output is still text, while
discovery operations will prefer JSON.

## Deliberate limits

- No HTTP route or new screen in this step, so `backend/webui.rs` is not
  rewritten during parallel work. The upper layer can inject the provider and
  consume its internal API without running `hf` directly.
- No P2P distribution of the model. The result produces a manifest that can
  feed the catalog and future deployment.
- No automatic CLI installation, and no option equivalent to
  `trust_remote_code` will be sent.
