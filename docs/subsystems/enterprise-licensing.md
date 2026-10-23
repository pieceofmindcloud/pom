# Subsystem: plugin licensing

This document records what the client delivers in this round and separates
current guarantees from the remaining limits. The full specification is a
design report kept outside this repository.

**Product pivot (2026-09-18):** POM no longer has a single product
"Enterprise license"; it has INDEPENDENT PLUGINS - any menu feature can be its
own plugin, bought/activated/deactivated separately. The binary keeps no static
catalog of installable plugins: `PluginManager`/`PluginRuntime` in
`crates/node/src/plugins.rs` creates runtimes for every `plugin_code` granted by
the server and restores the codes persisted locally. Each plugin has its own
activation state, lease, heartbeat, process host and version update check, with
nothing shared between plugins. Every persisted install is bound to the machine
fingerprint and dropped at startup when it no longer matches. Its machine
component is `/etc/machine-id` on Linux and, where there is none (a container),
a random id stored once in `.<slug>-machine-id` under the project root: the
container's `HOSTNAME` used before changed on every recreate, and the node came
back with its plugins gone. The Request Logs/Statistics plugin's
`plugin_code` was `"enterprise"` until 2026-10-04 and is now `"logs"`
(`LOGS_PLUGIN_CODE`); "POM Enterprise" is the product/plan name, and this
file and the `enterprise-protocol` crate kept the name. What
changed is that the GENERAL CODE stopped assuming "a single licensable
product". See `## Phase 7` below for what this change touched and what was
left out on purpose (the `pom_core` update itself, which was a separate product
boundary before and still is).

## Delivered boundaries

- `crates/enterprise-protocol` holds the versioned DTOs of the license contract
  (`pom-license/v1`), the entitlement state, the versioned fingerprint and the
  activation and heartbeat envelopes.
- `crates/node/src/plugins.rs` holds the generic manager
  (`PluginManager`/`PluginRuntime`, created dynamically per `plugin_code`) and
  the mockable HTTPS client. The runtime uses the real endpoint as the default
  configuration, but tests use `MockLicenseService` (one response per
  `plugin_code`, to prove two independent plugins); the license key only goes
  through validation/activation and is not part of the local activation
  snapshot (`plugin_license:<code>`). The client queries `/v1/keys`, verifies
  the Ed25519 grant and manifest, plus hash, size, ABI and platform. A key
  activation first calls `/v1/licenses/validate` once to learn which
  `plugin_code`s it grants (`licensed_plugins`, a list of plain codes), then
  activates each one separately through `/v1/licenses/activations` with
  `plugin_code` in the payload. A plugin's features come from the release
  manifest's `feature_set`, not from the license. The heartbeat deserializes
  the rich renewal payload and persists the new token, expiry, next heartbeat
  deadline and grant reported by the server, without replacing the deadline
  with a local estimate - all of it per `PluginRuntime`, never shared between
  plugins. `PersistedPluginState` separates two releases the heartbeat must not
  confuse: `installed_release_id` is the release actually downloaded and
  installed on this peer, and only a completed
  `activate`/`activate_and_install`/`apply_update` may advance it; the
  heartbeat alone never downloads or verifies anything, so it never writes to
  it. `plugin_release_id`, `release_changed` and `available_release_id` from
  the heartbeat payload are still consumed, but only to feed
  `status.available_update` - triggering `detect_update` (which queries the
  manifest again to bring the real version and changelog) when
  `release_changed` is true, including in the periodic heartbeat supervisor,
  which previously never called `detect_update` on its own. Each
  `PluginRuntime` also checks its OWN version update
  (`detect_update`/`apply_update`), reusing the generic manifest of its
  `plugin_code` - independent of any other plugin and of `pom_core` (see
  `## Authorized binary detection and update` further down, which is about
  `pom_core`, not this).

  `PersistedPluginState.installed_at` marks when the host became ready after
  the initial install or after applying a new release. `update_checked_at`
  marks the last successful manifest query, even when there is no update;
  failures keep the previous good query. Both show up in `PluginStatus` and are
  optional fields with a default when reading already persisted states.
  `last_success_at` stays reserved for the heartbeat and never serves as an
  install date.

  `PluginStatus.state` represents license validity; `active` is only true when
  the host finished the handshake and is ready. If the license is accepted but
  manifest, download or handshake fails, `active` stays false and
  `installation_error` keeps the actionable detail in the local state, even
  after a restart. The screen keeps the license separate from the install and
  shows the error; a successful heartbeat does not mark as ready a host that
  did not come up.

  `PluginHost::start` receives `POM_PLUGIN_DB` with the path derived from POM's
  database directory. For `plugin_code=logs`, the file is `logs.db`, so
  the existing persistent volume `/data` gets `/data/logs.db`. Other codes keep
  `<plugin_code>.db`; POM does not pass `enterprise.db`, and there is no
  `POM_ENTERPRISE_DB` nor any additional volume.

  `PluginStatus.license_expires_at` is the LICENSE validity, distinct from the
  technical `lease_expires_at` above: the lease is a short renewal deadline the
  heartbeat pushes every few days, the license is what the purchase actually
  grants (months or years). It comes from
  `LicenseStatusResponse::license_expires_at` (`GET /v1/licenses/status`, the
  same endpoint `restore()` already queried at startup), also queried again on
  the manual "Refresh" click (`PluginRuntime::refresh_license_status`, inside
  `PluginManager::refresh`) - never in the periodic heartbeat, which does not
  carry that field. A failure of that extra query is only logged, without
  failing the `refresh`. The screen used to show `lease_expires_at` under the
  "License valid until" label, which confused a technical expiry of days with
  the real license validity - fixed by reusing the same field `restore()`
  already fetched and discarded.

  **Fixed defect (2026-09-21):** until this fix, the heartbeat wrote the
  `plugin_release_id` received from the server straight into
  `installed_release_id` - the same field `detect_update` uses as the baseline
  to compare against the manifest's `release_id`. A heartbeat whose server
  already reported the next release (for example by treating `0.1.0+build.1`
  and `0.1.0+build.2` as the same version for entitlement purposes) adopted
  that release as "installed" without any real download; the "Refresh" click
  calls heartbeat and then `detect_update` in the same action
  (`PluginManager::refresh`), so the next comparison saw the already tampered
  baseline and always concluded "no update" - unrelated to the semver format of
  the release_id, which the string equality comparison already handled
  correctly. The regressions are
  `refresh_button_does_not_lose_an_update_signalled_by_heartbeat` and
  `a_background_heartbeat_alone_also_surfaces_the_update`
  (`crates/node/src/plugins.rs`).
- `crates/plugin-host` is the `pom-plugin-host` process. It loads an artifact
  only by absolute path, validates ABI, size and SHA-256, keeps the library
  loaded inside the helper process and talks to the client through
  length-prefixed JSON on stdin/stdout. The `plugin_code` received from the
  manifest resolves the `pom_<plugin_code>_plugin_v1` entry point; there is no
  fixed symbol for a product. `pom` does not use `libloading`.
- `crates/enterprise-plugin-fixture` is a test `cdylib` library that validates
  the `pom_logs_plugin_v1` and `pom_base_plugin_v1` ABI entry points, as
  well as the host flow.
- The production `cdylib` of the `enterprise` plugin (the audit domain in
  `enterprise.db`, with its own schema, indexes, filtered listing and
  statistics aggregations) **no longer lives in this repository**: it was
  extracted to `https://github.com/pieceofmindcloud/pom-plugin-logs` (see
  `## Extraction to pom-plugin-logs` below). This repository only keeps the
  generic licensing/host boundary, reusable by any future `plugin_code`.
- `crates/node/src/plugin_host.rs` (`PluginHost`, renamed from
  `EnterpriseHost`) supervises the helper process through async IPC with
  bounded frames, timeouts and orderly shutdown. A persistent reader correlates
  each answer by ID, even when concurrent requests come back out of order.
  After a timeout, the pending record is removed, but the reader consumes a
  late answer by ID without handing it to the next call. One instance per
  `start()` call - nothing is global - so `PluginManager` keeps one concurrent
  instance per active plugin without them stepping on each other. The active
  path sends neutral events to the host; the legacy `pom.db` is a transitional
  fallback for tests, migration and plugin failure.
- How POM starts the host (`plugin_host.rs`): the host's stderr, and that of
  the processes a plugin starts (node, npm, code-server), is read line by line
  into POM's log with the `plugin_code`. On Windows, where `pom.exe` has no
  console, the host starts with `CREATE_NO_WINDOW` (no console window, for it
  or for the processes it starts), and goes into its own kill-on-close Job
  Object (`process_tree.rs`): stopping the host, or POM exiting, ends the host
  and everything it started. Inherited pipes in those children used to keep
  the host from ever seeing its input close.
- The Docker image copies `pom-plugin-host` to `/usr/local/bin` next to `pom`.
  The entrypoint validates both, and `pom` only starts the host after
  activation, with the absolute path of the downloaded plugin and stdin/stdout
  pipes; `docker-compose.yml` only needs to persist `/data`, with no extra IPC
  socket.

The C ABI v1 is deliberately small and explicit: an immutable descriptor with
version, capabilities and the `create`, `ingest`, `query`, `free_buffer` and
`shutdown` callbacks, all with `repr(C)` types. The host validates every
callback before creating the handle. The local stdin/stdout IPC rejects empty
frames or frames larger than 16 MiB, using
`enterprise_protocol::MAX_IPC_FRAME_BYTES` in both processes. That limit covers
the plugin's queries and events, including serialized telemetry, without
changing the limits of the inference transport between peers.

## Extraction to pom-plugin-logs (2026-09-21)

The product code of the `enterprise` plugin (the `cdylib` that writes and
queries `enterprise.db`) left this repository for
`https://github.com/pieceofmindcloud/pom-plugin-logs`, a licensing repository of its
own with separate build, packaging and release pipeline. What **stays** here is
only the generic boundary, reusable by any future `plugin_code`:

- `crates/enterprise-protocol` (DTOs, C ABI v1, `PLUGIN_ABI_VERSION`,
  `MAX_IPC_FRAME_BYTES`) is still the single source of the contract. The new
  repository depends on it through git (`enterprise-protocol = { git =
  "https://github.com/pieceofmindcloud/pom", rev = "<commit>" }`), pinned to a
  commit, never a floating branch - Cargo resolves the package inside the full
  workspace clone without trying to build the other members (`node`,
  `llama-engine` etc.), because no dependency of `enterprise-protocol` uses
  `.workspace = true` inheritance beyond `version`/`edition`.
- `crates/enterprise-plugin-fixture` and `crates/plugin-host` stay here because
  they test/load the generic ABI/IPC mechanism, not the audit domain of a
  specific plugin.
- `crates/node/src/plugins.rs` and `plugin_host.rs` pass the `plugin_code` and
  the artifact path to the host. `pom` keeps treating the plugin as an opaque
  `cdylib` downloaded by manifest - where its source code lives is irrelevant to
  the runtime.
- The audit transport between admins stays in POM (`reqlog.rs` and
  `network/discovery.rs`), which replicates the `Begin` and `Complete` DTOs even
  when the local host accepted the event. Each receiver forwards the ordered
  copy to the active host; the plugin still owns persistence and queries in
  `logs.db`. There is no P2P transport inside the plugin, and no new
  artifact needs to be published to restore the fanout.
- `plugin_code` is `"logs"` and its `feature_set` is
  (`logs.request_logs`, `logs.statistics`); the wire contract with the
  license server did not change, only the code value.
- `pom-release.sh` no longer releases anything with that plugin_code: it now
  only releases `pom_core`. The release of the `logs` plugin (build,
  reproducible packaging for macOS/Linux/Windows, GitHub Actions and publishing
  the version already built on GitHub to the license server) lives entirely in
  `pom-plugin-logs` - see that repository's `AGENTS.md`.
- In the frontend, POM **no longer** contains the Request Logs and Statistics
  screens: they live in `pom-plugin-logs` (`ui/`) and arrive through the
  generic plugin UI mechanism (manifest, screens and i18n installed together
  with the backend) - see `plugins-ui.md`. POM only keeps the sales fallback
  (`EnterpriseFeatureShowcase`) for `/logs` and `/statistics` when the plugin
  is not active: a commercial page with the data, screenshots and price from
  the public catalog and the buy button, which opens Stripe checkout (see
  "Checkout through the node" below). An earlier version (97bb0e91) showed only
  the showcase even with the plugin active ("This plugin is already active in
  this cluster") - defect fixed. The `/api/ui/requests` and
  `/api/ui/requests/stats` routes stay in the node, `LocalOnly`, and refuse
  the data without entitlement. An admin of the chain reads every node's
  requests; any other node, an inference node included, only its own
  (`http-api.md`).

## Checkout through the node

`POST /api/ui/plugins/checkout` (every profile, `plugins::start_plugin_checkout`)
receives `{plugin_code, interval: "month"|"year", locale: "pt-BR"|"en"}`. The
node reads `GET /v1/checkout/plans` from the license server, picks the plan
that sells that `plugin_code` and creates the session at
`POST /v1/checkout/sessions`; the server picks the Stripe `price_id` by plugin
and interval and sets the success and cancel pages on the website. The `{url}`
response is only accepted when it points to `https://checkout.stripe.com` (or
`*.stripe.com`) or to the license server itself in simulated mode
(`checkout_url_allowed`). The node makes the call because the public endpoint
only allows CORS for the website's domains, and the UI runs on each node's
origin. Errors come back as `checkout_request_invalid` (400),
`checkout_not_offered` (409) or `checkout_unavailable`/`checkout_url_rejected`
(502). Tests: `plugins::plugin_checkout_tests`.

## Entitlement and UI

`GET /api/ui/plugins` publishes the local states of the plugins restored or
seen by the runtime. The public catalog `GET /v1/plugins` is read by the node
for the browser (`GET /api/ui/plugin-catalog[/:code]`, so the page never
leaves its own origin and the store's CORS policy does not matter) and includes plugins with no local state yet; the Plugins screen
asks it for one page at a time (`q`, `page`, `per_page`, answered with `total`). Each local state has
its own entitlement, `manage_url` and `available_update`. The answer starts at
`needs_activation`. The `state` field follows the license, while `active`
requires the host installed and ready. A failure after the license was
accepted keeps `state=active`, `active=false` and the reason in
`installation_error`; the failure does not show up as a ready plugin. A
transient heartbeat failure enters `degraded` during the grace window; a
revoked or expired license turns the features off - of ONE plugin, never of
the others. `POST /api/ui/plugins/activate` only receives `{license_key}` (no
`plugin_code`: the `{activated: [...]}` answer names which plugins that key
granted and installed). `POST /api/ui/plugins/:code/refresh|deactivate` and
`POST /api/ui/plugins/:code/update/check|apply` act on a single plugin.
Deactivating an activation the license server answers `ACTIVATION_UNKNOWN` for
(a server moved to a new database, or an activation already deleted there)
removes it locally: there is no slot left to free on the server. Any other
error keeps the license.

Plugins are local to each node, so activating a license, registering a GitHub
repository, installing, updating and using a plugin's UI is allowed to the
`admin`, `worker` and `visitor` profiles on their own machine, including the
development rebuild (`/api/ui/plugins/dev-sources*`), which also requires
`POM_DEV_MODE=1`. The `visitor` UI lists Plugins and the installed plugins' menus
below its conversations. The rule
lives in `ROUTE_RULES` (`backend/webui/routes/mod.rs`).

`GET /api/ui/plugins` is open to every profile. Its `PluginStatus` carries
`licensed_plugins`, `activation_candidates` with an eligible release,
`license_expires_at` and `plugin_usage` per `plugin_code`; the counters in the
scalar status belong to that entry's plugin, not to the license totals. The
screen uses this authenticated data to tell apart an active, missing, expired
or revoked license and to show activation usage. The public catalog publishes
neither entitlement nor license usage. Buying leads to the public store with
the plugin code in the query. The activation form shows up if any public
plugin that needs a license is not active in the local state, even when another
one is already active or there is license context. Missing local status,
expiry or revocation keeps the form visible; it disappears when every known
licensable plugin is active.

The authenticated routes `/api/ui/requests` and `/api/ui/requests/stats` fail
closed with `403` and `code: enterprise_required` when the `enterprise` plugin
does not have the matching feature active. `/api/ui/cluster-logs` stays a Free
operational log.

The `Request Logs` and `Statistics` entries stay in the menu for every
profile, with or without a license. A direct access or a click without
entitlement shows the Enterprise invitation and the link
`https://pieceofmind.cloud/enterprise` (through `useEnterpriseStatus()`, a
selector over `usePlugins()` for the `"logs"` `plugin_code`). The menu is
product discovery, not proof of authorization: the backend is still the
security boundary.

The **Plugins** screen (`/plugins`, `apps/frontend/src/routes/Plugins.tsx`) is
the only one that activates, updates and deactivates plugins - it replaced the
old single-product `Enterprise.tsx` screen. Its "Check for updates" dialog also
lists a newer POM, but installs it only through
`POST /api/ui/pom-update/check|apply`, the same routes the **About** screen
(`/config/about`) uses - see `## POM updates` below. The plugin routes and the
POM routes never mix.

## Public GitHub repositories and local preferences

The **Add from GitHub** card, at the top of the Plugins screen and above
license activation, registers URLs directly on the node. The flow is local to
POM and does not query the catalog, eligibility or any license server endpoint.
There is no `requires_license` field in the GitHub manifest, and the value the
commercial catalog publishes does not control installing from a repository.
The operator enters a canonical public repository
`https://github.com/<owner>/<repository>`. The generic format uses
`manifest.json` at the root of the default branch. For plugins with dynamic POM
UI, POM also accepts `ui/manifest.json` in the `pom-plugin-ui/v1` contract; in
that format, each release must attach `pom-plugin-<os>-<arch>.json` with the
integrity data of that platform's binary.

`manifest.json` contract (`schema: 1`):

```json
{
  "schema": 1,
  "plugin_code": "acme_plugin",
  "display_name": "Acme Plugin",
  "description": "Example plugin",
  "plugin_abi": 1,
  "feature_set": ["acme_plugin.core"],
  "artifacts": {
    "linux-x86_64": "acme-plugin.so",
    "macos-aarch64": "acme-plugin.dylib"
  },
  "preferences": null
}
```

`artifacts` maps `<os>-<arch>` to the exact name of the binary attached to each
release. In `ui/manifest.json`, the `plugin_code`, name and screens come from
the UI manifest, while ABI, features, preferences and the binary name come from
the per-platform manifest attached to the selected release. `feature_set`
belongs to the `plugin_code` and can be omitted if the plugin announces no
features; `preferences` is also optional and uses the typed schema documented
below. POM validates code, ABI, platform, extension, features, preferences and
manifest size limits before showing releases. The public GitHub API provides
the published releases and assets. The node walks pages of 100 until it lists
every published release, including prereleases and excluding drafts. Reading
is capped at 16 MiB of metadata and fails explicitly if it goes over that
budget, instead of hiding older releases. Each installable asset must publish
the API's SHA-256 digest. On screen, the plugin card offers a select with only
the releases compatible with this node (`available_for_node`), preselected on
the latest, and the Install or Update action. Picking an older version or
downgrading is allowed and always requires explicit confirmation. The manifest
can declare a `description` per language and `screenshots`; the node resolves
relative paths to `https://raw.githubusercontent.com/<owner>/<repo>/HEAD/<path>`
and the browser loads the images straight from GitHub, without going through
the node or the license server.

GitHub allows 60 API calls per hour per IP without a token, so the node keeps
the calls few (`plugin_github.rs`):
- Adding or refreshing a repository inspects it (repository, manifest,
  release pages). Installing a release reuses that saved inspection and reads
  only the selected release, fresh.
- Release assets (manifests and binaries) download from their
  `browser_download_url`, on GitHub's CDN, which does not count against the
  API limit; the bytes are still checked against the API's SHA-256 digest.
- Every API GET carries the last ETag for its URL; an unchanged answer comes
  back as 304, which GitHub does not count, and the node reuses the cached
  body.
- `POM_GITHUB_TOKEN` (optional) authenticates the API calls, sent only to the
  API host, raising the limit to 5,000 per hour for that token. An exhausted
  limit is reported with its reset time and this hint.
There is no periodic polling: GitHub is read only on add, refresh and install.

Every stable release of a POM plugin is also published to the license server
by the plugin repository's workflow, and free plugins (`requires_license =
false`) are served there publicly (`GET /v1/plugin-releases`,
`/v1/plugin-releases/{release_id}/download`). That is the fallback when GitHub
refuses or rate-limits:
- Adding a repository whose GitHub lookup fails lists it from the license
  server (`inspect_from_license_server`): `manifest_path = "license-server"`,
  release IDs prefixed `license-server:`, no preferences or screenshots, and
  a `check_error` saying where the releases came from. Refreshing such a
  repository tries GitHub first and falls back again; a repository read from
  GitHub keeps its saved inspection when GitHub fails, as before.
- Installing a `license-server:` release downloads it from there. Installing
  a GitHub release whose download fails takes the same version from the
  license server. Either way the listing is read again and the bytes are
  checked against its SHA-256 and size.
The base is the license server POM already uses (`license_base_url`).

Adding, updating and removing repositories only uses the local routes
`/api/ui/plugins/github-repositories*`, open to every profile. The list, the
manifest path and the details read are persisted in
`plugin_github_repositories` in `pom.db`; removing a repository takes the
source off the list but does not uninstall the already installed plugin.
Installing downloads the chosen release, checks the asset's digest and size,
writes the binary atomically and starts the host. No automatic check replaces
the choice or the installed version. The public runtime persists in
`plugin_public_install:<code>`, separate from `plugin_license:<code>`, with no
lease, grant or entitlement. A local conflict with the same `plugin_code`
already installed by the license mechanism is refused so that host is not
silently replaced; no license server query is made to decide whether the
repository needs a license.

The local development build/install is open to every profile on its own
machine and requires `POM_DEV_MODE=1`: the source is picked from an allowlist mounted under
`/opt/pom/dev-plugins`. It can install a development-only plugin with no prior
license or GitHub installation, persisting it separately in
`plugin_dev_install:<code>`; this starts the host but grants no licensed
features or entitlement. If a licensed or GitHub installation already exists,
`PluginManager` replaces its binary and restarts only that plugin's host while
preserving the existing entitlement. The operation does not activate a
license, query GitHub/the license server or restart POM. The local Compose
provides the `dev-runtime` target; the release `runtime` image does not include
the toolchain. The contract and limits are in `docs/subsystems/plugins-ui.md`,
"Local rebuild in development".

`PluginManifest.preferences` is optional and omitted on serialization when
absent, which keeps old manifests and the signed canonicalization. The current
versionless schema lists `features[]` toggles (`key`, `label`, `default`,
`description?`) and a `storage_directory?` (`label`, `default?`). POM validates
unique identifiers and size limits; the typed values are a separate DTO, with a
bool map and an optional path. Defaults are applied when reading the state. The
chosen directory must be absolute; if it does not exist yet, the node creates
it on save, and the final path must be a directory. The values live in
`plugin_preferences:<code>` in `pom.db`, never in the `plugin_license:<code>`
snapshot, in requests to the license server or in entitlement. The editor opens
from the gear icon on the card, only when the installed release has a schema
and the plugin is active, and shows the reason returned by the node when saving
fails. `host.configure` delivers those values to the plugin's own local host
and, on every save, the node also sends `host.event` with
`preferences.changed`; no license endpoint ever receives preference values.

## Phases and limits of this delivery

- **Phase 0 - contract:** delivered in `crates/enterprise-protocol`, including
  envelope, fingerprint, states, lease, manifest, grant, download and status.
- **Phase 1 - license without plugin:** delivered with `validate`, idempotent
  activation, heartbeat, status after restart, deactivation and keyless
  persistence. Supervision runs outside the hot path and keeps Free inference
  available.
- **Phase 2 - host and ABI:** delivered with `pom-plugin-host`, C ABI v1,
  framed IPC, fixture and real library. The library is never loaded by the
  `pom` process.
- **Phase 3 - audit extraction:** delivered on the active Enterprise path. The
  plugin writes and queries `enterprise.db`, keeping the current DTOs.
  `reqlog.rs` and the old schema stay as a migration and unavailability
  fallback; removing them for good requires an explicit import.
- **Phase 4 - Enterprise P2P:** delivered the fingerprint proof contract,
  nonce signing and the authenticated internal route
  `/api/internal/enterprise/fingerprint`. Still missing: wiring the collection
  to the admin `activation-batches` flow and transferring grants/artifacts
  between peers.
- **Phase 5 - frontend:** delivered `/enterprise`, activation, refresh and the
  active state. `/logs` and `/statistics` stay visible and clickable without
  a license, showing the Enterprise invitation instead of empty data.
- **Phase 6 - release/hardening:** delivered `build-enterprise-plugin.sh`,
  `pom-release.sh`, atomic install and Ed25519/hash/ABI verification.
  `pom-release.sh` queries `/internal/releases?plugin=<plugin>&status=all` with
  the `POM_RELEASE_TOKEN` token before uploading, considers releases of every
  platform and picks the next `+build.N`; `--version` is still a manual
  override. The POM binary catalog uses exactly `pom_core`, as defined in
  `license-server/AGENTS.md`; the node no longer reads it (it updates from
  GitHub releases, see `## POM updates`). As defined in
  `license-server/AGENTS.md`. For `pom_core`, the script builds the `node`
  package with the default features `cuda,vulkan` on Linux, `metal` on macOS
  and none on Windows CPU; `--features` replaces that set and `--feature-set`
  only changes the upload metadata. The artifact is `pom` on Linux/macOS and
  `pom.exe` on Windows. The server accepts `plugin_abi=1` as a generic required
  field for `pom_core`, not representing a cdylib ABI, and `feature_set=core`
  is still only metadata, with no entitlement. `--dry-run` stays offline and
  uses `+build.1` as a preview when there is no override. Still missing: native
  code signing/notarization, pointer-based rollback, a full circuit breaker and
  a real three-platform matrix.
- **Phase 7 - N independent plugins (2026-09-18):** delivered the product
  pivot. `crates/node/src/enterprise.rs` (a global `EnterpriseState`) became
  `crates/node/src/plugins.rs` (`PluginManager` with one `PluginRuntime` per
  `plugin_code`; the compiled list of the first implementation was later
  removed to accept every code granted by the server); `enterprise_host.rs`
  became `plugin_host.rs` (`PluginHost`, which already supported several
  concurrent instances - confirmed, no change needed). The UI swapped the
  `Enterprise.tsx` screen for `Plugins.tsx` (`/plugins`), which reads the
  public showcase and activates/updates/deactivates per `plugin_code`. The
  `pom_core` update detection/application left `EnterpriseState` and got its
  own file, `pom_core_update.rs` (`PomCoreUpdater`), with its own full screen
  at `/config/about` - never again sharing code or UI with the Plugins screen.
  **Contract assumption**, now confirmed against the real server:
  `ActivationPayload`/`ActivationResponse`/`GrantBody` carry `plugin_code`, and
  activation is one call per `plugin_code` (the client learns which ones to
  call by validating once and reading `licensed_plugins`).

The license server evolves in parallel, and the mock alone is not enough to
keep up with it: the mock agreed with this client's wrong assumption, so the
tests stayed green while every activation broke in production with
`missing field \`entitlements\``. That is why the contract is also asserted on
REAL response bodies, captured from the license server, in
`crates/enterprise-protocol/tests/wire_contract.rs`; the same contract is
asserted on the other side in `license-server/test/wire-contract.test.ts`
(pom-website repository), which rewrites those fixtures with
`POM_WIRE_FIXTURES=<dir>`.

## POM updates

This section is about POM itself, NEVER about a plugin - see
`crate::pom_core_update`, deliberately separate from `crate::plugins`. It needs
no license: the `pom_core` manifest on the license server and the borrowed
`logs` lease are gone. Releases are read, in this order, from:

1. the GitHub releases of `pieceofmindcloud/pom`
   (`GET https://api.github.com/repos/pieceofmindcloud/pom/releases`), as a
   public repository, without a token. Drafts, pre-releases and tags that are
   not SemVer are skipped. Not every release carries every platform, so the
   newest release with an asset for this build wins;
2. the site's download listing (`GET <license server>/v1/pom/latest`), which
   names the newest file per platform with its own tag. While the site hands
   out downloads only with an invitation it gives no address, and the update
   is shown with `installable: false, blocked: "invitation_required"`.

A release is newer when its SemVer tag is above `POM_RELEASE_VERSION` (stamped
by `crates/node/build.rs`, see `build-and-run.md`). Detection runs at start and
every 6 hours, and on demand through `POST /api/ui/pom-update/check`; it
publishes `{core_update, current_version, checked_at, check_error}` at
`GET /api/ui/pom-update/status` and downloads nothing.

`POST /api/ui/pom-update/apply` with `{ "release_id": "<tag>" }` checks again,
requires that tag to still be the update and to be installable (never in a
container), downloads the asset next to the installation, verifies its size and
SHA-256 (GitHub's asset `digest`, or the site's `x-content-sha256` header),
unpacks it and hands over to `pom-plugin-host --apply-update <plan.json>`,
started from a copy in the staging folder so it may replace the installed host
too. The host waits for the node to exit, swaps every entry the package carries
(the AppImage file, the `POM.app` bundle, or each entry of the tar.gz/zip
folder) with a backup, rolls back on any failure, and starts POM again with the
original arguments (`crates/plugin-host/src/update.rs`). The answer is
`{restarting: true}`; a refusal is `409` with `{error}`.

The Plugins screen's "Check for updates" asks this route together with every
installed plugin and lists POM last in its dialog, since installing it restarts
the node; the About screen shows the same state alone.

## Tests

```bash
cargo test -p enterprise-protocol
cargo test -p enterprise-plugin-fixture
cargo test -p pom-plugin-host
cargo test -p node plugins
cargo test -p node pom_core_update
cargo test -p pom-plugin-host update
cd apps/frontend && npm test -- src/lib/access.test.ts src/components/nav.test.ts src/App.test.tsx src/routes/Plugins.test.tsx src/routes/About.test.tsx
./scripts/build-enterprise-plugin.sh --platform macos-aarch64 --debug
./scripts/test_pom_release.sh
./pom-release.sh --platform linux-x86_64 --dry-run
./pom-release.sh --platform macos-aarch64 --dry-run
./pom-release.sh --platform windows-x86_64 --dry-run
```

## Maintaining this file

Update this document when a phase changes state, when a new plugin enters the
catalog, or when the boundary between the `pom` process, the host and the
plugin library(ies) changes. Keep the migration fallbacks and the limits that
still have no integration explicit.
