# Subsystem: UI provided by plugins

A plugin can ship, together with its backend, its own menu, screens, routes,
profiles and texts. POM only implements the generic mechanism (the
`pom-plugin-ui/v1` contract); the frontend knows no specific plugin. The native
routes `/logs` and `/estatisticas` remain as **advertising fallbacks** of
`EnterpriseFeatureShowcase`, but a manifest can declare one of them to replace
the screen at that URL. Other plugin screens live under each `plugin_code`'s
namespace. The real code of Request Logs and Statistics lives in
`pom-plugin-logs`, not here.

## How it reaches the browser

1. The plugin artifact (the same binary verified by SHA-256 at install time)
   carries the manifest, the JS/CSS bundle and the i18n catalogs. The plugin
   host answers the `ui.manifest` and `ui.asset` operations of the generic
   `query`. Nothing changes on the license server, and manifest and backend
   never drift in version.
2. `crates/node/src/plugin_ui.rs` validates the manifest
   (`PluginUiManifest::parse`) and serves:
   - `GET /api/ui/plugins/ui` - `{plugins: [{code, version, manifest}]}` only for
     **active** plugins with a running host and a valid manifest. Inactive,
     missing or invalid = not listed = only the fallbacks.
   - `GET /api/ui/plugins/:code/assets/*path` - only what the manifest lists in
     `assets`; content type from a closed list, `nosniff`, `no-cache`. This
     includes the menu PNG and documentation Markdown. i18n catalogs go through
     `validate_catalog` before leaving.
   Both sit under `/api/ui/plugins`, open to every profile
   (`RouteScope::InferNodeAdmin`, since plugins are local to each node); the
   authorization of the data the screen reads stays in the plugin's routes (for
   example `/api/ui/requests`, AdminOnly).
3. The frontend (`usePluginUi`) stores the result in `lib/pluginUi.ts`. The menu
   (`components/nav.ts`), the profile guard (`lib/access.ts`), text loading
   (`lib/pluginI18n.ts`) and the router (`App.tsx`) all read the same validated
   registry. The Plugins screen invalidates that query after activation,
   install, update or deactivation actions, so navigation follows the change as
   soon as the node confirms it; the 30 s polling covers changes made outside
   this tab.

## Manifest (`pom-plugin-ui/v1`)

| Field | Rule |
|---|---|
| `schema`, `plugin_code` | `pom-plugin-ui/v1`; the code must be the installed plugin's |
| `icon_image` | optional; path of a `.png` asset. The frontend uses it in the menu and falls back to `menu[].icon` when the image fails or is missing |
| `description` | optional; short text per language (`en` required, up to 2000 bytes each), shown on the Plugins screen card |
| `screenshots` | optional; up to 8 images (`.png`, `.jpg`, `.jpeg`, `.webp`, `.gif`) as paths relative to the root of the plugin's GitHub repository or `https://` URLs without credentials. They are not assets and the node never serves them: relative paths become `https://raw.githubusercontent.com/<owner>/<repo>/HEAD/<path>` and the browser loads them straight from GitHub |
| `menu[]` | `id` (unique in the manifest, including children), `label` per language (`en` required), `icon` (a known name, otherwise generic), `to`, optional `roles` (`admin`/`node`/`infer`); with neither `roles` nor the legacy `profiles` field, every profile is eligible. `group` is accepted but does not organize items |
| `menu[].children[]` | optional; submenus with `id`, `label`, `icon`, `to` and `roles` (intersected with the parent's). With children, the item becomes an expandable section of the sidebar; the parent's `to` is still required for hosts without submenu support |
| `routes[]` | `path`, `screen`, optional `roles` (`admin`/`node`/`infer`) or legacy `profiles` (`admin`/`node`/`infer`, non-empty), `full_bleed`; with neither, every profile is eligible |
| `screens{}` | `module` (ES asset), `export` (default `default`), `styles[]` |
| `i18n{}` | language -> JSON asset; if present, `en` is required |
| `documentation` | optional; current format with `guides[]` and `reference[]`, each entry with `id`, a localized `title` (with `en`) and a relative Markdown asset under `docs/`; a published list of `docs/*.md` paths is also normalized into guides |
| `assets[]` | closed list; relative paths, no `..`; everything screens, documentation and i18n mention must be here |

A manifest that also publishes documentation uses this format. The plugin
keeps the files inside the package's `docs/` folder; POM does not write or move
them into the workspace.

~~~json
{
  "icon_image": "ui/icon.png",
  "documentation": {
    "guides": [
      {
        "id": "getting-started",
        "title": {"en": "Getting started", "pt-BR": "Primeiros passos"},
        "asset": "docs/guides/getting-started.md"
      }
    ],
    "reference": [
      {
        "id": "configuration",
        "title": {"en": "Configuration"},
        "asset": "docs/reference/configuration.md"
      }
    ]
  },
  "assets": ["ui/icon.png", "docs/guides/getting-started.md", "docs/reference/configuration.md"]
}
~~~

The `/admin-ui/documentacao` screen adds the guides and reference of active
plugins to the system documentation. It downloads each Markdown file through
the assets route only when the person opens it; an inactive plugin or an
invalid manifest publishes no pages.

The host normalizes each `menu[].to` and `routes[].path` under
`/<plugin_code>/...` and keeps both in the same registry. So
`plugin_code: "base"` with `to: "/example"` and `path: "/example"` uses
`/base/example` both in the link and in the matcher; in the browser, the
basename adds `/admin-ui`. Repeated and trailing slashes are normalized. A
prefix already equal to the `plugin_code` is not duplicated, which keeps
manifests that already carry that segment working. Paths with `.`/`..`,
encoded separators, an invalid schema, a mismatched code or inconsistent
structures make the host ignore the whole manifest. A menu target must also be
covered by a declared route.

Different plugins can use the same relative path: their targets end up at, for
example, `/base/example` and `/analytics/example`. `/logs` and `/estatisticas`
are native extension points: if exactly one active plugin declares one of
them, its route and menu use the native URL and replace the matching fallback.
If two plugins claim the same point, neither takes the native URL and both stay
under their namespaces, avoiding a collision. Without a single declaration,
POM's fallback stays available.

New menus are shown as top-level items before Documentation, in the order they
appear in the manifests and in the plugin list. An item with `children`
becomes an expandable section, like Models or Network, with one submenu per
child visible to the profile; the section disappears when no child is left. The
`group` field still has no effect. Older hosts ignore `children` and only show
the parent's `to`, which is why the contract is still `pom-plugin-ui/v1`. The
registry normalizes each target once and both navigation and the router
consume that same path. The `/admin-ui` basename is still applied by
`BrowserRouter`, including on direct reloads and browser history navigation.

The route's profiles decide who enters the plugin screen - including `infer`,
whose reduced POM list does not apply to these routes. `menu[].roles`
restricts the link's visibility and the matching direct route;
`routes[].roles` takes precedence over `routes[].profiles`, and the effective
list is the intersection with the menus that cover the target. For existing
manifests, `menu[].profiles` is still accepted as an equivalent when `roles`
was not declared; `roles` wins when both fields appear. With none of the menu
or route fields, every profile is eligible. A profile outside the effective
list gets the normal refusal.

## Screens

`components/PluginScreen.tsx` imports the asset module (same origin, the
session applies) and mounts the declared export as a React component of the
app's own tree, with an `ErrorBoundary`; a failure becomes an error card. The
host remounts the screen when the normalized route changes, including between
targets of the same `plugin_code`, so an internal navigation does not retain
the previous route's component. The plugin does not bundle another React:
`lib/pluginHost.ts` exposes on `globalThis.__POM_HOST__` React, ReactDOM, the
JSX runtime, React Query, the design kit (`components/ui`), `format`,
`useI18n`, `useTheme`, `useCluster`, `api.get`, `Modal`, `TelemetryLine`,
`ExecutionTelemetry`, `Markdown` and `storageKey`. The plugin swaps those
imports for reads of that object when bundling; hooks and contexts are the
application's. The surface is versioned (`PLUGIN_HOST_VERSION`) and only grows
compatibly. Version 2 added `context()` (current language and theme), `events`,
`protocol` and `plugin(code)`, the door to the event protocol described below.
Version 3 added `TelemetryLine` (the basic one-line telemetry) and the
`chat.telemetry` slot; the detailed telemetry view left the POM, so
`ExecutionTelemetry` is kept only for bundles built against version 2 and now
renders that basic line. The plugin's CSS uses the same color tokens (CSS
variables) as POM.

## Plugin RPC

A plugin screen that needs more than reading (saving settings, a directory,
custom queries) calls `__POM_HOST__.plugin(code).rpc(op, params)` (host
version 4). It posts to `POST /api/ui/plugins/<code>/rpc/<op>`; the node
forwards `{params, profile}` to the plugin's `query` as operation
`rpc.<op>` and returns the plugin's JSON reply as is. The node knows no
operation and checks only the name, the size (256 KiB), the same origin and
that the plugin is active. `profile` is the node's persisted profile
(`admin`, `node` or `infer`), so the plugin decides who may call each
operation; the enterprise plugin, for example, answers its directory and
insights only for `admin`.

## Slots inside native screens

`slots` maps a slot id (lowercase, dots, dashes or underscores, up to 64
characters; at most 16 per manifest) to a key of `screens`. Both
`plugin_ui.rs` and `lib/pluginUi.ts` reject a manifest whose slot names an
undeclared screen. A host screen that offers a slot asks
`usePluginSlot("<id>")` (`components/PluginSlot.tsx`) and renders `PluginSlot`
with its own props; the first active plugin in the node's order that claims the
id wins. Loading or render failures show the slot's fallback instead of an
error card. Hosts ignore slot ids they do not offer.

| Slot | Where | Props |
|---|---|---|
| `chat.telemetry` | end of the compact telemetry line under each Chat answer (not for `infer`, not while streaming) | `ChatTelemetrySlotProps` in `lib/chatTelemetry.ts`: `messageId`, `trace`, `timings`, `source`, `environment` (cluster snapshot, network RTT, runtimes - fetched only while a plugin claims the slot), `model`, `onAsk({visibleText, requestContent})`, `askDisabled` |

~~~json
{
  "screens": { "chat-telemetry": { "module": "ui/screens.js", "export": "chatTelemetry", "styles": ["ui/plugin.css"] } },
  "slots": { "chat.telemetry": "chat-telemetry" }
}
~~~

## Release preferences and host context

Local preferences are not part of the `pom-plugin-ui/v1` screen contract
above. The typed schema is optional in `PluginManifest.preferences` in the
release/license contract (`crates/enterprise-protocol`): boolean toggles and an
optional storage directory field. POM validates that schema, applies the
defaults, persists the values per `plugin_code` in `pom.db` and never sends the
values to the license server nor treats them as entitlement. The Plugins
screen edits the values in a modal opened by the gear icon at the top of the
card, only while the plugin is active, and shows the reason returned by the
node when saving is refused. An absolute storage directory that does not exist
yet is created on save. After saving, the node resends `host.configure` and
sends `host.event` with `preferences.changed`.

`host.configure` includes the local values under `preferences`, together with
the gateway context, for the active plugin itself. This is IPC between the node
and the local host, not traffic to the license server. The **Add from GitHub**
card sits at the top of the right column, above license activation, and
neither reads nor queries the server catalog. The operator registers any
public GitHub repository; POM reads `manifest.json` from the root or, if
missing, `ui/manifest.json` (`pom-plugin-ui/v1`) and lists the published
releases together with the manifest metadata. In the second format, each
release attaches the per-platform manifest `pom-plugin-<os>-<arch>.json`. The
manifest declares `plugin_code`, name, `plugin_abi`, features, optional
preferences and an `artifacts` map per `<os>-<arch>`. Each repository becomes
(or joins) a plugin card with a select of only the releases compatible with the
node, preselected on the latest, and the Install or Update action; the operator
can pick an older tag. POM validates ABI and manifest, requires the SHA-256
digest from the GitHub API, downloads and verifies the asset and installs the
chosen version without checking whether it needs a license. The repository
list stays local to the node; installing creates no lease, grant or
entitlement. Plugin activation/licensing stays separate. Any profile (`admin`,
`node`, `infer`) can register repositories, install and activate plugins on its
own node. See `enterprise-licensing.md` for contract, integrity and
persistence.

The `workspace_root` field of the same contract is the only configurable root
for user projects and dynamic plugin data. Projects live directly under that
root. Each plugin must keep its dynamic state under a hidden directory
namespaced by its own `plugin_code`, for example `.deepseek_harness`;
project-specific data, when needed, lives inside the matching hidden directory
in that project. Runtime and fixed install files stay outside the workspace.
POM neither migrates nor removes existing plugin content: adopting the field
applies to new data and the plugin decides how to preserve its legacy data.

## Local rebuild in development

The local Compose mounts `POM_DEV_PLUGINS_DIR` (default `..`, the workspace directory containing the sibling plugin repositories) at
`/opt/pom/dev-plugins` and enables `POM_DESENV=1`. The optional cluster
`docker-compose.dev-plugins.yml` keeps each source bind read-only and overrides
the build image to `dev-runtime`; that target includes Rust/Cargo and native
tools, exposes the toolchain to the non-root node user and sets `CARGO_HOME`
to a writable cache in the project data volume. The release
`runtime` image still has no compiler. The panel offers the rebuild block on every
profile when `/api/ui/node` confirms `pom_desenv=true`.

The backend discovers direct child directories named `pom-plugin-*` under the
source mount; a mounted directory is listed only when its Cargo manifest and
root `manifest.json` or `ui/manifest.json` are valid and all plugin manifests
agree on a safe `plugin_code`. The code-server source uses
`plugin_code: "code_server"`. `GET /api/ui/plugins/dev-sources` lists only
valid mounted sources. The AdminOnly route
`POST /api/ui/plugins/dev-sources/:repository/rebuild-install` uses fixed
arguments for `cargo metadata --no-deps --locked` and `cargo build --release
--locked`, requires a single `cdylib` target and checks canonical source and
manifest paths. `CARGO_TARGET_DIR` points to
`<project_root>/.pom-dev-plugin-builds/<repository>`, outside the mounted source,
so read-only source binds are supported; the installed artifact must canonicalize
inside that build directory. There is no shell command and no build argument
taken from the request.

The local build can install a plugin even when it has never been installed or activated. It writes a separate development-install record, starts only that plugin's isolated host, and grants no licensed features or license state. If a licensed or GitHub installation already exists, the local artifact replaces its running binary without changing that entitlement. The operation never activates or renews a license, calls GitHub or the license server, changes the release selection or restarts the POM process. The response exposes success or the step/exit code, never the compiler's stderr. The GitHub release install flow stays independent and unchanged.

## Node context (`host.configure`)

A plugin that calls the node's models embeds neither address nor credential:
the node hands them over through the generic `query`, in the `host.configure`
operation:

```json
{"operation": "host.configure",
 "gateway": {"openai_base_url": "http://127.0.0.1:8080/v1", "api_key": "sk-..."},
 "workspace_root": "/data/workspace",
 "preferences": {"features": {"capture_headers": false}, "storage_directory": null}}
```

`openai_base_url` is the node's own gateway on loopback (loopback clear text is
always accepted, even with TLS). `api_key` is the node's chat key
(`pom-<node_id>`), the same one the UI Chat uses, so it also works on a node
without administrative authority. The node hands the context to every host
that starts and again when it changes (key revoked and recreated); plugins that
do not know the operation answer with an error, which is ignored.
`workspace_root` is an absolute path visible and writable by the plugin
process inside the container. On a new install the persisted default is
logically `~/pom_workspace`, with no question in the wizard; in Compose the
stable mount is `/data/workspace` and must stay persistent across restarts.
POM checks the path when saving the configuration and resends the context to
running hosts when the root changes.

## Services a plugin offers to the cluster

A plugin is local to its node, but what it runs can serve everyone - a vector
database that is the retrieval source of the pipeline, for example. Two generic
pieces, in `crates/node/src/plugin_services.rs`, make that visible and
reachable without the node knowing any plugin.

**`host.services`.** A running plugin may describe what it serves:

```json
{"operation": "host.services"}
{"services": [{"id": "rag", "kind": "rag", "name": "Qdrant",
               "status": "ready", "version": "v1.19.1", "detail": null}]}
```

`id` and `kind` are lowercase slugs, `name` and the optional `version` and
`detail` are short single-line texts, `architectures` is an optional list of
GGUF architecture slugs (up to 16, meaningful for kind `inference`), and `status` is one of `ready`,
`starting`, `installing`, `waiting` or `error`. The node drops an entry that
does not follow the contract (at most 8 per plugin), asks the plugins at most
every 5 s and publishes the result in `plugin_services` of `GET /api/ui/node`.
The dashboard draws each service next to its machine: in the Flow, hanging
under the machine's card with a connecting line, and in the machine's detail
card (`components/PluginServices.tsx`). A plugin that does not know the
operation answers with an error, which is ignored.

**Peer calls.** `POST /api/internal/plugins/:code/peer/:op` lets a peer call
the plugin's `peer.<op>` operation on another machine. The call must carry the
signed control envelope (the shared connection key alone is refused, because it
proves membership and not identity) and its issuer must be a live peer. The
plugin receives who is asking and decides what to hand over:

```json
{"operation": "peer.connection", "params": null,
 "caller": {"node_id": "<node>", "head": true, "model": "<model>"}}
```

`head` is true when the caller announces layer 0 of a model, the machine every
prompt enters through. This is how a plugin shares a secret with the heads of
the network and nobody else: the Qdrant plugin answers `peer.connection` with
its port and API key only when `caller.head` is true. The body is capped at
256 KiB and the plugin's reply is returned as is.

`GET /api/internal/plugins/services` (signed or key-authenticated peer call)
returns the same service list, so a peer can find which machine serves what.

**Inference services.** A service of kind `inference` runs whole models in
place of llama.cpp and lists the GGUF `architectures` its engine runs. It may
also publish `settings`, the engine settings a deployment can set: up to 32
entries of `{key, type, label, description?, min?, max?, options?}`, where
`key` is lowercase words with dots or underscores, `type` is `number`,
`integer`, `boolean`, `enum` (with `options`) or `list`, and `label` and
`description` are texts per language with `en` required. The node checks only
that shape; the deploy inspector draws them, an empty field keeps the
engine's default, and the values travel in `RuntimeParams.engine_settings` to
`engine.load` (`params.settings`), where the plugin validates them. Such a
plugin implements four operations the node calls through its own local host,
never from another machine or a browser: `engine.load` (`params`:
`model_path`, `model_label`, `n_ctx`, `max_sessions`, `kv`, `gpus`,
`settings`),
`engine.unload`, `engine.status` (`state` among `idle`, `installing`,
`preparing`, `loading`, `ready`, `stopping`, `error`, with `detail`, `n_ctx`,
`busy`) and `engine.upstream` (`{status: "ready", port, token}` of a loopback
server whose `POST /pom/generate` takes the rendered prompt and streams NDJSON).
The deployment and generation contract is in `inference.md`, section 7.1, and
`deployment.md`, section 1.6.

## Transparent retrieval (RAG)

`crates/node/src/rag.rs` adds passages from the network's RAG server to chat
requests. The RAG server is any plugin, on any machine, that publishes a ready
service of kind `rag`; the gateway knows no plugin by name.

- **Finding the server.** The gateway keeps the choice for 15 s. A request
  never waits for the network to find it: this node's own plugins are asked in
  line, peers are asked behind the request (`/api/internal/plugins/services`),
  so with the server on another machine the first request after startup goes
  out without context.
- **Whether to search at all (optional, off by default).** With
  `POM_RAG_DECISION=1`, when the request asked for retrieval with
  `"rag": true`, the gateway first asks the pipeline a typed yes-or-no
  decision (`decisions.rs`, the same mechanism as `/v1/systemone`): would the
  user's stored documents help answer this message? It costs one token; the
  last peer reports the probability of each label, and below 0.3 for "yes"
  (`POM_RAG_DECISION_MIN`) nothing is searched and nothing is added. The
  question's head is fixed and kept by the prefix cache, and the decision
  reads only the last 600 characters of the message, never the conversation:
  a 30 thousand token prompt is not sent twice. It is still an extra pass
  through the pipeline for those characters, so it only pays for itself when
  a good share of the messages sent with retrieval on turn out not to need
  documents; without it, the search score (`POM_RAG_MIN_SCORE`) is what keeps
  irrelevant passages out, at no pipeline cost. A decision that fails does not
  stop the search. The audit records the probability as
  `rag.needed`, and `rag: {skipped: "not_needed"}` when the search was skipped.
  Like the query rewrite below, the decision is a request of its own and takes
  a KV session on every hop.
- **The query.** A message of up to 400 characters
  (`POM_RAG_REWRITE_MIN_CHARS`) is searched as written. A longer one is reduced
  to search terms first: when the request asked for retrieval with
  `"rag": true`, by a short generation on the pipeline itself (at most 32
  tokens, no thinking, 8 s, reading the first 400 and the last 1100 characters
  of the message); otherwise, or when that generation fails or
  `POM_RAG_REWRITE=0`, by dropping stop words and repeats and keeping the last
  24 significant words. The short generation is a request of its own and takes
  a KV session on every hop: with `max_sessions = 1` it displaces the retained
  conversation, which comes back from the host-memory cache or is prefilled
  again. The audit records the query and how it was obtained
  (`rag.query`, `rag.query_source`: `message`, `model` or `words`).
- **Searching.** `POST /v1/chat/completions` takes the text of the last
  message when a person wrote it (role `user`; a tool result or an assistant
  turn at the end is an agent in its loop, not a question) and calls the
  plugin's `peer.search` - directly when the plugin is on this node, through
  the peer route otherwise. The call goes along the route already measured to
  that machine (`GatewayState::control_post_json_routed`): the pinned address,
  or the libp2p tunnel alone when that is what reaches a peer behind NAT,
  instead of walking every announced address. The whole search has 300 ms
  (`POM_RAG_TIMEOUT_MS`, 20 to 5000); past it, or on any failure, the request
  goes on unchanged. Retrieval never fails a request.
- **What is added.** One passage by default (`POM_RAG_PASSAGES`, up to 20),
  of at most 1500 characters, scoring at least
  0.8 (`POM_RAG_MIN_SCORE`), in the order the plugin ranked them - its ranking
  may use more than the similarity it reports as `score` -, appended after the whole conversation as a
  "Retrieved context" block naming each source. `x_pom` is unchanged; the
  request audit gets `rag: {node_id, plugin, passages: [{path, score}]}`,
  never the text.
- **The prefix cache survives.** The passages change with every question, so
  they must not be part of the retained KV. The request carries a retain
  boundary at the end of the conversation (`DecisionProbe.retain_prefix_bytes`
  with no labels; see `inference.md` 3.9) and the cache key stays the
  conversation alone. The next turn's prompt starts with exactly what was
  kept. A request with this boundary is not batched with other prefills on the
  head, like every probe request.
- **Collections.** `"rag_collection": "<name>"` in the request keeps the search
  to one collection of the server (letters, digits, `_` and `-`; anything else
  is ignored); without it the plugin searches every collection it has a folder
  linked to. The gateway reads the names with the plugin's `peer.collections`
  (same route and timeout as the search, cached 15 s) and publishes them as
  `rag_collections` in `GET /api/ui/chat/capabilities`. The audit records the
  collection as `rag.collection`.
- **Turning it on and off.** A request without the `rag` field retrieves;
  `"rag": false` does not. The Chat has a database icon next to the model
  picker (shown only while `GET /api/ui/chat/capabilities` reports
  `rag: "supported"`); its menu offers no search (the default), every
  collection, or one collection, and the Chat always states the choice,
  `true` or `false`. `POM_RAG=0` turns retrieval
  off on the gateway.

`/v1/messages`, `/v1/completions` and `/api/generate` do not retrieve.

## Events and notifications (`pom-plugin-events/v1`)

POM and plugins talk through a single protocol. Every event uses the same JSON
envelope, whatever the channel:

```json
{"protocol": "pom-plugin-events/v1", "id": "<hex>", "type": "theme.changed",
 "target": "*", "source": "pom", "at": "<RFC 3339>", "payload": {"theme": "light"}}
```

`target` is `*` (all plugins) or a `plugin_code`. Events POM sends:

| Type | Payload | When |
|---|---|---|
| `locale.changed` | `{locale}` | language change and once when the shell mounts |
| `theme.changed` | `{theme}` | theme change and once when the shell mounts |
| `preferences.changed` | `{preferences}` | the plugin's preferences were saved (native channel) |
| `notification.received` | `{message_id, plugin_code, title, body, level, sender_node_id, sent_at}` | a plugin notice arrived over the network |
| `notification.response` | `{request_id, kind, action, error?}` | answer to a plugin command |
| `deployment.started` | `{kind, deploy_id, job_id, model, nodes, deployment_name?, reused?, version?}` | a deployment (`kind: "deploy"`) or redeployment (`"redeploy"`) started on this node |
| `model.serving` | `{model, deploy_ids, node_ids}` | a model starts being served by the network (enters `/v1/models`) |
| `model.stopped` | `{model}` | a model stops being served |

Browser channels (`lib/pluginEvents.ts`, fed by `components/PluginBridge.tsx`):

- SDK: `__POM_HOST__.plugin(code).events.subscribe(type | "*", handler, {replay})`;
  `replay` immediately delivers the last `locale.changed`/`theme.changed`;
- DOM: `CustomEvent("pom:plugin-event", {detail: envelope})` on `window`, for
  code without the SDK, such as a third-party application embedded in a screen
  (the harness case, which does not use POM's React hooks);
- iframes: `postMessage(envelope)` to `iframe[data-pom-plugin="<code>"]` of the
  same origin.

Native channel: the node calls the host's `query` with
`{"operation": "host.event", "event": <envelope>}`
(`PluginRuntime::send_host_event`, envelope built by `plugin_event_envelope`).
It carries `preferences.changed` to the plugin itself and, to every running
plugin, the node events published by `PluginManager::publish_event`:
`deployment.started` (emitted by `deploy_endpoint` and `redeploy_endpoint`) and
`model.serving`/`model.stopped` (`spawn_serving_watcher`, which every 5 s
compares the served models, the same as `/v1/models`; the first snapshot is
only the baseline). The node events also sit in a sequenced queue of 100, read
by the UI at `GET /api/ui/plugins/events?after=<seq>` (open to every profile,
like the rest of `/api/ui/plugins`); without `after`, the response only carries
the current sequence. `PluginBridge` polls the route every 3 s and forwards the
envelopes to the screens over the browser channels, keeping `id` and `at`
(`forwardNodeEvent`). A plugin that does not know the operation answers with an
error, ignored as with `host.configure`.

In the other direction, the plugin sends commands through the SDK
(`plugin(code).notifications.notify|confirm`, which return the answer) or
through
`CustomEvent("pom:plugin-request", {detail: {protocol, id, type, plugin_code, payload}})`
and same-origin `postMessage`. `runPluginCommand` validates protocol, active
plugin and payload, and always answers with a `notification.response`
addressed to the plugin:

| Command | Payload | Result |
|---|---|---|
| `notification.notify` | `{title, body?, level?, scope?}` | `delivered` or `failed` |
| `notification.confirm` | `{title, body?, acceptLabel?, cancelLabel?, tone?, display?}` | `accepted` or `cancelled` |

`scope: "local"` (default) shows a toast only on this UI and leaves the notice
in the bell. `scope: "network"` publishes a `plugin_notice` through the network
Chat (see `p2p-network.md`, "Plugin notices"): it requires the network Chat to
be on, stays 24 h in memory on each node and shows up in the bell and as a
toast on the other UIs, and as `notification.received` for plugins. In the
bell, a notice is only a message, without buttons: it counts as unread until
the person opens the bell, when it becomes read (read ids stay in
`localStorage`, so a network notice still present in the snapshot does not come
back as new after a reload), and it can be removed. `notification.confirm` puts
the request in the current user's top bar notification bell, with Accept and
Refuse, like a transfer offer (`lib/pluginInbox.ts`, this tab only), and
announces it with a toast; `display: "dialog"` opens a modal dialog right away.
The answer goes back to the plugin's window. `pom-plugin-base` is the
reference: its Plugin screen demonstrates the three commands and lists the
events received by the screen and by the native backend.

## Web UI served by the plugin (`/api/ui/plugins/:code/proxy/*`)

A plugin screen can depend on an HTTP server the plugin itself runs on
loopback - for example a whole web application. The plugin announces where it
is through the `ui.upstream` operation:

```json
{"status": "ready", "port": 43127, "token": "<secret of 16 to 512 ASCII bytes>"}
```

(`{"status": "starting"}` and `{"status": "error", "error": "..."}` become
503.) `crates/node/src/plugin_proxy.rs` mounts that server on the same origin
as the UI and explicitly matches `/proxy`, `/proxy/` and `/proxy/*path`. Both
root forms reach the upstream `/`. The explicit `/proxy/` route is required
because Axum catch-all routes do not match an empty path remainder. The trailing
slash keeps relative code-server assets under the proxy prefix. The proxy
forwards:

- HTTP with streaming bodies in both directions, and upgrades (WebSocket);
- the path after the prefix, byte for byte, and the query intact;
- `x-pom-plugin-token: <token>`, so only the node reaches the plugin's server;
- `x-forwarded-prefix`, `x-forwarded-host` and `x-forwarded-proto`, so the
  plugin builds URLs under the prefix.

The session cookie, `Authorization` and `x-pom-access-token` never reach the
plugin, and the node drops `Set-Cookie` from responses. The route is open to
every profile (plugins are local to the node) and uses the administrative
surface authorization, because the browser's scripts, WebSockets and
`EventSource` do not carry the UI's `Authorization`; in exchange, it refuses
(403) an `Origin` other than the node itself, a `Sec-Fetch-Site` other than
`same-origin`/`none` and calls between nodes.

## Plugin i18n

Each plugin carries its own texts: `{"<plugin_code>.key": "ICU text"}` per
language. `lib/pluginI18n.ts` loads the **selected** languages (the active one
and `en` as a floor, only those the manifest declares and POM supports),
validates them again in the browser (keys only in the `<plugin_code>.`
namespace, string values, ceilings) and hands them to `setPluginMessages`
(`i18n/locales.ts`), which puts them in the same `formatMessage` map without
ever overwriting a POM key and removes them when the plugin leaves. A missing,
unreadable or invalid catalog is dropped with a warning: it falls back to `en`
or the key. **POM owns no plugin text.**

## Tests

- Rust: `cargo test -p node --bin pom plugin_ui` (manifest, paths, profiles,
  catalogs, the real manifest of the `enterprise` plugin and the
  `ui/manifest.json` of `pom-plugin-base`, including list-form documentation);
  `plugin_proxy` (`ui.upstream`, origin barrier, filtered headers, HTTP and
  WebSocket against a real server); and
  `plugins::tests::host_context_and_ui_proxy_reach_a_real_plugin_host`
  (`host.configure` on start, on change and on restart of a real host, and the
  proxy through the UI router).
- Rust: `plugin_ui::tests::menu_children_description_and_screenshots_are_validated`,
  `plugin_github::tests::finds_ui_manifest_and_installs_the_selected_platform_release_manifest`
  (description and `screenshot_urls`) and
  `network_chat_tests::plugin_notices_are_validated_kept_in_memory_and_never_persisted`.
- Frontend: `lib/pluginUi.test.ts` (registry, namespaces, collisions, paths,
  profiles, menu and submenus), `lib/pluginEvents.test.ts` (envelope, SDK, DOM
  and iframe channels, replay, commands and refusals), `lib/pluginI18n.test.ts`
  (install, selected languages, missing/invalid), `routes/Plugins.test.tsx`
  (immediate manifest invalidation after lifecycle actions, GitHub card above
  activation, compatible releases with the latest preselected, preferences
  through the gear and the refusal reason), `App.test.tsx` (fallback, screen by
  profile, two equal relative routes and a direct URL under `/admin-ui`).
- Manual end-to-end done at delivery: the real `cdylib` loaded by
  `pom-plugin-host` answering `ui.manifest`/`ui.asset`, and the real bundle
  rendered in POM's `App` with the plugin's pt-BR catalog.
