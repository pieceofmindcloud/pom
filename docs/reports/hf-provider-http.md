# Hugging Face over native HTTP

## Decision

The operator decided not to redistribute the `hf` CLI. `HuggingFaceProvider`
now talks directly to the Hub API through `reqwest`, keeping the shape of
`ModelProvider`, the backend's internal contract and the JSON consumed by the
remote models screen.

The old path in `hf_cli.rs` was removed. After the switch there is no caller
for an alternative implementation, and keeping the subprocess would preserve
dead code, two authentication policies and the source of the
`cli_not_installed` error.

## Factual answer about the `hf` executable

The official `hf` is distributed as part of the Python package
`huggingface_hub`; no official standalone binary was found. The official
installer creates a virtual environment and installs the package. Freezing
that package with PyInstaller would remove the dependency on a Python
installed by the user, but would bundle the Python interpreter and the
dependencies into the executable, with a different artifact for Linux, macOS
and Windows. Sources checked:
[CLI documentation](https://huggingface.co/docs/huggingface_hub/en/guides/cli),
[official installer](https://github.com/huggingface/huggingface_hub/blob/main/utils/installers/install.sh)
and [published package](https://pypi.org/project/huggingface-hub/).

## Routes considered

| Route | Size and distribution | Effort and risk | License |
| --- | --- | --- | --- |
| Native Rust HTTP | Adds no Python, CLI or runtime to the package; uses Rust dependencies already present and works on all three platforms | Implements authentication, metadata, streaming, resume, cancellation, progress, cache and integrity in POM; the risk becomes maintaining the HTTP contract | Does not redistribute the Hub's Python packages; the licenses of POM's Rust dependencies still apply |
| Frozen CLI | The observed `huggingface_hub` 1.28.0 wheel was 793.2 kB, but the real executable would have to include interpreter, bootloader and dependencies per platform; the final measurement of the three executables was stopped by the decision | Less protocol code in POM, but a per-platform Python build, signing, packaging and release matrix | Would require notices and `THIRD_PARTY_LICENSES`; PyInstaller showed up as GPLv2 in the local inventory and needs a review of the bootloader exception before a closed product |

The chosen route is native HTTP. The screen was not changed.

## What became POM's responsibility

- Streaming: each file response is consumed through `bytes_stream` and written
  to disk; the whole file is never loaded into memory.
- Resume: POM issues a `HEAD`, stores size, `ETag` and `x-repo-commit`, and
  uses the metadata SHA-256 when available. A partial only continues if its
  identity matches the current identity. The request uses `Range` and
  `If-Range`; with no trustworthy identity the partial is discarded so a
  different file is never silently corrupted.
- Cancellation: a `CancellationToken` interrupts both the request wait and the
  streaming. The partial and its identity JSON are removed, and final files
  downloaded in the same operation are removed by the job.
- Progress: written bytes, known total, percentage and speed are computed
  locally, without parsing terminal text.
- Cache: POM uses its own layout. Partials live in
  `<destination>/.pom-hf/partials` or `<cache_dir>/partials`; a file only
  moves to the final destination after validation. This avoids depending on
  private details of the Python cache and keeps resume under POM's control.
- Integrity: the SHA-256 exposed by the API is compared after streaming, and
  the final manifest also records the SHA-256 computed by `pom::hash_file`.

## API, authentication and security

The provider uses `/api/models` for search, `/api/models/{repo}` for details,
`blobs=true` for file metadata, `whoami-v2` for authentication and
`/{repo}/resolve/{revision}/{filename}` for downloads. `repo_id`, revision,
file name, filters and directories go through the existing validations before
becoming a URL or a local path.

The credential sources, in order, are `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN`,
`HF_TOKEN_PATH`, `HF_HOME/token` and the default Hugging Face path under the
user's home directory. The token is only used in the Bearer header. It never
goes into logs, telemetry, responses or error messages. A missing token
produces an unauthenticated status; HTTP 401 without a token becomes
`authentication_required`, HTTP 401 with a token and HTTP 403 become
`access_denied`. A nonexistent repository and a nonexistent file also have
distinct errors.

The historical `cli.installed` field stays in the status for UI compatibility
and is `true` when the Rust provider is available. So the normal path cannot
produce `cli_not_installed`; a missing token shows up as an authentication
action, and network failures show up as an actionable transport error.

## Licenses of the discarded route

The local inventory of the Python environment observed: `huggingface_hub` and
`hf-xet` Apache-2.0; `click` BSD-3-Clause; `filelock` MIT; `fsspec`
BSD-3-Clause; `httpx` BSD; `httpcore` BSD-3-Clause; `PyYAML` MIT; `tqdm`
MPL-2.0/MIT; `typing_extensions` PSF-2.0; `certifi` MPL-2.0; `packaging`
Apache-2.0 or BSD-2-Clause; `idna` BSD-3-Clause; and `anyio` MIT. The inventory
also listed `pyinstaller` GPLv2 and `pyinstaller-hooks-contrib` Apache/GPL.
Those last two would not be runtime dependencies of the HTTP provider, but
they turn freezing for a closed product into a matter of legal review and
notices, not a simple copy of the wheel.

## Test coverage

The provider tests use a local HTTP server, never the real Hub, and cover
search, details, files, license, authentication without a token and
authenticated, access denied, missing file, traversal, resume with
`Range`/`If-Range` and cancellation with cleanup of the partial. The backend
remains the only owner of the public API and the UI does not need to know the
transport.
