# Reuse survey for the remote models UI

Date: 2026-08-24

This survey was done before implementing the model search, selection and
download UI. The new screen follows the existing POM flows and does not run
commands or downloads in the browser.

## Frontend

- Framework: React 18 + TypeScript, bundled by Vite and styled with Tailwind.
  The entry point is `apps/frontend/src/App.tsx`.
- Routes: `react-router-dom`; the new screen will be `/modelos/remotos`, with
  its entry registered in the `components/nav.ts` table and in the `App.tsx`
  router.
- Design system: `components/ui.tsx` provides `PageHeader`, `Panel`,
  `PanelHeader`, `Button`, `Input`, `Select`, `Field`, `Badge`, `Progress`,
  `Empty`, `Skeleton` and `ErrorState`. The screen will use these components
  and the `index.css` tokens, including the light and dark themes.
- Dialog: the `Modal`/`Confirm` pattern in `components/Confirm.tsx` will be
  reused for the download confirmation and for details that need modal focus.
  No second primitive will be created.
- Notifications: `ToastProvider`/`useToast` and `ApiError` handling are the
  feedback channel. No message will be built from the `hf` command line.
- State and calls: React Query in `lib/queries.ts`, `api.ts` as the only
  relative HTTP client, `types.ts` as the contract and a query hook for
  search/details. Download state stays in the backend; the component only
  derives filters and selection.
- Real time: the backend will reuse `webui::Job`; the screen will reuse
  `useJobs`, `requestConditional`/ETag and `jobsPollInterval` of 2 s while
  running and 10 s at rest. No WebSocket, new SSE, GGUF fetch or downloader in
  the browser.
- Internationalization: every new string goes into the `en` and `pt-BR` blocks
  of `i18n/locales.ts` and is read through `useI18n().t`.
- Responsiveness: the screen follows the scrollable column of the existing
  pages; filters and details use the current flexible layout and stay usable
  on desktop, tablet and mobile.

## Backend

- API: a new backend module registers generic `model-provider` and download
  routes, merged into the gateway with a minimal change. The UI only knows
  model and download DTOs, not command names.
- Abstraction: handlers receive `GatewayState` and use the `DynModelProvider`
  already defined in `models/provider.rs`; the default implementation is
  `HuggingFaceProvider`. The parser and argument building stay in
  `models/hf_cli.rs`/`huggingface.rs`.
- Jobs: each download is a `webui::Job` kept in the node's memory, with a
  specific status (`queued`, `starting`, `downloading`, `verifying`,
  `completed`, `cancelled`, `failed`) and optional bytes, speed, ETA and error
  fields. This keeps the backend's state storage and the adaptive cadence
  already used by the bell and the slicing panels.
- Cancellation and queue: each job gets a `CancellationToken`; a semaphore
  limits concurrent execution to two tasks and the others stay `queued`.
  Cancelling a token only ends that process.
- Security: `ConnectionAuth` protects search, details and mutations; the
  handlers validate identifier, revision and file through the provider
  contract. The UI never sends a Hugging Face token and never receives raw
  process output.
- Space and catalog: the backend revalidates size and space before starting,
  picks the first configured model folder, writes only through the provider,
  invalidates the catalog cache on completion and lets the local catalog
  discover the GGUF. No downloaded file is executed.

## Boundary decisions

The download is started by a structured JSON request with `repo_id`,
`revision`, `filename` and an optional `quantization`. The destination is
decided in the backend from POM's folder configuration, so the browser cannot
choose arbitrary paths. Quantization is only presentation and deduplication
metadata; the provider is the source of truth for the file.
