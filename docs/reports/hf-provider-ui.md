# Delivery report: remote models UI

Date: 2026-08-24
Branch: `fm/rdma-hf-warnings`
Specification: kept outside this repository

## 1. Implemented architecture

The UI talks to the agnostic `model_provider_backend` layer, which receives a
`DynModelProvider` in `GatewayState`. The default provider is still
`HuggingFaceProvider`; no component knows about `hf`, shell, stdout, stderr or
credentials.

The backend exposes DTOs for CLI status, search, details, license, storage and
downloads. Each download is a `webui::Job` with its own status, cancellation
token and a limit of two concurrent processes. The frontend builds the screen
from the existing primitives, React Query, the `api.ts` client, the
conditional jobs endpoint with ETag and the adaptive polling POM already uses.

`RemoteModels` queries the authenticated status before searching. Missing CLI,
authentication required and authenticated session show up as localized
states; when the operator opens a detail, the license is queried through the
provider and the `gated` marker stays visible before the download
confirmation.

## 2. Files created

- `crates/node/src/backend/model_provider.rs`
- `apps/frontend/src/routes/RemoteModels.tsx`
- `apps/frontend/src/components/RemoteModelCard.tsx`
- `apps/frontend/src/components/RemoteModelDetails.tsx`
- `apps/frontend/src/components/DownloadModelDialog.tsx`
- `apps/frontend/src/components/ModelDownloadsPanel.tsx`
- `apps/frontend/src/lib/modelProvider.ts`
- `apps/frontend/src/lib/modelProvider.test.ts`
- `apps/frontend/src/lib/modelProvider.api.test.ts`
- `apps/frontend/src/components/RemoteModelCard.test.tsx`
- `apps/frontend/src/components/ModelDownloadsPanel.test.tsx`
- `docs/reports/hf-provider-ui-reuse.md`
- `docs/reports/hf-provider-ui.md`
- `docs/superpowers/plans/2026-08-24-rdma-hf-provider-ui.md`

## 3. Files changed

- Backend: `crates/node/src/main.rs`, `backend/gateway.rs`,
  `backend/webui.rs`, `models/provider.rs` and `models/hf_cli.rs`.
- Frontend: `App.tsx`, `components/nav.ts`, `components/nav.test.ts`,
  `lib/api.ts`, `lib/queries.ts`, `lib/types.ts` and `i18n/locales.ts`.
- Maintenance docs: `apps/frontend/AGENTS.md`,
  `apps/frontend/src/components/AGENTS.md`, `src/routes/AGENTS.md`,
  `src/lib/AGENTS.md`, `src/i18n/AGENTS.md`,
  `crates/node/src/backend/AGENTS.md` and `docs/AGENTS.md`.

## 4. Components

`RemoteModels` offers search with a 400 ms debounce, immediate Enter, a GGUF
filter, sorting by relevance/downloads/update, pagination kept in the URL and
comparable cards. Each card shows repo, author, description, tags, downloads,
likes, update, GGUF, gated/private and the downloaded or downloading states
when the backend knows them.

`RemoteModelDetails` loads files on demand in a `Modal`, highlights GGUF and
exposes structured quantization, name, size, revision, license, tags and local
state. `DownloadModelDialog` confirms the operation, shows the logical
destination and known space, and blocks when space is missing or the
destination is unavailable.

`ModelDownloadsPanel` reuses the backend `Job` to show queue, status,
percentage only when there is a trustworthy total, bytes, speed, ETA,
cancellation, retry and safe messages.

## 5. Endpoints and internal commands

Routes authenticated with `ConnectionAuth`:

- `GET /api/ui/model-provider/status`
- `GET /api/ui/model-provider/search`
- `GET /api/ui/model-provider/details`
- `GET /api/ui/model-provider/storage`
- `POST /api/ui/model-provider/downloads`
- `POST /api/ui/model-provider/downloads/:id/cancel`
- `POST /api/ui/model-provider/downloads/:id/retry`

The service only calls `ModelProvider` methods: `check_authentication`,
`search_models`, `get_model_info`, `get_license`, `list_files` and
`download_file`. The Hugging Face implementation builds separate arguments and
runs the external process through the existing runner. The UI sends structured
JSON, never a command line.

## 6. Start flow

On confirmation, the backend validates `repo_id`, `revision`, `filename` and
`quantization`, checks for duplicates, rereads the remote file list,
revalidates free space, picks the first configured model folder, creates the
folder and registers a single job. The response only contains `download_id`.
The task starts as `queued` and waits for the two-process semaphore.

## 7. Progress

The provider sends `DownloadProgress`; the backend maps it to `queued`,
`starting`, `downloading`, `verifying`, `completed`, `cancelled` or `failed`
in the same `webui::Job` the rest of the application reads. The frontend reuses
`GET /api/ui/jobs`, ETag and the existing cadence of 2 s with active work and
10 s at rest. There is no WebSocket, new SSE or downloader in the browser.

## 8. Cancellation

Each job keeps its own `CancellationToken`. Cancelling checks that the job is
still active, cancels only that token and the provider ends the matching
process. The semaphore releases the next queued task. A terminal cancellation
is preserved against a later anonymous update.

## 9. Persistence

Download state lives in the node's job map, not in the React component. The
jobs endpoint survives refreshes and navigation while the process runs;
terminal jobs stay for the retention period the webui already uses. The
deduplication identity is `repo_id`, revision and filename. POM does not
include `local_path` in job JSON, so the local path is never exposed to the
browser.

## 10. Local catalog

On success, the backend writes a side record in `.pom-hf/` with source, repo,
revision, filename, quantization, size, SHA-256, timestamp and the files of the
`ModelManifest` returned by the provider. It then invalidates the local
catalog. Search reads these records to mark the repo as downloaded, and details
check the physical file plus the side record before marking a quantization as
installed. No model is loaded on the GPU or executed.

## 11. Tests added

- Rust: the structured identity rejects traversal and non-GGUF extensions,
  messages do not leak token/path, remote jobs are not confused with slice
  jobs and the page size is bounded. The existing parser also got coverage for
  description and update.
- Frontend: URL serialization and `AbortSignal`, structured
  start/cancel/retry endpoints, job filtering, polling, percentage without a
  total, result cards, GGUF metadata, the progress/cancel/retry panel,
  localized provider status and details with license/gated before download.
- `rtk npm test`: 50 files and 291 tests passed.
- `rtk npm run build`: TypeScript and Vite passed; Vite keeps the informative
  warning about a chunk above 500 kB.
- `rtk cargo build -p node`: passed without the Hugging Face provider
  warnings.
- `rtk cargo test -p node`: 752 passed, 5 ignored.
- `rtk cargo fmt --check` and `rtk git diff --check`: passed.
- `rtk cargo clippy -p node`: exit code zero; pre-existing warnings remain in
  other subsystems, none in the changed files.

## 12. Known limitations

- The screen only offers filters the current endpoint supports safely: GGUF,
  plus sorting by relevance, downloads and update. Filters by size, license or
  quantization need per-result file metadata and are left for a contract
  extension.
- The existing local catalog is still the inventory of GGUF files; the side
  record is the provider's source identity and neither distributes weights nor
  changes the slicing flow automatically.
- Byte resume is still delegated to the Hugging Face cache/provider. POM does
  not implement a second resume mechanism. (Superseded by
  `hf-provider-http.md`, where POM took over resume.)

## 13. Next steps

1. Use the side `ModelManifest` in the slicing/deployment flow when the remote
   source integration needs to tell files apart by source.
2. If needed, evolve the search contract to remote filters by size, license
   and quantization without N hidden calls per result.
3. Add response fixtures for future CLI versions and an alternative HTTP
   implementation of the same `ModelProvider`.
