# src/i18n

Two-language UI localization (English and Brazilian Portuguese) for the pom
panel. All visible strings live here as flat message keys; components consume
them through `useI18n().t`. The same file also holds a module-level active
locale used by non-React code (`api.ts`, `chatStore.ts`).

The translations live in `messages.csv`, with the columns `key,en,pt-BR`, one
row per key and the messages in ICU MessageFormat. `locales.ts` imports the CSV
as raw text at build time and interprets the messages synchronously with
`intl-messageformat` 11.2.14. The provider stays separate. Tests:
`locales.test.ts` (resolution + formatting), `locales.equivalence.test.ts`
(frozen reference dump with explicit copy revisions) and
`provider.test.tsx` (jsdom).

## Per-file

- `messages.csv` - the single source of the messages, ordered by key for stable
  diffs. Simple texts stay literal; the administrative password messages explain
  that only the Argon2id hash is persisted and never carry a real secret;
  interpolation uses `{name}` and
  logic uses `plural`, `select` and exact ICU selectors. Internal `select`
  values are prepared only by the loader to preserve truthiness conditionals that
  existed in the old code. The deployment keys also cover `unloadIfIdle`, idle
  minutes and the `idle` status. The Public networks and Statistics filters keep
  their options and labels in English and pt-BR here, with no hard-coded strings
  in the routes.
  The machine card's metrics use the GPU, usage and power labels here to keep the
  detail available in both languages.
  `chat.telemetryLine.*` covers the compact telemetry line of the Chat; the
  detailed telemetry texts moved to the plugin that claims `chat.telemetry`
  (`locales.equivalence.test.ts` lists them as moved). The `deploy.knob.hidden_state_dtype*` messages explain that f16
  stays the default, f32 uses four bytes per value without loss, and
  f8/i4g64/i4g128 are lossy options subject to a capability.
  `deploy.visionUnavailable` states that the screen only offers vision when the
  identity and the GGUF dimensions prove the projector association.
- The `deployStudio.*` group holds every label, tooltip and accessibility text of the
  Deploy screen (`DeployModel`, `DeployNetworkCanvas` and the vision rows of
  `DeploymentInspector`): the kind selector, the machine and Tensor Parallelism
  panels, the per-machine range buttons, the VRAM projection warnings and the map
  toolbar. These screens no longer carry hard-coded Portuguese. `common.whatIs`,
  `common.warningFor` and `common.viewWarningFor` name the `InfoTip` and
  `AlertTooltip` triggers; `chat.image*` and `chat.noCanvas2d` are the image
  resize errors, `peer.routeUnavailable` and `plugins.screenExportMissing` are
  read through `formatMessage(getActiveLocale(), ...)` outside React.
- The keys `chat.attachmentResized` and `chat.imageContextTooSmallDetail` cover
  the automatic image downsizing notice and the rare refusal when the context
  does not fit even the minimum resolution.
- The messages `chat.thinkToggle` and `chat.thinkToggleHint` name the Chat's
  persistent thinking control in both languages.
- The `adminAuthority.*` group covers the principal's screen and the read-only
  views of secondary admins and nodes: roster, promotion chain, successor,
  concrete refusals, confirmations and the outcome of the actions. The
  `notifications.*` group also names succession offers. The keys of the
  removed in-place role changes (`notifications.nodePromotion*`,
  `notifications.nodeRelinquish*`, `network.requestNode*`,
  `network.relinquishNode*`) are still in the table and unused.
- The `plugins.*` group covers loading, errors and empty-catalog states,
  missing metadata, available releases, prices, active or unnecessary licenses,
  per-plugin activation usage and limits, public-store purchase links, the
  screenshot lightbox, activation and per-plugin actions, a single global card
  for adding local GitHub repositories, manifest paths, releases and
  confirmation of the selected tag, typed preferences, and local
  development-source builds and installs without license activation. It also
  provides an action to remove local builds and reports only build stages and
  exit codes. Release errors include the detail validated by the node.
  Activation also displays installation failures and handshake details,
  distinguishing a valid license from a host that is not ready yet. Every label
  must exist in both the `en` and `pt-BR` columns; names, descriptions, codes,
  releases and image URLs come from the public catalog and are not fixed
  translations. Catalog descriptions fall back to English when the selected
  language is unavailable. License expiration, last installation/application
  and last update check are distinct timestamps in the node contract and are
  formatted using the selected locale.
  `plugins.loading`, `plugins.loadingNamed` and `plugins.loadingHint` are the
  words of the puzzle drawn while a plugin screen opens (the plugin's own texts
  may not have arrived yet, so they are POM's keys, not the plugin's).
- `locales.ts` - a small CSV parser, synchronous loading of the `?raw`, an
  `IntlMessageFormat` cache and the public locale API. Do not put new
  translations here. The fallback stays: requested locale, then English, then
  the key itself. ICU formatting errors are warned in the console and return the
  raw default, so an omitted variable does not take the screen down. All
  existing keys and texts, including the deployment logs, preferences, telemetry
  and remote models groups, live in the CSV.
- The `trustAnchor.*` group covers the trust root screen: anchored and pending
  state, the fingerprint and its copy, the install form, and the five server
  refusals by code (`trustAnchor.refusal.*`). The screen shows the translated
  refusal and only falls back to the server text for a code it does not know.
- The `remoteModels.*` group covers the compact login status, search, trending
  public models, details, unknown license, confirmation, queue, progress,
  cancellation, retry and provider errors in both languages. Do not put CLI text
  or a local path in those values.
- The `huggingFace.*` group covers the token field of the Preferences screen.
  The persisted value never enters a message; the UI shows only the mask and the
  effective source.
- Plugin texts do **not** live in this CSV: each plugin carries its own
  catalogs (the former `tokenLogs.*` group moved to `pom-plugin-logs/i18n/`).
  `locales.ts` only offers `setPluginMessages`/`clearPluginMessages`, which put
  the `<plugin_code>.*` keys into the `formatMessage` map and never overwrite a
  POM key. See `docs/subsystems/plugins-ui.md`.
- The `deploymentLogs.*` group covers the process console of the local replica:
  auto-scroll state, copying the last 500 lines, the copy's outcome, the filter,
  and collection states.
- The `about.*` group covers the AGPL-3.0 license published in `LICENSE`, the
  links to the GNU text and to the repository file, the LGPD
  security/privacy notice, and the explanation of plugin creation; it also
  covers Enterprise status, the `pom_core` update, catalog count and filter,
  ecosystem labels, version and repository.
- `provider.tsx` - `LocaleProvider`, the `useI18n()` hook and the `LanguageSelect` component.

The official profile identifiers are `admin`, `worker` and `visitor`. The visible
label of the first goes through `profile.admin`; the last two stay lowercase by
product decision. The toolbar, the network screen and the bootstrap use these
keys, never literals nailed into components.

The `enterprise.*` group covers the discovery state of the Request Logs and
Statistics screens. Those screens stay navigable without a license so the user
sees the activation invitation and the purchase link. The same group names the
`pom_core` binary update notice and the explicit install-and-restart action.

## Editing messages

- `SUPPORTED_LOCALES = ["en", "pt-BR"]`, and `Locale` remains the literal union.
- Every key must exist in both columns. When adding or editing a message, change the same row in the CSV directly and keep the key ordered.
- Use the ICU syntax of `intl-messageformat`: `{count, plural, one {# node} other {# nodes}}` for plural, `{value}` for interpolation and `select` for textual choices. For conditional messages, preserve the value names the components already pass.
- The CSV enters the bundle through a `?raw` import; do not use fetch, async state or per-route loading.
- Run `npm exec -- vitest run src/i18n/locales.test.ts src/i18n/locales.equivalence.test.ts` after any change. The equivalence test compares every key, both languages and the cases of the frozen dump; deliberate copy revisions go in the test's `copyUpdates` map so the 1.8 MB minified dump is not rewritten.

- The deploy.addNode* group covers the incremental dialog: target machine
  catalog, whole model, KV cache location and K/V types. Every new key of this
  flow must exist in en and pt-BR, including the label
  deployments.kind.mixed used by the history.

## Key functions

### locales.ts
- `formatMessage(locale, key, values)` - looks up `messages[locale]`, falls back to `messages.en`, then returns the raw key if absent. The fallback chain is what makes adding a key to one locale safe.
- `resolveLocale(savedLocale, browserLocales)` - the saved value wins if valid; otherwise any `pt*` browser locale maps to `pt-BR`, any `en*` to `en`, default `en`. Saves nothing; the provider does.
- `getActiveLocale()` / `setActiveLocale(locale)` - module-level locale for code outside React (error strings in `api.ts`, default chat titles in `chatStore.ts`); the provider syncs it on mount.
- `isLocale(value)` - type guard.
- Node-written text (errors, refusal reasons, job steps) is English at the source; there is no client-side translation of server text.

The chat telemetry additions live in both locale blocks: execution RTT, the
source-to-destination edge scope, machine capacity/configuration, effective
placement, per-peer `POM_PROFILE_SYNC`, backend-reported execution engine,
  execution diagnosis, TP collective table, prefix reuse and transmitted-token
  counts, gateway prefix-miss reason and effective prefill chunk, overlap
  status, per-traversal transport identity/bytes, distributed
  telemetry status/coverage, remote-stream aggregates with producer/downstream/channel/queue/callback wait
  split and token delivery quantiles, synchronous MTP draft time, explicit HTTPS fallback counts/reasons, semantic TTFT labels,
  AI-analysis action, initial-model mark and the explicit "time outside the
  node" diagnosis must remain available in
English and Brazilian Portuguese. Generic configuration state labels for
fit-memory and KV offload are separate from the per-peer `POM_PROFILE_SYNC`
state labels. The footnote explains the measurements but does not claim a fixed
environment value.

The `canvas.gateway*` messages belong to the virtual return node on both
network canvases. They identify the gateway, return leg, cycle cost breakdown
and the count of deployed routes; the component displays `pipelineTotalUnknown`
when a route does not have all directional legs and no aggregate RTT is
available.

`chat.coverageUnavailable` names the explicit unknown state returned when the
Chat capability read is refused or cannot be completed. It must not be replaced
with the incomplete or empty-coverage message.

The `networkChat.*` messages cover the separate Network chat surface, including
the disabled state, participants, files, typing indicator, mobile online-list
toggle and unread badge. They must remain paired in both CSV locale columns
because this route is reachable from the same AI chat shell.

The keys `network.networkInfo*`, `network.share*`, `network.publicName*`,
`network.visibility*`, `network.access*` and the geographic fields cover the
network information screen and the sharing of the trust. Every new key of this
flow must stay present in the CSV's `en` and `pt-BR` columns; document and key
texts are server data, not locale messages.
The keys `network.forgetNetwork*` name the confirmation and the outcome of the
local network cleanup; they must not be confused with `network.leave`, which
takes the machine out of the cluster without erasing its configuration.
The keys `network.traversal*` label the NAT traversal panel, including the three
NAT report states in `network.traversalNat.{unknown,public,private}`. The
`network.*` copy that named libp2p now names the p2p transport (revisions in
`locales.equivalence.test.ts`).
The keys `publicNetworks.*`, `network.metrics*`, `network.participants*`,
`keys.*`, `preferences.admin*` and `nav.*` cover the reorganization of the
screens, including unavailable metric states, the reachable/unreachable states
of the Public networks indicator, and the labels of the separate Network chat.
The keys `setupWizard.uploadTrust*` name the alternative upload of
`trust.json`; the wizard's manual confirmation checkbox no longer exists.

The per-request time labels also exist in both blocks: TTFT, prompt
preprocessing speed, output generation speed, total time, tokens/s, and the
explicit unmeasurable-rate label used for a zero-token prefill or a one-token
answer.

The Chat also translates the error label arriving inside the stream, the control
to copy the message, and the copy state. The server text is preserved without
going through the language fallback, so the real diagnosis is not lost.

The inference head combo also has keys for its accessible name and for the
diagnostic hint shown next to the native selector with a model visual.

The `chat.history` message names the bounded conversation list used by the
`visitor` Sidebar. It remains in both CSV columns even though it is primarily an
accessible label, so the menu has no language-specific copy in the component.

The `chat.head*` messages name the infer-only entry choice: selected head,
layer-zero alternatives, coverage/RTT/load policy, five-minute RTT validity,
stale-measurement fallback and the explicit no-candidate/incomplete-pipeline
states. They stay in the CSV because this panel is visible in both locales.

The slice locale group also covers multipart part lists, summed-size loading
state and explicit incomplete-set errors in both languages.

The `dashboard.local*` messages describe the node and infer dashboard cards:
local model, layer slice, in-flight requests and the explicit unavailable state
when `/api/ui/node` cannot be read. The cluster-wide coverage and pipeline
verdict remain administrator copy, so a profile with only local evidence never
gets a translated zero or false incomplete alarm.

### provider.tsx
- `LocaleProvider({ children, initialLocale })` - initial state from `initialLocale` or from `localStorage` `codai.locale` + `navigator.languages`; writes `<html lang>` on change; persists the choice.
- `useI18n()` - `{ locale, setLocale, t }`.
- `LanguageSelect({ className })` - the en/pt-BR `<select>`.

## Links to the rest of the app

- Imported by nearly every route and component (28 files use `useI18n`).
- `api.ts` and `chatStore.ts` import `getActiveLocale` from `locales` for non-React strings.
- `index.html` has a pre-paint script that stamps `document.documentElement.lang` from the same `codai.locale` + browser-locale logic, so the first render is already in the right language before `LocaleProvider` mounts.
- Chat attachment, context-limit, drop-zone and Web Speech labels live in
  `messages.csv` in both locales. `locales.ts` remains the parser and fallback
  layer; new Chat copy should be added to the CSV rather than hard-coded in the
  route.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.