# Web UI

POM's administrative web UI, a React 18 + TypeScript + Vite SPA in
`apps/frontend/`. It is the product's interface: every node serves the UI
together with the API on the gateway port (`--gateway-port`, 8080 by default),
under the `/admin-ui/` mount; the root only redirects to it. The panel can start
`localhost`-only or be exposed to the network with a token and a server
session, while the inference routes stay dual-stack for the pipeline's HTTP
fallback.

UI labels are quoted below in English (the `en` locale); the Portuguese
labels live in `apps/frontend/src/i18n/messages.csv`. Route paths keep their
Portuguese names (`/deploy`, `/models`, ...) because they are part of the
URL contract.

The name and logo the screen shows come from the root `brand.json`, copied
into the bundle by `scripts/build-brand.mjs` - the same file the binary embeds.
`Logo.tsx` draws the mark from `/brand/pom-mark.png` and writes name and slogan
from it; the tab title and icons enter `index.html` through the `brandHtml`
plugin in `vite.config.ts`. Neither has the name written by hand.

## How the UI comes up

- **The bundle is read from disk on every request**, not embedded in the
  binary (`crates/node/src/backend/webapp.rs`). `cargo build` never touches
  this directory; editing the UI only needs `npm run build` in `apps/frontend`
  (and reloading the browser). The comment at the top of `build.rs` records the
  same split: UI build and Rust build are independent steps.
- The directory is decided by `config::ui_dist_dir()`
  (`crates/node/src/utils/config.rs`): `$POM_UI_DIST` if set, otherwise
  `<project_root>/apps/frontend/dist`. The Docker image uses the variable
  because `POM_HOME=/data` is not the image's `/app`; a developer who keeps the
  bundle elsewhere points it there directly.
- Without `index.html` in the directory the node starts normally and answers
  **500** on the UI routes, with a message saying where it looked and the fix
  (`npm run build` or pointing `POM_UI_DIST`). `dist/` and `dist-release/` are
  local artifacts ignored by git.

### Compact Chat in the macOS menu bar

The macOS bundle keeps the Rust process as the owner of the gateway and adds an
`NSStatusItem` in `crates/node/src/macos_status.m`. A click opens the same SPA at
`/admin-ui/chat`, without sidebar or topbar, in a `WKWebView` of 390 x 844
points - the logical screen of an iPhone 14/15. On a screen that cannot fit
that height the panel shrinks to what fits; the 390 width is fixed. The
`Larger window` button only shows up when the WebKit bridge exists, and asks
the native process for a resizable 1120 x 880 points window, larger than the
panel on both axes. In a regular browser that button is not rendered.

The larger window title and its menu-bar tooltip use the display name from
`brand.json`; the packaged app's Dock label uses the same name. The bundle also
installs the macOS menu bar in AppKit: the application menu with About and Quit
(Cmd+Q), an Edit menu (Undo, Redo, Cut, Copy, Paste, Select All) and a View
menu with Reload (Cmd+R). The Edit menu is what makes Cmd+C/Cmd+V work: AppKit
delivers those shortcuts through the menu bar's key equivalents to the first
responder, the focused web view, and without the menu they do nothing. The app is a regular one (Dock
icon, menu bar, full screen); closing the window only hides it and the app stays
in the menu bar. Besides the menu bar:
- a right click (or Control-click) on the status item opens a menu with
  "Open POM", "Reload" and "Quit POM";
- right-clicking the Dock icon offers Quit;
- the `pomQuit` WebKit bridge, behind a power button in the top bar
  (`QuitNativeAppButton`), rendered only inside the macOS app.

Reload exists because a WebKit window has no browser reload button, and a
screen that stopped working had no way back. On the interface it reloads the
current page from the origin, fetching its code again; while a native loading or
"could not reach the gateway" page is showing, it goes back to waiting for the
gateway instead. The menu bar's Reload acts on the open panel, else the window;
the status item's acts on both.

The window renders in the system WebKit, so the interface must parse on the
oldest macOS the app supports: macOS 11/12 ship Safari 14/15, which has no regex
lookbehind (`(?<=`, `(?<!`). One in any chunk is a `SyntaxError` that kills the
whole screen importing it ("Invalid regular expression: invalid group specifier
name"). The `noRegexLookbehind` plugin in `apps/frontend/vite.config.ts` fails
the build when a shipped chunk carries one.
Quit calls the standard application termination and ends the whole process,
including the Tokio service on the background thread.

Rust chooses the dimensions and the address (`macos_status.rs`) and hands them
to Objective-C at startup: the native side does not know the node's
configuration, and duplicating the numbers on both sides would invite drift.

When the Rust service ends, the bridge stops and wakes the AppKit loop without
ending the process. So `macos_status.rs` can wait for the service thread and
return to the shell the message and exit code of an invalid configuration,
instead of exiting silently with status 0.

### Windows window and tray

On Windows, the same binary uses `windows_subsystem = "windows"`, so the UI
does not open a separate console. `crates/node/src/windows_status.rs` creates a
Tao window and a real WebView2 through Wry, loading the same `/admin-ui/chat`
and the port chosen by Rust. The notification area icon uses the POM art
embedded in the binary; the `Open POM` menu restores and focuses the window.

The window's X does not end the process: it hides it in the tray, keeping the
gateway and the administrative session alive, like the macOS status item. The
`Quit POM` menu is the terminal exit and ends the whole process. The WebView2
runtime is a dependency installed on Windows, not a helper executable shipped
by POM.

The address is `https://127.0.0.1:<gateway_port>/admin-ui/chat` (plain `http` only when the node could not make its certificate), with the port
from `pom.json`, `--gateway-port` as a fallback and 8080 as the default - the
panel never hardcodes 8080. Both WebViews use
`WKWebsiteDataStore.defaultDataStore`, so they share cookies with the panel
itself and with each other; the route sits **after** the access gate in
`App.tsx` and uses the same `HttpOnly` cookie session as the browser, with no
parallel credential. If the gateway does not answer, the native delegate shows
a localized unavailability page with a retry button, instead of leaving the
panel empty. That page's texts live in
`assets/brand/macos/{en,pt-BR}.lproj/Localizable.strings`.

On Windows, the Tokio service sends a signal after the gateway listener accepts
the port; only then does `windows_status.rs` build the WebView2. That keeps a
slow start, especially with `RUST_LOG=debug`, from opening the page before the
gateway is listening and leaving `ERR_CONNECTION_REFUSED` stuck in the window.

The item's icon is a **template image**: AppKit discards the color and paints
the alpha channel with the bar's color, which solves light and dark themes at
once. That is why it cannot be the mark as it is - `pom-icon-src.png` is an
opaque blue square, and alpha 255 everywhere would become a solid black
rectangle in the bar. `scripts/derive-macos-status-icon.py` extracts the white
stroke as alpha, drops the wordmark (unreadable at 18 points) and writes
`assets/brand/macos/pom-status-template{,@2x,@3x}.png`. The three go in as
representations of the same 18-point `NSImage`.

### GPU temperature

Both cards show **one line per GPU**: the Flow card used to show only the
machine's highest reading and the VRAM gauge of the first card, so on a box
with two cards half of the hardware vanished from the screen - and the second
one could be boiling with nobody seeing it. The mini card's VRAM is the **sum**
of the cards that report it (`lib/gpu.ts`), and cards with the same name get
`#index`. The normal, attention and critical bands use the existing `aqua`,
`amber` and `rose` tones; machines without a GPU omit the temperature section,
while a GPU whose sensor does not answer shows "temperature unknown" and
"sensor unavailable". The `GET /api/ui/node` API carries the thermal policy
together with the machine, and `GET/POST /api/ui/thermal` reads or writes its
three values.

The thermal protection panel lives in Settings > Preferences. The backend
validates the policy before persisting, requiring a minimum cutoff of 70 °C and
a maximum cooldown of 60 seconds, and the node's own runtime applies the
hysteresis and the cooldown. When the model is paused for heat, the server
state shows up as "waiting to cool down", distinct from an unavailable peer or
a failure.

### Routing in axum (`webapp.rs`)

The bundle is mounted last in the router under `/admin-ui/`; `/` redirects to
that mount and no old path needs to expose the shell. A real file wins (with
`mime_guess`), a miss with an extension is 404 ("asset not found") and an SPA
route gets `index.html`, so reloading the browser at
`/admin-ui/config/keys/api` works. `resolve()` rejects `..`, absolute paths and
`Prefix` (there is a test). Cache: `index.html` with `no-cache` (it is the only
file that changes under a fixed name), content-hashed assets `immutable` for 1
year. In localhost mode, the gateway boundary refuses root, mount and assets
coming from outside with a self-contained 403 HTML page; `/v1/**`,
`/api/generate`, `/health`, `/peers` and the other pipeline routes stay outside
the boundary.

## Build and development

- `package.json` scripts: `dev` (`vite --port 5199 --strictPort`), `build`
  (`tsc -b && vite build`, type-check through project references), `preview`
  (`vite preview --port 5199 --strictPort`), `test` (`vitest run`).
- Vite 6 + `@vitejs/plugin-react`; Tailwind 3.4 through PostCSS; React 18;
  TypeScript 5.7. `cacheDir` is pinned to `node_modules/.vite` because Vite's
  fallback silently loses the config (no PostCSS, no proxy) when a
  `git rebase`/branch switch restarts the dev server in the middle of the
  rewrite.
- **There is no sample backend**: every screen reads a real node. In
  development the dev server forwards `/api`, `/v1`, `/admin`, `/peers`,
  `/health` and `/generate` to `VITE_API_TARGET` (default
  `http://127.0.0.1:8080`; point it at a real node, e.g.
  `VITE_API_TARGET=http://<node-ip>:8090`). A detached node (`--no-discovery`)
  is the legitimate development target.
- Vitest 3: no config file; tests that need a DOM opt in per file with
  `// @vitest-environment jsdom`.

## Menu and routes

`src/components/nav.ts` holds the menu as data (`NAV`: sections and leaves with
`labelKey`, route and `end`), consumed by the Sidebar and by the router in
`src/App.tsx`. `Deployments` uses the rocket (`Rocket`) and `History` the clock
(`History`): both sections used to use `Boxes`, the same icon as `Models`, and
two sections with the same drawing cannot be told apart at a glance. The
`/deploy` leaf keeps `Rocket` without clashing with the section, because the
Sidebar draws no leaf icons. Inside `Models`, the order is `Find HF models`,
`Full models`, `Slice models`, `Shards`. The side menu can be minimized: in
that state it only shows icons, with a 64px brand header to line up with the
toolbar. Sections with leaves show a submenu on hover, focus or click; picking a
leaf navigates to its route. In the minimized state, navigation uses
`overflow-visible` without `overflow-y-auto`, because that combination makes the
browser compute horizontal overflow as auto and clips the submenus; `z-index`
does not get past that clip. The minimized choice is persisted in the browser.
`App` has no setup branch: `useAdminSurface()` probes `/api/ui/node` with
`auth: false` and only chooses between the panel, the password gate and the
local-only notice. Routes:

- `/` -> Dashboard; `/chat` -> Chat (outside `<Routes>`/the transition because
  it owns a live stream and the scroll; unmounting it to animate would drop
  tokens); `/network-chat` is the network Chat on its own page, with the same
  visual pattern as the AI Chat, and follows the end of the history when the
  person is at the bottom; `/admin-ui/chat` is the compact alias used by the
  macOS menu bar app and renders the same Chat without the administrative
  chrome. The two Chat surfaces are reached from the Sidebar's nav entries
  (`nav.ts`: `nav.chat` to `/chat`, `nav.networkChat` to `/network-chat`), and
  `NetworkChat.tsx` uses `Tabs` (`components/ui.tsx`) to switch between the
  network room and the lobby, which only an admin sees: one thread per
  candidate, picked from the candidates list, with the selected one outlined
  and named above its messages, next to Approve / Decline. A visitor sees the entry only
  when the network opens its Chat to visitors (`chat_visitors`). The
  `Network Chat` entry shows the
  number of messages received since the last read, and entering the screen
  clears the badge. On a mobile viewport, online users start hidden and show up
  through the `See online` button. The AI/NETWORK tabs are the first strip of
  the content on both chat screens, above the header and the conversation.
- `/models` -> WholeModels; `/models/remote` -> RemoteModels; and
  `/models/slice` -> SliceModels, with the jobs panel and slicing progress
  embedded
- `/shards` -> Shards; `/shards/deploy` -> redirects to `/deploy` (old
  link)
- Those four routes, plus `/transfers` and `/transfers/send`, are
  full screen (`isFullBleed` in `App.tsx`), under the same contract as
  `/deployments`/`/deployments/logs`: a fixed header
  (`FullBleedHeader`/`FullBleedToolbar`, `components/ui.tsx`) and a table or
  its own content scrolling inside, instead of the central column with
  `max-w-[1400px]`. `WholeModels`, `Shards` and the `RemoteModels` search became
  full-width tables; `RemoteModelCard`, the card the search used before, was
  removed. `Shards` shows, for each slice set, the vision / MTP link editor
  (`SidecarLinkEditor`): two pickers over the compatible sidecars of every
  machine, each option naming its machines and the full path on each, saved on
  the principal. `WholeModels` keeps the catalog's projector column and opens
  the same editor in a window. The deploy inspector offers the same pickers
  (head projector, tail MTP draft) prefilled from the link, and says whether
  the target already has the file or will copy it.
- `/deploy` -> DeployModel (a top-level entry to put slices into service)
- `/deployments` -> Deployments (the record of deployments, activating the
  default one, versions per `deploy_id`, redeploying by version or by new
  parameters, and undeploying). Full screen, like Logs, and clicking a
  version's row opens on the right the **same** inspector as the deploy screen,
  read-only, with the parameters that version started with
- `/deployments/logs` -> DeploymentLogs, a terminal console of the whole
  cluster's process log, in arrival order at the principal and filterable by
  `deploy_id`; not to be confused with the request audit at `/logs`. The
  history is capped on the principal at the last seven days or 500,000
  records, whichever comes first; `cluster_logs_min_level` in `pom.json`
  controls the minimum captured level and defaults to `info`.
- `/transfers` -> Transfers; `/transfers/send` -> SendFile
- `/logs` and `/statistics` -> `EnterpriseFeatureShowcase`, the sales page
  of the logs plugin (Request Logs, Statistics and Quota, one purchase; public
  catalog, purchase on the site in a new tab), when no active plugin declares
  the target. While neither is declared the menu shows one "Business Pack"
  entry (to `/logs`) in place of the whole Logs section and Statistics, since
  the deployment logs are kept by the same plugin. The desktop shells
  (WebView2, WebKitGTK, the macOS panel) open any link to another host and any
  new window in the system browser (`crates/node/src/utils/external_links.rs`,
  `macos_status.m`). A single manifest declaration replaces
  the fallback at that URL, including for Enterprise. Other plugin screens come
  from `pom-plugin-ui/v1` and live under `/<plugin_code>/<manifest path>`; for
  example, `plugin_code: "base"` and `/example` open at
  `/admin-ui/base/example`. Menu and matcher use the same normalized table; new
  menus show up in manifest order, right after that plugin's other entries or,
  without any, before Documentation. If two plugins claim
  `/logs` or `/statistics`, both use their namespaces and the native fallback
  stays. `/logs/statistics` remains a native alias of `/statistics`.
  Plugin menus and screens are available to all profiles; manifest `roles` and
  legacy `profiles` are validated and preserved but do not gate this POM's UI.
  Native route restrictions remain in force, so `visitor` cannot open
  `/deployments` (deployment logs included), even when a plugin claims an
  extension point. `/logs` and `/statistics` open to a `visitor` only when an
  active plugin serves them: its own node's requests, never the fallback.
- `/plugins` -> Plugins, the showcase of publishable items returned by the
  license server's `/v1/plugins`, read through the node, crossed by `code` with the
  local license and install state. Open to every profile: plugins are local to
  each node, so `admin`, `worker` and `visitor` can activate licenses, add GitHub
  repositories and install plugins on their own machine. The browser reads the
  public catalog through `GET /api/ui/plugin-catalog` on its own node, which
  sends the store no credential, and the client only accepts plugins
  with valid releases. The screen shows icon, up to four screenshots,
  description, price and release when the server provides them; missing fields
  and images have a visual fallback. The description uses the pt-BR version if
  it exists, otherwise the English one. Up to four screenshots can be enlarged
  in a full-screen dialog, with Escape and focus returned to the control that
  opened it. Expiry and activation usage per plugin come from the authenticated
  state published by the node, never from the public catalog. Plugins active on
  the node stay listed even when missing from the public catalog, with local
  actions preserved. When the required license is not active, Buy redirects to
  the public store with the `plugin_code` encoded by `lib/pluginPurchase.ts`.
  Active plugins show license expiry, last install/application and last update
  check as distinct dates; the screen reads those times from the state
  persisted by the node. The activation key shows up when any public entry that
  needs a license is not active in the local state, even if another plugin is
  already active or there is license context; missing local status, expiry and
  revocation keep the form visible. It disappears when every known licensable
  plugin is active. The `License active` badge reflects the license state, not
  the host's readiness; `active` only enables plugin actions when the host is
  ready. An install failure shows on the card with the detail of
  `PluginStatus.installation_error`; when the activation POST fails, the screen
  rereads the local status to show that diagnosis. Deactivation asks for
  confirmation. The route uses `FullBleedHeader` and internal scrolling like
  `/models`; the Plugins panel uses `overscroll-y-contain` so scrolling at the
  edge does not propagate to the page. The document stays without scrolling;
  the inner areas define their own limits. Loading, error and empty catalog
  have their own states. On `admin`, when `pom_dev_mode=true` in `/api/ui/node`,
  the column also queries the local development sources and offers
  build/install even for a plugin that has never been activated; those controls
  do not show up for other profiles or for nodes without the flag. A
  development-only install grants no licensed features, while rebuilding an
  existing licensed or GitHub install preserves its entitlement. That endpoint
  is PrincipalOnly and the UI only gets success or the step/exit code, not compiler
  logs. The
  GitHub release flow stays separate. See `docs/subsystems/plugins-ui.md`,
  section "Local rebuild in development".
- `/documentation` -> Documentation (full screen; see its own section below)
- `/public-networks` -> PublicNetworks, a full-bleed catalog of public networks
  discovered through the pkarr layer. The list is paged and searched on the
  node, over its table of every network it has heard of
  (`GET /api/ui/networks/known`, `http-api.md`): the screen paints from that
  table at once and, when the live listing (`GET /api/public/networks`, which
  records what it finds) answers, reads the page again; a row the live
  listing did not return says when it was last seen. The screen has one
  header, a search box with its own Search button and the filters folded
  behind a "+"; on a phone the table becomes a list of cards. The search box, the
  country, city and distance filters and the order (distance, name) go to the
  node; latency is measured in the browser and reorders the page. Each name
  is preceded by its discovery icon (public globe, hidden eye-off, off
  wifi-off), the same Network settings and the quick setup draw. Clicking a
  row selects it (iris band) and opens a side panel drawn like the
  Dashboard's property inspector, in small print: the network's name, whether
  it takes new members (each role, network node and inference node, with its
  icon, name and state), description, channels (links) and rules (always
  open) - nothing the row already shows, and no keys or fingerprints - with
  the network's actions at the bottom (quick setup, ask to join, lobby,
  leave, more details, trust document, remove). "More details" opens a near full-screen window with every field,
  technical ones included. A network whose record says `lobby_chat` (its
  Chat is on) offers "Talk" and a "Lobby" button. Every lobby
  (`LobbyChat` with `layout="page"`) replaces the list and the page header on
  the same screen; open lobbies stay in the same folder tabs as Network Chat,
  shown over the list and over every lobby ("Networks" is the way back, each
  lobby tab has its own close button), and keep polling, so several
  conversations run at once. Without
  `lobby_chat` there is nothing to talk in: the button is "Ask to join" and
  the full-screen lobby shows only the hardware card and the request. "Ask to join"
  opens that request alone in a dialog, without a tab. A network whose lobby
  answers `lobby_disabled` refuses admission as well, so the dialog or tab
  shows only the notice that it cannot be reached for admission now. Two
  kinds of rows stay at the top of every page whatever the order, filtered
  client-side by the same search, country, city and distance as the list
  (`GET /api/ui/networks/pinned`): the
  network this machine is in, tagged "Your network", shown even when it is
  not public; then the private networks added on this machine, tagged
  "Private - added here" with an amber band. "Add network" in the header
  imports one from a JSON file holding its trust document
  (`POST /api/ui/networks/custom`), and an added row can be removed from the
  list. Only the row of this machine's own network exports its trust
  document as `<name>-trust.json`, the file another machine imports; another
  network's document is not offered for download. While the own network's
  discovery is off or hidden, its principal (`authority_level: primary` and
  `can_edit`) gets "Quick setup" in the header, on the row and in the side
  panel: `components/NetworkQuickSetup.tsx`, a step-by-step window
  (discovery, information, who can join and accepted roles, location, review)
  that saves through `POST /api/ui/network/settings` like Network settings. An added network is
  joined like a public one, through its lobby; its admin only needs discovery
  on "Hidden", since the lobby follows discovery. The network Chat is not
  needed to ask for admission; without it the lobby window shows no
  conversation, only the hardware card and the request panel. Network
  settings asks the quick setup's questions in its order and drawing
  (discovery, information, who can join, location, then this machine's relay,
  saved at once), with plain text fields and no Markdown preview. The quick
  setup also has the relay step, suggesting "Relay for any network" to
  strengthen the distributed network; it is saved after the network. Both
  leave the relay out while the relay report says `reachable: false`: behind
  a NAT the machine cannot relay for anyone. A visitor also gets "Relay
  anonymously for this network" (`relay.anonymous`, see `p2p-network.md`), and
  an admin sees how many visitors relay that way, even behind a NAT. "Who can join" and "Accepting new members" appear
  only with discovery on or hidden; "Who can join" has "Anyone without
  approval" (`auto_approve`), which admits every request on arrival and
  carries its warning in the choice. The location (city, pin, geographic
  label) is shown only on a public network. The table groups by country/city and sorts
  by the approximate distance authorized in the browser; it shows the computed
  metrics when the local cluster matches the network and marks as unavailable
  what the public catalog does not provide. The join button reuses the trust
  screen without a public network secret. On the row of the network this node
  already belongs to, Join is replaced by "Leave network" for a member and by
  no action at all for the principal. A network whose `accepted_enrollments`
  is empty shows a disabled "Closed" button; with a partial list Join stays
  enabled and the join dialog disables the roles that are not
  accepted. The principal edits the list in Network settings ("Accepting new
  members"). Every listed network gets a button that opens
  `components/LobbyChat.tsx`: that network's lobby through this node, with
  the hardware card the network will see and the admission request. The
  conversation is shown only when that network has its Chat on; otherwise the
  window is just the card and the request. The same component
  is used by the deprecated setup wizard, whose join step lists the federated directory
  (`PublicDirectoryPanel`), so a machine that has joined nothing can ask first;
  the list comes from the installation's relays and from the relay key or
  node address typed in the panel.
  Members read and answer in the "Lobby" tab of `/network-chat`, which lists
  each candidate with its declared hardware (`LobbyHardwareCard`). The switch
  is "Lobby chat" in Network settings. A network with `public_stats` fills the
  nodes, VRAM and models columns of a row this node is not a member of
  (`lib/publicStats.ts`, asked from that network's principal through this
  node); latency stays unavailable there. The switch is "Published stats" in
  Network settings. The menu entry and the screen are now called "Networks".
  Joining is asked, not taken: the row's main button opens the lobby window,
  whose side panel has "Request admission" with the role to join as, shows the
  request's state, and offers "Join now" (behind a confirmation) once an admin
  approved. The bell lists machines asking to join this network, with role,
  direct or relayed connection, latency and hardware, and a link to that
  candidate's thread, where an admin approves or declines after talking;
  once decided, the candidate's window hides the conversation; on the candidate it lists the answer, with "Join now" when approved.
  After "Join now" the node restarts into the network and, until it holds a
  membership token, has no models to show. The interface says so instead of
  "the network serves no model": `GET /api/ui/connection` gives the phase
  (`discovering`, `authenticating`, `connected`, or `standalone` for an admin),
  a "Connecting…" badge sits in the top bar and on this machine's row here,
  and the Chat shows "Connecting to <network>" with the steps (find the
  network, get access, see the models). A step that lasts 60 s says why it may
  be stuck, with the admin's last refusal. Once in, the Chat stays on "see the
  models" for up to 90 s, polling the models every 3 s, and the badge stays on
  a worker or visitor: how many machines it sees, amber when no admin is among
  them, or "Network out of sight" when it sees none. The same happens after a
  member reboots.
  Network settings calls visibility "Discovery" and offers off or on (listed);
  the announced-but-not-listed value stays selectable only where already set.
  It no longer offers the open admission mode.
- There is no `/setup` route: the address falls to the not-found page. See
  "First access wizard" below.
- `/config/network` -> NetworkSettings; `/config/network/participants` -> the
  participant list for `admin` and `worker`; `/config/preferences` ->
  Preferences; `/config/preferences/admin` -> administrative preferences;
  `/config/about` -> About, the last item of Settings, with links to the
  repository's AGPL-3.0 `LICENSE` and the official GNU page, an LGPD
  privacy/security notice and site links, a contact form
  (`https://pieceofmind.cloud/contact.html`) and official repositories; it also
  shows the running POM release and the on-demand check of POM's GitHub
  releases (the site as fallback). When an update is available, the screen
  itself offers to apply and restart, or says why this node cannot (a
  container, a download that still needs an invitation); it also shows the catalog of direct dependencies grouped by license;
  `/config/trust` gathers this machine, sharing and connection keys;
  `/config/keys/api` -> TokenKeys; `/config/storage` -> ModelPaths
  (`/config/folders` remains an alias). When `POM_MODEL_DIRS` or `POM_SHARD_DIR`
  sets a folder (`model_dirs_env`/`shard_dir_env` in `/api/ui/paths`), the
  matching field is locked, with the variable's name in a badge and a note:
  editing there would have no effect. Saving a folder that does not exist yet
  creates it (absolute paths only); a path to a file is refused. The setup wizard does not show up in the
  menu, although its test route stays available.
- `*` -> NotFound

On a public network, the local `NetworkSettings` is a read-only replica for
`worker` and `visitor`. The entry directory only carries the data needed to
discover the network; after the principal's authenticated `Announce` arrives,
the backend fetches the full document, including name, organization,
description, rules, channels and visibility. The screen must not treat
capabilities announced during boot as a ready pipeline: the catalog only marks
full coverage when each peer confirmed `/health` or is `idle`.

On the `visitor` profile, the Sidebar stops being a list of screens:
`ChatSessionList` takes the main menu with `New conversation` and the saved
conversations. The new button stays outside the scrollable area, the list uses
`min-h-0` + `overflow-y-auto` and the active session gets a highlight and
`aria-pressed`; so many conversations neither hide the network nor make the
menu grow past the window. The network is the only extra main destination.
Documentation and Settings (the API key allowed to infer) are footer
projections next to the flags. `ChatSessionProvider` keeps the Sidebar's
selection and the Chat in the same session. Administrator and node still get
the normal `NAV` projection.

### First access wizard

**This wizard is deprecated and unreachable.** It is not the first run wizard
("First run wizard and guide" below), which runs on a configured node. A node
without a network creates a local one and serves the interface, and that is
the only way to start.
`App.tsx` neither imports nor routes `routes/Setup.tsx` or
`routes/SetupWizard.tsx`, the shell probe has no "setup" answer, and the node
never starts the `/api/setup/*` router (`http-api.md`, "Setup routes are not
served"). The files and their tests are kept until the wizard is removed for
good; the rest of this section describes that dormant code. Do not link to it.

`routes/Setup.tsx` was the real wizard, served by the loopback router
`/api/setup/*`. It only asks what cannot be decided later
(`routes/setupWizardFlow.ts`):

- **Administrator, 3 steps:** Start (role and machine name, with a hardware
  summary), Network (name and **Private** or **Public**; the location only
  for public, because it goes to pkarr and the directory) and Invitation
  (connection key, when private, and the `trust.json` to download or copy). The
  preferred address is optional, starts empty under "Advanced" and is only a
  hint (IP or DNS name); `trust.json` carries no IP - a joining node finds the
  primary by its key through pkarr. There is no
  "local-only network" or "who can access" question: private requires the key
  and the `trust.json`, public requires the `trust.json`.
- **Node and inference, 2 steps:** Start and Join. Join looks for the
  administrator through local discovery and receives the root and the key
  without typing anything; pasting the `trust.json` and the key stays collapsed
  as an alternative. The city of a public network comes in the `setup` block of
  `trust.json`.
- **Local discovery during the invitation (optional):** while the
  administrator is on the invitation step, each computer looking for the
  network shows up with an animation and what it reported about itself - name,
  system, IP, CPU, RAM, GPUs and VRAM - and goes from "joining" to "connected"
  when it receives the root. Finishing with nobody is normal.
- **Defaults instead of questions:** model and slice folders, disk space,
  RAM/CPU reserves and the thermal limit come from the machine; the UI is open
  to the network with the password `admin`; the workspace is `~/pom_workspace`.
  Everything can be adjusted later, inside POM. While the password is the
  default one, the gate does not ask for it (`useAdminSurface` logs in with it
  when `/api/access` says `default_password`) and the bell
  (`Notifications.tsx`) carries a notice asking to change it in Preferences.
  The notice is sent once per start of the node (`boot_id` of `/api/access`,
  state in `lib/defaultPasswordNotice.ts`): opening the bell reads it, it can
  be dismissed, and it comes back unread after a restart. Ignoring it blocks
  nothing.

The shell uses `.pom-login*` and `.pom-setup*` (`index.css`): the floating dark
panel with the `assets/pom-mark-dark.png` mark, the `BrandLanguageSwitch`
language selector and the step track drawn as connected nodes with a pulse of
light traveling - the same animation as the gate, which is also the loading
indicator. Inside the panel the dark theme tokens always apply
(`.pom-setup-scope`).

The wizard does not require HTTP(S) ingresses of the federated directory for a
public network. When configured through `POM_PUBLIC_DIRECTORY` or later in
administration, they are persisted in the trust document.

`routes/SetupWizard.tsx` and `components/setup-wizard/*` are still only the
visual reference mock. They are not used by the real first access.

### Remote models

`RemoteModels` loads, on opening, the trending GGUF models from the public
Hugging Face API (`/api/models?sort=trendingScore&limit=20&filter=gguf`),
without sending credentials. The provider search still uses the authenticated
node; the login status stays compact inside the bar, next to the Search button.
After the search, the detail resolves the license through the provider and
shows the gated model marker before the download confirmation; so the screen
neither assumes a license nor starts a protected operation without explaining
the required authentication.

The screen title is "Find models on Hugging Face". The detail queries
`GET /api/ui/model-provider/storage` and shows the model folders resolved from
the configuration before starting. The download picks one of those folders; the
job persisted in `GET /api/ui/jobs` records the final path and
`ModelDownloadsPanel` shows progress, cancellation and retry. That panel also
shows up in `/transfers`, next to file transfers.

In `/config/preferences`, `HuggingFaceSettingsPanel` offers the optional token
for private and gated models. The field is `password`, starts empty and only
shows the mask returned by the backend. Saving or removing calls
`/api/ui/model-provider/config`, invalidates the status and makes the provider
reread the credential without restarting the process; the precedence follows
what is documented in `http-api.md`.

### Profiles: what each one sees

The node announces a profile persisted in `pom.json`: `admin`, `worker` or
`visitor`. The administrator operates the cluster; the worker takes care of its own
machine; `visitor` has a reduced view to talk to the models, read the
documentation, manage its own API key and ask to join the network's processing.
Any of the three can activate and install plugins on its own machine.
`GET /api/ui/node` is the source the browser uses and the toolbar shows the
label through i18n. The same snapshot carries `pom_dev_mode`, true only when the
process was started with exactly `POM_DEV_MODE=1`. `DeployModel` and History
completely hide `Configurações POM (DESENV)` ("POM settings (DEV)", the
inference engine and idle unloading) when the flag is false; the whole sampler
(min-p, frequency, DRY), the seed and where MTP runs are default settings
shown to everyone, in the deploy inspector and in History's redeploy panel
(the same inspector, editable, on the right side of the list). On the `/deploy` screen, Tensor, the
Simple/Complete strategies and the "Back to the node's plan" button are also
hidden when `pom_dev_mode` is false. This is only local visibility: the UI and the
backend keep the deployment values received over the network, even when the
node cannot edit them.

On the Dashboard, the reading is split by responsibility. The `admin` sees the
cluster aggregates, such as coverage, machines, active model and in-flight
load. The `worker` and `visitor` profiles only see the local machine's facts:
served model, layer range and in-flight requests. For `visitor`, global coverage
is not an operational indicator of its screen, so it is not rendered as a
pipeline alarm. If `/api/ui/node` is unavailable, those fields show
unavailable; missing data never becomes an empty model, an incomplete pipeline
or zero.

"Requests on this node" measures the number of inference requests currently in
flight in that machine's `LoadTracker`. Zero is a valid measurement only when
the machine itself answered zero; a read failure stays unknown. Likewise, a
peer's load on the deployment map comes from that peer's own `/api/ui/node`, not
from the coordinator's tracker.

`src/lib/access.ts` combines the native routes with the profiles from the
plugin registry (`lib/pluginUi.ts`); a native route missing from the table is
open to `admin` and `worker`. Today `/deploy` and `/deployments` are
administrator only. A plain node keeps the Models section (search, full
models, slicing, shards) for its own files, but deploys nothing, not even a
full model on itself: to deploy, it returns to its own network, where it is
the admin. The native showcases `/logs` and `/statistics` stay
reachable for discovery; plugin screens follow the profiles declared in the
manifest. For `visitor`, the list of allowed and denied native routes lives in
`INFER_ROUTES` and `INFER_DENIED_ROUTES`. In the Chat, run telemetry and its
summaries are also hidden. The key matches a **whole segment**, so `/logs` does
not cover `/logs-antigos`.

In the Chat, `visitor` gets its own slice through
`GET /api/ui/infer/heads?model=...`: the **Inference entry** box shows the
selected head and the other ranges starting at L0, with RTT, age and in-flight
load. If the candidate's live read does not arrive, the load shows as
unavailable, not zero. The policy is written on screen: coverage first, a
weighted score of fresh RTT and load, and a tie-break by id. An RTT without a
fresh measurement shows up as a fallback, and a model without a head or without
a complete path produces a readable warning instead of a raw failure. That
slice does not open the general peer list nor offer control authority. When the
combo changes, the Chat sends the chosen `node_id` as `inference_head` on the
next request; the pipeline and telemetry use that head, instead of reapplying
the RTT/load score.

`Logs` queries `GET /api/ui/requests` with the operator's connection key. The
compact bar applies filters by the API key's non-secret identity, prompt text,
node and period, lists the most recent entries first in pages and loads the
next one when the sentinel enters the viewport. The statistics screen queries
`GET /api/ui/requests/stats`, which accepts the same filters, including the
user label and the model, and aggregates on the server by token, node, day and
peer, plus input/output tokens, inference, TTFT, prefix cache and terminal
states. Clicking any non-interactive area of a row opens the inspector, keeps
the magnifier and highlights the selected row with an accent color distinct
from hover. The inspector's header repeats the row's details. SENT shows the
last user message as the new prompt and the rest of the history as context,
including assistant/tool answers that show up after it, in two rendered
Markdown surfaces of split height; Response uses the same read-only viewer,
without an edit bar, HTML, images or links with an unsafe protocol. The
displayed content is capped at 64 KiB to keep responsiveness. The global
preference between rendered Markdown and raw JSON lives in `localStorage` and is
restored on future visits. The Statistics screen uses the same aggregated
source, but is not a tab inside Logs. Each Markdown area also has an icon-only
expand button in the top right corner; reading opens in a large window,
follows the global toggle and keeps the same sanitization and credential
redaction. REQUEST and TELEMETRY keep the full JSON; TELEMETRY shows the request path,
the plugin's visual telemetry panel (the same one the Chat opens) and the raw
JSON. The loading indicator takes a slot to the left of the errors badge,
keeping the badge fixed on the right. Copying redacts credentials, including
the `key` of records in `api_keys` when the value has a known secret format,
without hiding innocent `key` fields. `in_progress`, `incomplete`, `partial`,
error and abandonment get distinct visual states. The identity shown is the id
and label configured for the key; no secret is asked from the endpoint or
rendered in the browser.

**A single table, read in two places.** `App.tsx` queries it to block the route
(`Forbidden` in place of the screen) and `navForProfile` (`components/nav.ts`)
to prune the menu. Two separate lists would give the two classic defects: a menu
offering what the route refuses, or a route open to a typed address the menu
already hid. A section with leaves disappears when **no** leaf is left - "Logs"
disappears entirely for the `worker` profile, "Settings" would stay up if only
one leaf closed.

**This is not the only authorization.** The profile comes from the node in
`pom.json` and the browser uses it for presentation and navigation; the backend
enforces the same profile table (`ROUTE_RULES`, see `http-api.md`) and the
credential is still checked at the gateway (see *Authentication*). The backend
also enforces `visitor`'s local view: `/api/ui/cluster` and
`/api/ui/cluster/models` do not fan out to peers, `/admin/nodes` answers 403 and
another machine's details answer 403. There is no profile switch in the UI.

On the `/config/network` screen, a `visitor` sees its own machine's data. Nothing
changes a role in place, in the interface or on the node: the role is decided
when the network admits the machine, so the card only explains that serving
models takes leaving and asking for admission again as a network node. A
`node` likewise has no way to step down to `visitor` without rejoining. The principal
administrator (a secondary admin sees this screen and the rest of the
interface as a node does, plus the deploy screens: the profile context
carries `secondary`, and principal-only controls check it) can promote a
node (never an inference node, which the
candidate list leaves out; the "Cluster version" panel on this screen and on
Administrators shows the trust chain version this node holds, marks it
outdated when a peer announces a newer one and syncs by itself, and the
participants list shows each machine's version, amber when it differs) with `POST /admin/nodes/{id}/promote-admin`;
the gateway requires effective principal authority, and the target validates
the Ed25519 grant in the chain of trust. The `connection_key` stays only the
transport credential between nodes.



`/config/administration` shows the principal, the secondary admins, the
promotion chain and the successor's eligibility. Only the principal can promote
or demote. Resigning shows the successor determined by the last promoted one
and explicitly refuses the cases with no secondaries or no active secondary.
The offer to transfer the principal role is answered by the successor in the
bell; until acceptance, nothing changes and the principal stays principal. The
page and the bell consume presentation contracts, without carrying key,
signature or grant storage decisions.

`/config/trust` is the machine's **trust root**, and it is the work that
comes before everything above. It shows the state - **Anchored** or **Not yet
anchored** - at a glance, and, when there is a root, the SHA-256 fingerprint in
groups of eight, with a copy button, next to the cluster and the root's
machine. That is the information the operator carries to check on another
node; it is public and grants nothing.

On a node with no root yet, the same screen shows the other side: a field to
paste the content of the trust file coming from the admin and another to
**type** the fingerprint the operator read there. The second field is never
filled from the first - deriving it would cancel the check, which is the only
thing that makes the pasted file worth anything. Outside loopback the form gives
way to the explanation of why installing is local, but the decision is never the
screen's: the server refuses missing loopback, an already anchored node and a
fingerprint that does not match, and returns a stable code the screen
translates (see `http-api.md`, "Anchoring the root from the UI").

**It is open to all three profiles on purpose.** Before anchoring, a node with
`profile: admin` has no effective admin capability and does not open the
administrative surface - putting this screen behind the administrator profile
would make it unreachable exactly in the state it exists to serve. And whoever
receives the anchor is usually a `worker` or a `visitor`. That is why
`/config/trust` is not in `RESTRICTED` (`lib/access.ts`), shows up in the
Settings menu and is promoted to `visitor`'s main menu.

The `Topbar` header keeps the node's identification, profile, notifications and
the native window shortcut; the `This node` button is not rendered. The node's
short id is not shown there: in its place an amber "Behind NAT" badge appears
only while some peer is reached through a relay that carries the traffic, the
hole punch it coordinated having failed (`ClusterView.relayed_peers`, from
`Network::peer_route` kind `relayed`). Being behind a NAT whose hole punch
worked shows nothing. A click opens a dialog naming those machines and their
relay kind, what it costs (a detour, about 2 MB/s on public relays) and how to
give the machine a direct path (`components/NatBadge.tsx`). The
Sidebar's TLS indicator (`components/TlsIndicator.tsx`) is also hidden for
every profile. The policy is still available in Administrative preferences; no
TLS configuration or behavior changed.

On the `TokenKeys` screen, node and infer only see their own node's keys, do not
pick another node and keep creation tied to that identity. The admin can pick
the node when creating, sees its full tokens and gets other nodes' tokens
masked. The admin's tokens come first in the table. On infer, generating a new
key uses the current endpoint and then revokes the previous one.

Sidebar and Topbar live in their own `ErrorBoundary`, so a broken screen does
not take down the menu. The Sidebar's **header** only carries the brand lockup,
in an edge-to-edge band (`LogoFull variant="band"`) closed by a divider line.
The **footer** carries the language as **flags** (`brand.flags`, from
`brand.json`) instead of "EN"/"PT-BR" - with the language name in
`aria-label`/`title`, because a flag is not a language for a screen reader -,
the POM release the node runs (`NodeInfo.pom_version`, `v0.5.6` as GitHub tags
it; `· <build_commit>` after it only with `pom_dev_mode`) and the light/dark
theme. When `/api/ui/pom-update/status` reports a newer release the version
turns amber and a "New version" link above the row names it and opens About,
where it is installed or told why it cannot be. Network > Participants shows
each machine's announced `pom_version` (amber when it differs from this node's,
"version unknown" for a node that announces none; the build only in development
mode). The PipelineRail is still rendered hidden on
the Dashboard (`hidden`, for a future surface).

On `visitor`, the footer puts Documentation and Settings next to the flags; the
main network item stays in the history column to keep access to the request to
join processing. Those positions are `NAV` metadata, not a second list of
routes.

## Frontend architecture

`src/` has four subfolders, each with its own `AGENTS.md` (authoritative map):

- **`src/lib/`** - HTTP client, react-query hooks, `localStorage`
  persistence, chat stream/state, formatting, toasts and all the types.
  `api.ts` is the only fetch path: a **relative** path (the node serves API and
  UI on the same port, the browser never learns a peer address), signed with
  `Authorization: Bearer <connectionKey>`, and also the SSE reader
  (`streamSse`). `api.getConditional()` keeps the snapshots of
  `/api/ui/cluster`, `/api/ui/jobs` and `/api/transfer/offers` by `ETag`,
  accepting 304 without reparsing JSON. `queries.ts` concentrates the polled
  reads: `useNode` 4s (keeps the `connection_key`), `useCluster` 5s, local
  catalog 30s, cluster catalog 60s, `useJobs` 2s while a job is running and 10s
  when idle, `useTransfers` 2s, `useOffers` 2s while there is an offer and 10s
  when empty, admin nodes 8s, request log 3s. The catalog polls every 2s while
  `metadata_complete` is false, then goes back to the normal interval.
  `optional()` only swallows 404 (an old node without the route is not an error
  to show). `types.ts` is the single source of the shapes the node serves
  (comments point to the Rust).
- **`src/components/`** - chrome (`Sidebar`, `Topbar`, `Notifications`, `nav`)
  and shared widgets. `ui.tsx` is the design system in one file on purpose
  (panels, buttons, inputs, badges, meters, states) with CSS token classes, no
  runtime styling. Cluster visualizations: `ModelNetworkCanvas` (a pan/zoom SVG
  graph with RAM/VRAM/load and main/parallel/inactive edges, positions
  persisted per deployment fingerprint; the card also summarizes how many layers
  landed on the GPU and how many stayed on the CPU) and `PipelineRail` (the path
  the request travels). Guards for remote/destructive actions:
  `Confirm`/`Modal`, `ReleaseSlice`, `RemoveNode`, `LeaveNode`,
  `UndeployPipeline`, `AddNodeDialog`.
- **`src/i18n/`** - en + pt-BR in `locales.ts` (a flat table with
  `Message = (values, locale) => string`, pluralization through
  `Intl.PluralRules`). `provider.tsx` exposes `useI18n().t` and the
  `LanguageSelect`. `index.html` has a pre-paint script that stamps `lang` from
  the stored locale/the browser locale, so the first render is already in the
  right language. Text written by the node (errors, refusal reasons, job
  steps) is English at the source and is shown as it arrives, in both locales.
- **`src/routes/`** - one screen per route, all through react-query talking to
  the node's HTTP gateway; none keeps cluster state in the browser. The main
  screens are described below.

## Authentication

Before reaching the handlers, the gateway applies the administrative gate to
`/api/ui/**`, `/api/transfer/**` and `/admin/**`, and the browser sends the
person's token to all three (`isAdminSurfacePath`): the session cookie is only
written when a session is issued, so a session older than a cookie path never
reaches it, and the transfer polls failed with "missing administrative access
password" while the rest of the screen worked. In local mode it checks the connection's real
address and only accepts loopback; in network mode it requires
`X-POM-Access-Token`. The person's token is different from the
`connection_key`: the first opens the administrative UI, while the second
authenticates nodes and pipeline clients. Control calls between nodes use the
second with an internal marker of their own, which the browser never sends.

`GET /api/ui/node` only returns the `connection_key` for private networks,
after the gate. Inference exclusively uses the dedicated API key
`pom-[node_id]`, obtained at `POST /api/ui/chat-key` and sent as `X-API-Key` on
each `/api/generate` or `/v1` call. If it is revoked, the next attempt creates a
replacement and repeats the message once. The first access uses the setup
router (no `pom.json`), which serves hardware, models, the administrative root
preview, the administrative password, root installation, checks and
`POST /api/setup/save`. The save installs the separate trust root and writes
`pom.json` before the respawn. The respawn creates an independent session on
Unix, inherits the original arguments and redirects stdout/stderr to
`<project_root>/pom.log`, so a startup failure after the save can be
diagnosed. On Windows the binary creates no console, so tracing also opens
`<project_root>/pom.log` directly; the PowerShell `*> file` redirection is not
the reliable source in that mode. For a native reproduction, use
`RUST_LOG=debug` and collect that `pom.log` together with
`infer_server_<port>.log`. The wizard neither suggests nor requires
`advertise_addr`: the interface enumeration (`preferred_local_advertise_ip`)
only reports whether there is a public IPv6 (`public_ipv6_available`), and the
announcement keeps redetecting the addresses on every cycle. A configured node's answer publishes
`addr_candidates`, `endpoint_id` and the tunnel capabilities, used by the TP
deploy to carry the right address to each rank.

`GET /api/public/networks` combines the federated directory's cache and
ingresses with pkarr discovery when the active network is public. The
canonical geo label (`com.<country>.<city>.<node>`) is a field of the record,
never the pkarr key. The local index persists labels already observed and
accepts `geo_prefix`/`geo_suffix`, allowing candidates to be filtered before the
HTTP probes. The `/public-networks` page is full-bleed, like Logs, and shows a
table with name, organization, place, reachability, nodes, latency, VRAM and
models. The place (city, state, country) is **not** what the network wrote: the
node looks up the coordinates each listed network publishes in its own offline
city database (`gateway::resolve_places`, `geo::resolve`) and rewrites the
fields before answering, so the country and city filters work on those names
and a network without coordinates is listed without a place. The discovery
label stays in the details window. The toolbar sorts by distance (default),
by latency or by name and filters by distance (100, 500 or 2000 km).
Distances are computed in the browser from this network's own location, or
from the browser's location after the operator presses "Use my browser's
location" - the screen never asks for it by itself and the position never
leaves the browser. Latency is measured only on request ("Measure latency",
or choosing the latency order): the node asks each listed network twice
through the lobby connection (`GET /api/ui/network-chat/lobby/latency`) and
the column shows the smaller round trip, "no answer" for a network that stayed
silent, and the average between its own machines for the local network. **No IP is listed**: reachability comes from
`GET /api/public/networks/locate`, which the node resolves from the publisher's
key through pkarr (`direct`, `relayed` or `unresolved`). A check that could not
be made - the node refused the key or did not answer - shows as "Could not
check" (`unknown`), never as a network missing from the DHT. The details show the
publisher key and the dedicated relays (`relay_hints`). The local cluster's metrics are computed from
`ClusterView` when the network matches; the other fields show as unavailable
because the current public catalog does not provide them. The trust document
travels inside the signed federated record for a join without a secret. A
private node queries the directory over outgoing HTTP(S), but does not use the
private network to look for candidates. Joining a public network ends the current private network; two
simultaneous P2P networks are not supported in v1.

The UI **profile** (*Menu and routes*) is not part of this math as a
credential: the browser uses it for navigation, and the backend enforces the
same profile table independently. The administrative routes use the connection
key; `/api/generate` and `/v1` exclusively use the dedicated inference API key,
whatever the profile on screen.

The network Chat generates message and typing ids with `src/lib/id.ts`. It uses
`crypto.getRandomValues` when `crypto.randomUUID` does not exist, so the
composer keeps working on a LAN HTTP deployment, without depending on a secure
context. The message window follows new entries when it is close to the end;
when scrolled up, it keeps the position until the person goes back to the
bottom.

The Dashboard keeps the indicators and actions in the compact top bar. When
there is an active deployment, the properties button is only an icon inside the
map, in the top right corner, lined up with the zoom and Fit controls.

With several models live, the model picked in the map's bar is the one the top
bar's Add and Undeploy act on, and the one the properties panel shows: the
Dashboard owns that choice (`ModelNetworkCanvas` `model`/`onModelChange`,
resolved by `selectedCanvasModel` in `lib/networkGraph.ts`). Before, both
buttons always acted on the node's active model, whatever the map showed.

## Modal windows: portal and Esc stack

`Modal`/`Confirm` (`src/components/Confirm.tsx`) go through `createPortal` to
`document.body`. It is not a preference: `position: fixed` stops being relative
to the window when some ancestor has `transform`, `filter` or `backdrop-filter`
(that ancestor becomes the containing block); the top bar and the Dashboard
header have `backdrop-blur`. `Modal` keeps a stack of the open ones and only the
top one answers Esc, because one window can open another (the leave-the-network
confirmation is born inside the card). There is a test (`Confirm.test.tsx`) that
asserts the cause (the window is a child of `body`) because jsdom does not
compute layout.

## The Dashboard: pipeline speed and cost

In the Flow view, each machine card shows, besides the edges' RTT, the slice's
measured TPS - decode and prefill -, read from `peer_perf` of
`/api/ui/chat/telemetry` (the same 15 s query that brings the RTTs). The
gateway box, at the end of the pipeline, adds network and compute for one token
per route and shows the equivalent tok/s of the best route; while some peer of
the route has no measured speed, the total is only the RTT, as before. The
numbers come from the speed test each peer runs when it starts serving and
from the average of real traffic (see `parallelism.md`, "Choosing the pipeline
by delivery time").

## The Dashboard: Flow and Details show the same machine

`PeerDetailCard` (`src/components/PeerDetailCard.tsx`) is the card of one
machine, and there is **only one**: the Details tab stacks one per machine and
double-clicking a card in Flow opens the same component in a
`PeerDetailDialog`. Duplicating the presentation in both places is what is
being avoided.

The card also carries the machine's **KV cache** (`components/KvCache.tsx`):
the disk its snapshot directory uses and the number of snapshots
(`node.kv_cache`), the host-memory cache when a model is loaded
(`cache_ram` from `/health`), and "Clear cache", which empties both on that
machine only through `POST /api/ui/kv-cache/clear` after a confirmation. The
disk line shows with or without a loaded model, because snapshots of models no
longer served are exactly what accumulates. A node that does not publish the
field yet draws nothing.

That card is **the administrator's**, and `canSeePeerDetails`
(`src/lib/access.ts`) says who sees it. It carries each machine's internal
state and the actions on it - release slice, remove from the pipeline, take off
the network -, which are cluster operations and not the node's own. Outside
`admin` only Flow remains, and that is why **the view selector does not show up
either**: where there is a single view there is nothing to choose. Both doors
close together - `ViewSwitch` disappears from the Dashboard and double-clicking
the canvas card stops opening `PeerDetailDialog` -, because hiding the selector
and still opening the card through the drawing would be the same screen in two
states. The rule sits next to the route table because it is the same question:
what this profile sees. For the admin, without `ClusterView.active_model` the
initial view is Details and the Add button does not show up; with an active
model, the initial view is still Flow. A manual view choice wins over that
default.

Details lists the **whole network**, not the pipeline: `networkPeers`
(`src/lib/networkPeers.ts`) separates who serves the active model in layer
order (`pipelinePeers`) from who is connected without a slice (`others`, by
name). A machine without a slice has an empty `model` with zeroed
`total_layers` (`hasSlice`), **not** a "0-0" range, and the card writes no range
at all in that case.

The machine's actions live in a **toolbar at the top of the card**, next to the
MTP badge: free memory (`ReleaseSlice`, only on the machine itself),
`RemoveNode` (leaves the model, only when it serves) and update/restart when the
peer announces `dev_update`. Leaving the whole network is not offered on the
card: the machine does that from its own My Machine screen. They
used to be scattered across the bottom of the card, one per section, and
restarting a machine meant scrolling to the end of everything it reports.

The `AddNodeDialog` queries `POST /api/ui/deploy/node/options` after each
machine change. So the selector shows the slices and the whole model the target
really has, plus the controls for layers on the GPU, the KV-on-GPU-versus-RAM
boolean and that machine's K/V types. POM does not expose an exclusive GPU
selection for the KV: when on, it follows the device of the weights' layer.
Confirming calls `POST /api/ui/deploy/node` only on the target and keeps the
other peers' inference. With an active `deploy_id`, the change creates a new
version; selecting a whole GGUF next to slices produces the `mixed` label in
History and two possible entry heads.

`RemoveNode` also gets the `deploy_id`. A partial removal creates a version
with the remaining machines and keeps the deployment active. The last removal
creates an empty version, which stays in History for auditing, but no longer
shows up as a serving deployment.

The card is dense on purpose, to fit one screen: what is useful without having
to be always visible lives behind an `[i]` (the reason for "no slice", the
`gpu_note` warning, the fit note, the model path, pid and restarts), and
"llama.cpp load parameters" and "Processes on the GPU" are closed blocks that
open. Disks are **one gauge per mounted partition** (`disks` from
`/api/ui/cluster`), because a machine with NVMe and HDD, or with separate `/`
and `/data`, showed up as a single volume - and whoever decides where a 40 GB
GGUF fits did not see the disk where it would fit. A node that does not publish
the list yet falls back to the project volume. Next to "Add node" there is
`UndeployPipeline` when a model is live, but both controls are only mounted
when `/api/ui/node` reports the `admin` profile. The server also refuses
`POST /api/ui/deploy/node`, `POST /api/ui/deploy/node/remove` and
`POST /api/ui/undeploy` for the `worker` and `visitor` profiles, with the message
localized by the UI. `PipelineRail` still exports `pipelinePeers`/`trackOf`,
used by the Dashboard and DeployModel.

On a serving machine, the card also carries **"How the model was loaded"**
(`ModelPlacement.tsx`): a strip with one cell per layer - green fully on the
GPU, amber split, grey on the CPU (the colors come from the product's tokens;
the strip came out invisible while it asked for `bg-lime-400`, which does not
exist in this palette) -, the weights on each side in bytes, where the KV cache
ended up and the output head. It is the measured answer, not the
configuration: a layer counts by its **bytes** in each buffer, so the experts of
a MoE pushed to the CPU show up as a split layer instead of an offloaded layer.
The data comes from `/api/ui/peer/:node_id/runtime`, which forwards that
machine's `/health`, and the fetch only happens with the card mounted
(`usePeerRuntime(nodeId, enabled)`) - the Dashboard reloads on a short cycle,
and one `/health` per peer on every cycle would be expensive for a section that
is almost always closed. A node that does not report the placement makes the
section say it does not know. See `inference.md`, "Where the layers ended up".
The same card carries "llama.cpp load parameters" (`LoadParamsSection`), which
shows the parameters that machine's context started with: Flash Attention,
`n_batch`/`n_ubatch`, total and per-sequence `n_ctx`, KV sequences, threads,
layers on the GPU and KV cache type. It serves whoever wants to compare the node
with an `llama-cli` command line without opening `/health` by hand. The values
come from `/health`'s `load_params`, read from the peer's live context; a node
whose `/health` does not carry the field simply does not show the section.
Flash Attention tells "on (auto)" from "on (requested)", because llama.cpp's
default is to probe the backend, and shows "unknown" when the diagnostic bridge
did not answer - never "off". When the fit changed the values, a note says they
may differ from the requested ones. See `inference.md`, "Flash Attention and
the load parameters".

When the machine's configuration asked for MTP, the same card shows whether the
`nextn` head became active. If creating the MTP context fails, it shows "MTP
unavailable" and the `mtp_unavailable_reason` returned by `/health`; the
operator does not need to infer the degradation from missing logs.

## Settings / Network and Preferences

`/config/network` gathers the Network Information: public name, organization,
description, rules, channels, visibility and location. Node and infer get read
only, while admin can edit. The same screen computes nodes, average latency, known VRAM and models from
`ClusterView`.

### Network location

A network declares where it is so the Networks screen can sort by distance.
The declaration is the principal's, made in `LocationEditor` (Network settings
and the setup wizard), and it follows four rules:

- **The browser's location is asked only on request.** The dialog has a "Use
  my browser's location" button; nothing calls `navigator.geolocation` before
  it is pressed. Browsers share a location only on HTTPS or on localhost, so
  on a plain `http://<address>` panel the button explains that and the
  administrator places the point on the map or picks a city by name.
- **The city is never typed.** It is a row of the offline database embedded
  in the node (`crates/node/src/utils/geo.rs`, the unmodified cities export
  of the Countries States Cities Database, ODbL 1.0, about 150 thousand
  places). Placing a point asks `GET /api/ui/network/geo/nearest`, which
  answers the closest cities by distance to each city's reference point (not
  by boundary, so the administrator chooses among the nearest); the search
  asks `/geo/cities?country=&q=`. The settings carry the row's `city_id`, and
  the node rewrites country, state and city names from that row on every save.
- **The administrator chooses what is published** (`location_precision`):
  `approximate` (default) snaps the point to a grid of about 10 km and
  declares `accuracy_m` 10000, `city` publishes the city's reference point,
  `exact` publishes the point as placed. The node applies this before storing
  anything: a more exact point than the chosen one is never written to
  `pom.json`, sent to members or published. The point reaches the node in the
  save and in the nearest-city lookup, which stores nothing.
- **The location is not part of the trust document.** Country, city,
  coordinates and the discovery label are settings the principal changes at
  any time; they travel to members with the other network settings and to
  everyone else in the signed directory record. The location is declared, not
  verified: nothing proves a network is where it says.

The map is Leaflet with OpenStreetMap tiles, loaded only while the dialog is
open; those tile requests are the one thing in this flow that leaves the node,
and the dialog says so. Without internet the map stays empty and the button
and the search still work. The Networks screen computes distances in the
viewer's browser, never sends the viewer's position anywhere, and writes
"about N km" for a network whose `accuracy_m` is a kilometre or more. The
About screen credits both data sources.
`/config/network/participants` reuses the known nodes card and is available to
admin and node. Each peer shows the **route in use**, not the address it
announces: a badge (Direct, Direct (P2P), Direct (hole punch), Via relay, No
route) and that route's address - the relay's, when the hole punch did not
close. The announced one stays in the row's `title`. See `p2p-network.md`,
"Diagnosis".

`/config/trust` gathers this machine, network sharing and connection keys.
`/config/preferences` holds the administrative exposure, the `ThermalPanel`,
the `HuggingFaceSettingsPanel` and the link to storage.
`/config/preferences/admin` keeps the administrative preferences for TLS,
QUIC, process logs between nodes, network chat and local discovery. The split
reorganizes the same operations and keeps the existing authorization.

The exposure is persisted in `pom.json`: localhost is the installed default,
and the network uses a password separate from the `connection_key`; only the
Argon2id hash is persisted, and the previous password is required to change it.
Switching to localhost clears the password. An old token is migrated to
Argon2id on the first load, keeping its value as the password. The port is
validated before saving and a collision is reported without starting on another
port. The `ThermalPanel` reads and writes the policy at
`GET/POST /api/ui/thermal`; the Hugging Face panel uses
`/api/ui/model-provider/config`.

## Settings / Preferences: no TLS switch

TLS between nodes is always on, so the preferences have no panel for it and
the Sidebar no TLS icon (`p2p-network.md`, section 5). The QUIC panel
(`QuicPanel`) is always shown on the administrative preferences route, and the
local discovery switch takes its authority from `NodeInfo.quic_authority`,
which is the same principal check. The interface on `http://localhost` stays
plain HTTP; from another machine the browser is sent to `https://`, and warns
about an unknown issuer until that machine trusts the node's local CA. The
Preferences screen has the "Node certificate" panel
(`components/HttpsCertificatePanel.tsx`, `GET /api/ui/tls/ca`): whether this
computer trusts the CA, its fingerprint, the `.crt` download, the manual steps
per system and for harnesses (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`), and
"Install on this computer", offered only to a request from the node's own
machine, since the trust store is that machine's.

## Settings / Administrative preferences: QUIC transport

The "QUIC transport" panel (`QuicPanel`) sits next to TLS on the administrative
route. The switch is disabled outside the principal; the server also answers
403 if another node tries `POST /api/ui/network/quic`. The change does not
restart the model: new hops prefer HTTP/3 over QUIC, and a failure falls back to
the HTTP/HTTPS of the TLS policy. The telemetry card shows `http`, `https` or
`quic` according to the transport that really carried each hop, and marks the
fallback.

## Deployment process logs

`DeploymentLogs.tsx`, at `/deployments/logs`, is the terminal console of the
cluster's process and not the request audit of `/logs`. The black background
and monospace font leave each line with the machine name, level and origin
time; the lines come from the admin's local replica in arrival order and the
screen polls for new batches every second. The deployment selector sends
`deploy_id`, allowing the same logs to be opened from a deployment in History.

The lines are kept by the logs plugin (`pom-plugin-logs`) on each admin; the
POM only captures, carries them to the admins and holds the collection policy.
Without the plugin active on this node the console says so instead of
polling, and the Dashboard does not show the dock. While
`NodeInfo.cluster_logs_enabled` is false the console and the dock show
"Collection off" instead of "Live", since no new line arrives.

`Preferences.tsx` shows the "Process logs between nodes" panel only when the
logs plugin is active on this machine. The switch is open to every admin
(a secondary admin's change is passed on to the principal), and the backend rejects turning collection on unless the plugin is
active there; turning it off always works. Only the admins need the plugin. State and authority come from `GET /api/ui/node`; the change
uses `POST /api/ui/network/logs` and needs no restart. The counter of entries
dropped by the bounded queue shows above the terminal, so the screen does not
present an incomplete view as if it were the full stream.

On the Dashboard, the admin can open `DeploymentLogDock` at the bottom. The
collapsed bar takes a single line; open, it grows upward to half of the panel,
queries `GET /api/ui/cluster-logs` in real time and follows the end while the
reader stays at the bottom. The deployment's active version is available in an
inspector on the right, read-only, using `DeploymentInspector` and the same
fields as the original configuration. Node can also read the versions through
`GET /api/ui/deployments`, but still without the administrative actions.

## Chat

`src/routes/Chat.tsx` talks to the cluster through `POST /v1/chat/completions`
with `stream: true`, like any harness - what shows up there is what a harness
would get. The history belongs to the browser (`chatStore`, `localStorage`, a
cap of 50 sessions, synced between tabs), not to the node. `ChatSessionProvider`
also keeps the stream and accumulates deltas in memory, publishing and
persisting the partial on a 50 ms window and flushing immediately at the end or
on interruption. So leaving the route does not interrupt generation: on coming
back, the conversation shows the text already received and the live state
without forcing a re-render per token. Only the stop button or sending a new
message aborts the previous request. The conversation keeps three things
besides the turns:

- While an answer has not started, `ChatWaitingLine` shows a line that
  changes every few seconds (`lib/chatWaiting.ts`, texts under `chat.wait.*`).
  The sets follow the wait: `wake` while `chat/capabilities` reports
  `asleep` - a machine of the pipeline unloaded its model for idleness or is
  loading it again - then `pieces`, `assemble` and `long` as the seconds pass.
- The single file field reads `GET /api/ui/chat/capabilities?model=<chosen>`,
  which aggregates the chosen model's head and coverage without reusing the topology the `visitor`
  profile cannot query. It accepts any file type, button and drag and drop.
  Images require `vision: supported`; `unsupported` keeps the model without
  vision and `unavailable` says the read was refused or could not finish,
  without inventing an empty model. Images are resized in the browser to fit the
  effective budget before becoming base64 in the request: the estimate keeps
  four bytes per token, reserves 10% for safety and tries JPEG quality and
  resolution up to 2048 px on the longer side, without going over 512 px for
  normal images. The aspect ratio is kept, and the bubble and the pending
  attachment show "Resized automatically" when the image was changed. Sent images
  stay whole in the bubble, with a caption and alternative text.
- The dictation button only renders when the browser exposes Web Speech, and it
  dictates in the interface's locale.
- The `Think` button uses the `thinking` field of the same capabilities read
  and only shows up when the head's global `no_thinking` is off. The choice is
  sent as `thinking_enabled` on each Chat call and stays in the `ChatSession`
  persisted in `localStorage`, so it carries across messages of the same
  conversation until the user changes the toggle. The head still enforces the
  global off, even if an old session carries `true`.
- A model's thinking is shown closed: one line, "Thinking" with pulsing dots
  while no answer text arrived and "Thought" after, with the count of thinking
  tokens (one per stream delta that added to the reasoning,
  `ChatStreamState.reasoningTokens`, kept as `ChatMessage.reasoningTokens`).
  The text opens only on a click.
- While the network serves no model, the Chat draws no composer and no RAG
  menu: an empty conversation shows `components/MissingPiece.tsx`, three
  puzzle pieces in the brand colours locking together around an empty,
  pulsing slot, under the POM mark, with "One piece is missing". A profile
  that may open `/deploy` gets the next steps (download a model, deploy it,
  talk): the local catalog (`/api/ui/models`, asked only on that screen)
  decides whether downloading is done and deploying is the current step;
  any other profile is told to ask an administrator. "How POM works" opens
  the documentation, and "See earlier conversations" reopens the latest
  conversation with turns, drawn only when one exists. A conversation with
  turns keeps them and puts that line in place of the composer.
- An empty conversation opens like other chat tools: no title bar, only the
  POM mark (`components/ChatHero.tsx`, the site's mark for the active theme,
  rising from below after a short pause; the line and the nodes follow it),
  a line that opens "P O M" into PIECE OF MIND, PLACE OF MIND, POWER OF
  MACHINES and PLATFORM OF MIND in turn, the access gate's three linked nodes,
  and the composer centred under them, drawn larger there. The composer is
  one element with a framer-motion `layoutId`: the first message carries it to
  the bottom bar of the conversation. Its placeholder is one of the
  `chat.prompt.*` lines, in the tone of the waiting lines, chosen from the
  conversation id (`promptPlaceholderKey`) so it does not change while typing;
  the network Chat keeps "Write to the network". While no conversation has
  messages, the conversation list and its New chat are not drawn beside the
  Chat, nor in the infer menu while the route is `/chat`. On any other route the
  infer menu keeps New chat (with the empty list), since that menu has no other
  way back to the chat once every conversation was deleted.
- Text files are still refused before sending when the estimated size exceeds
  the model's effective context: first `context_length` from `/v1/models`, then
  the active node's `n_ctx`. Images use the same budget, but are reduced before
  validation; only an image that does not fit even at 512 px gets refused. The
  textual content and the multimodal parts stay in `requestContent`, so the
  history resends the same structured request. Each pending attachment can be
  removed without erasing the draft.
- The microphone button only shows up when `SpeechRecognition` or
  `webkitSpeechRecognition` exists; the transcription uses the UI locale when
  possible and goes straight into the text field.

`ChatSessionProvider` shares the active session and the actions between the
Chat and the Sidebar. On `visitor`, the Sidebar uses `ChatSessionList` as the main
menu: the history list has internal scrolling limited to the available space,
highlights the active conversation and keeps `New conversation` fixed above it.
Switching or deleting a session does not abort a generation in progress;
deleting the session only keeps the stream from recreating it when it ends.

That menu is seen from **every** route, so picking a conversation or starting a
new one **navigates to `/chat`** besides switching the active session. Without
that, a click made from settings or documentation changed the shared state and
left the screen where it was - the menu looked like it had stopped responding.
Whoever is already in the chat is not sent again, so equal entries do not pile
up in the browser history.

- **Each answer shows one telemetry line** (`components/TelemetryLine.tsx`):
  generation speed and tokens, TTFT, end-to-end total, prompt tokens and their
  processing speed, and the pipeline. That is the whole open-source telemetry.
  When an active plugin claims the `chat.telemetry` slot
  (`docs/subsystems/plugins-ui.md`), its component is mounted at the end of the
  line with the trace, timings, origin and the captured environment, and
  provides the detailed view (in the enterprise plugin: a side panel that opens
  a full dialog, with the animated pipeline, request waterfall, per-phase
  breakdown, findings and JSON/AI export). The network RTT and runtime queries
  for that environment run only while a plugin claims the slot.
  At the end of the stream, `Bubble` temporarily uses the trace, timings and
  origin from `telemetryCapture` when the final capture arrives before the
  message's immutable update. `telemetrySnapshots` keeps the full capture per
  `messageId`, so earlier messages keep their own line and slot props.
  Once the history gets `message.trace` and `message.timings`, they stay the
  preferred source; that transition keeps the footer visible without reloading
  the page.
- **Switching models midway becomes an event in the flow** (`role: "event"`,
  which `turnsOnly` filters before the conversation travels in the request).
  The selector sits below the text field. `lib/chatModels.ts` removes from that
  selector the ids with the `@d-<deploy_id>` deployment selector; the backend
  catalog and the deployment management screens still see the full ids.
- **The title is edited by clicking it** and is saved (`titled` keeps the first
  message from overwriting the chosen name).

`chatStream` appends structured `reasoning_content` directly and keeps the
`<think>` splitter as a legacy fallback without leaking delimiters between
chunks; `finishChatStream` records the summary of TTFT,
prompt preprocessing speed, output generation speed and end-to-end total, plus
the per-hop trace. The Chat shows the basic numbers in the telemetry line; the
remaining fields (`server_ttft_ms`, `engine_ttft_ms`, `client_ttft_ms`, network
time) travel in the trace to the `chat.telemetry` slot. During the SSE, the browser measures first chunk,
first token, duration, end of the last token, chunks and bytes with
`performance.now()`; the `Date.now()` attached on receipt only serves to
correlate with the peers' wall timestamps. No clocks of different machines are
subtracted. If `/v1/chat/completions` sends an SSE frame with the OpenAI error
envelope, `streamErrorMessage` keeps the real message even when there is
already text in the answer; it shows up in a block that wraps lines, allows
selection and offers a copy button. The generic empty answer message only shows
up when the stream ends with no tokens and no error. Each phase also shows the
hidden-state bytes sent and received in the crossing and the `dtype` actually
used. In decode, the backend line separates the readiness observed by the
scheduler, the wall time of `ggml_backend_tensor_get` and the capture's
post-processing. Telemetry tells apart the peer's and the client's f32/f16
conversions, the f8 conversion and the residual frame serialization; the
conversion times are subsets of the overall encode/decode. The old capture
total stays identified as compatible; `tensor_get` may include the D2H copy and
the CUDA backend's internal sync. The QUIC transport table also shows the wait
in `send_data` and `finish`, separate from the wait for headers and the RTT.
When the stream ends, the Chat asks for a snapshot of `/api/ui/chat/telemetry`
for the trace's adjacent pairs and `/api/ui/peer/:node_id/runtime` for each
machine of the trace. The panel shows the total observed RTT of the calls, the
RTT of each edge with the origin -> target label, capacity and requested
configuration, effective placement by GPU/CPU/mixed buffers and each peer's own
`POM_PROFILE_SYNC`. The placement is not derived from `n_gpu_layers`, and the
sync state is not deduced from a zero time. If the collection fails, the screen
says unavailable or not measured instead of filling in zero. In a one-machine
pipeline, it explains that there are no edges between peers.

## Documentation: two audiences on the same route

`apps/frontend/src/routes/Documentation.tsx`, with the tabs in `?tab=`.

- **Guides** (`src/guides/`) - the half written for humans: ordered pages with
  animated SVG diagrams, timelines, headline numbers, checklists and expandable
  questions, generated from data (`Block[]`, `content.ts`) by `GuideView`, in
  English and Portuguese. The animations are CSS (`guide-*` in `index.css`), so
  `prefers-reduced-motion` freezes them and each drawing still reads static. It is the default tab: a human must not land on
  `AGENTS.md - crates/routing-base/`. No number shows up there unless it is in
  the markdown - when they diverge, the markdown wins. The list of guides is in
  `apps/frontend/src/guides/AGENTS.md`.
- **Reference** - the repository's living markdown, written for agents,
  embedded in the bundle by `apps/frontend/scripts/build-docs.mjs` (there is no
  route on the node; the screen reads `manifest.json` + `import.meta.glob`).

Four decisions that are not cosmetic:

- **The screen is full width** (`App.tsx` lists `/documentation` next to `/` in
  `isFullBleed`) and scrolls inside: the navigation column and the reading
  column are flex children with `min-h-0`, each with its own scroll. Without
  that `min-h-0` the child grows with the content and overflow never kicks in -
  that is how the search ran off the screen without a scroll bar.
- **File names do not show up in the navigation.** The manifest carries `label`
  (a short human name) and `kind`: a folder's `AGENTS.md` is **the overview of
  that section**, so the section header is what opens it (clicking "docs/"
  shows `docs/AGENTS.md`) and it is never listed as a leaf. The reader also
  drops the document's own `# `, which would only repeat the header's title in
  other words.
- **Collapsing works.** The chevron toggles, the label opens the overview, and
  the section containing the open document is no longer forced to reopen - that
  was the defect: a category that always stayed expanded.
- **On a phone it is two screens with a single address.** Below `sm:` the
  navigation column takes the full width and the reader is hidden
  (`mobileReaderOpen`); picking a guide or document opens the reader full screen
  with a Back button fixed at the top (it only exists mounted while that view is
  open). Back only touches that local state - `?doc=`/`?guide=` stay intact, so
  reloading the page does not lose the place. A deep link that already arrives
  with `?doc=`/`?guide=` in the URL counts as the same selection and opens
  straight in the reader. From `sm:` on the two columns live together again,
  regardless of that state.

## The UI with the cluster on TLS

With the policy on, opening `http://<peer>:8080/` returns **301 to `https://`**
on the same port, and the panel works normally from there. The certificate the
browser gets is a self-signed ECDSA one, different from the Ed25519 one the
peers get - a browser does not verify Ed25519. See `p2p-network.md`, "Two
certificates on the same port".

**The browser warns about an unknown issuer** until the computer trusts the
node's local CA (Preferences, "Node certificate", or the desktop app's prompt at
start). The certificate keeps its key across restarts, so an accepted warning
does not come back after a restart. See `p2p-network.md`, "Two certificates on
the same port".

## The Flow card says how much of the model fit on the GPU

A layer on the CPU is orders of magnitude slower than on the GPU, and that is
the first reason for a sluggish inference. The Flow card shows the split
(`LayerPlacementMeter`, in `ModelNetworkCanvas.tsx`) in the same shape as the
VRAM and RAM gauges, right below the layer range: an amber bar and
"12 GPU · 10 CPU" when layers are left on the CPU, a neutral bar and "22 on GPU"
when none are. Double-clicking still opens the full detail; this is a summary,
not a replacement.

The number does **not** come from `NodeConfig.n_gpu_layers`. That is the
*request*, and the request is almost always the sentinel 99 ("offload
everything"); llama.cpp's `fit` decides, lowering the number until it fits in
VRAM. Deriving the split from the request answers "all on the GPU" exactly when
some are not - the only case the operator needs to see. What counts is
`RuntimeStatus.reported_gpu_layers`, which the node reads from the inference
server's own `/health` `n_gpu_layers_effective`, in the same 3s poll that
already gives it `reported_total_layers`. No new call: `/api/ui/cluster` already
fetches each peer's `/api/ui/node`.

That fetch has two error outcomes, and the screen separates them. `unreachable`
is the peer that answered at the announced address and whose read broke - a
dead end, a pink dot, and the message states the error. `rediscovering` is the
peer whose addresses all stopped answering and whose current address the node
already asked the network for: a pulsing amber dot and "Rediscovering this
node", because temporary IPv6 addresses rotate on their own and the state
resolves on the next gossip round. Saying "nothing below can be read" in that
case sent the operator after a defect that fixes itself - but the promise has a
90 s deadline, after which the card goes back to `unreachable`, because a peer
that announces and does not answer exists. In both cases the peer's layers
still count for coverage - its inference server may be perfect and only the
control address died - so the card says so, and the "Coverage" card at the top
loses the "complete" green and shows `dashboard.coverageUnconfirmed` while the
active model's `model_inventory[].unreadable_nodes` is not empty.

Without an engine report the line does not show up. `layerPlacement`
(`src/lib/layerPlacement.ts`) returns `null`, and the screen stays silent
instead of guessing - saying "all on the GPU" without knowing would be worse
than silence. Negative means "all" in llama.cpp's convention, and read as zero
it would say the whole machine runs on the CPU.

## The Flow canvas: arrangement by layers

`ModelNetworkCanvas` draws the network, and `lib/networkLayout.ts` decides where
each card goes. The rule is **one column per pipeline stage, in pipeline order**
(`autoLayout`). A card's column is the longest chain of ranges that end before
it starts, so ranges that start at the same layer share a column even when they
end at different layers: with 0-15, 16-30, 31-40 and 16-40, the column after
0-15 holds 16-30 and 16-40, and 31-40 comes after it. Inside a column, the
replicas of the first range (lowest end layer) are centered on the axis - the
route that goes straight on - and the alternative ranges stack below it. That
is what makes a fork look like a fork and an edge become a short line.

The precedence is: dragged position > computed arrangement > grid
(`fallbackPosition`, only left for a card the arrangement does not know).
**A new deployment is already born arranged**, without anyone clicking
anything.

Two buttons, and the difference between them matters:

- **Arrange** (`LayoutGrid`) - applies the arrangement by layers **keeping what
  the operator dragged**. A hand-made arrangement is a decision, and a button
  that erased decisions would be the button nobody presses.
- **Reset** (`RotateCcw`) - discards everything, including the drags.

The stack uses each expanded card's **measured** height (a `ResizeObserver`
on the card; `offsetHeight` ignores the zoom transform) and adds the height of
the plugin service badges hanging under a card (`SERVICE_TETHER_HEIGHT`), so
Arrange/Reset never put a card on top of the one above or of its badges. The
badges are icon-only in the Flow and open to name and kind on hover or focus.
The engine badge follows what serves the machine's model (`servicesInUse`):
llama.cpp gets its own badge, and an installed inference plugin that is not the
engine in use (Strata on a llama.cpp deployment) is not drawn.

The Deploy Studio's map uses `deployLayout` instead: a multi-stage plan keeps
the columns, a single stage (a whole model or replicas of one range) is laid
side by side, and machines on the map without a range yet sit side by side in
a row under the plan instead of stacking in the column of a range they hold.

`autoLayout` is stable with respect to input order (tie-break by
`start_layer`, `end_layer`, `id`): the peer list reorders on every cluster
poll, and an arrangement that depended on it would shake on screen.

**Fixed defect:** `deploymentLayoutKey` used to include each peer's
`changed_at`. It changes on every node restart, which changed the key, which
made `readPositions` return empty - the operator's arrangement was not
rearranged, it was **lost**, and the screen fell back to the grid every time a
machine restarted. Today the fingerprint is only model + set of ranges.

## Slicing, deploying and transferring on screen

- **Slice** (`SliceModels`): Pipeline and Tensor Parallelism are a pair of tabs
  (`role="tablist"`) at the top of the source panel, not a checkbox - each mode
  has its own controls below. The Tensor Parallelism tab only exists when
  `useNode().data.pom_dev_mode === true` (`NodeInfo.pom_dev_mode`, the same
  `POM_DEV_MODE=1` that already hides the DEV sections of `DeploymentInspector`);
  without the flag the screen never offers nor sends
  `mode: "tensor_parallel"` - the only possible mode is pipeline, and a `mode`
  already set to `tensor_parallel` when the `/api/ui/node` answer arrives later
  is reverted. Pipeline offers a plan by size (target in GB per slice, empty =
  "the rest") or a specific range by start/end layer; the specific range uses
  the GGUF's architectural options, sends `range: { start_layer, end_layer }`
  and adds the part to the manifest by hash, without replacing or deleting
  earlier slices. In TP mode, the operator picks 2..64 ranks and the preview
  shows one full range per rank with the identifier `1of2`, `2of2`, `1of4`
  etc. Each change posts `POST /api/ui/slice/plan` (260 ms debounce); TP carries
  `mode: "tensor_parallel"` and `tp_size`, while pipeline carries `slices` or
  `range`. `POST /api/ui/slice` runs the job. The activity panel stays visible
  also for finished jobs and shows the named phase, bytes, rate, ETA when
  reliable, current file and partial outputs on failure. The preview checks the
  free space at the real configured destination, with a concrete number and a
  warning before starting. The Shards screen separates `parts` and `tp_parts`,
  groups TP by size and sorts the ranks inside each group. The transfer
  considers both lists. When the catalog confirms a multipart GGUF, this screen
  shows the real sum of bytes, the MULTIPART label and the list of physical
  parts. While the header is still being enriched, it does not show the first
  part's size. If the source is incomplete, the reason coming from
  `split.count`, `split.no` or the missing part shows up in the panel and the
  slice button is disabled; the operator needs to put the full set together and
  let POM produce the slices. The preview and the job start repeat the GGUF
  preflight for the head the loader requires. In `deepseek4`, an intermediate
  range copies the real head weights the loader requires, and the preview shows
  the additional payload repeated in each one; if the tensor table does not
  allow the copy, the refusal lists the concrete reason. The final range is only
  offered when the table contains the full head.
- **Deploy** (`DeployModel`): opens the Deploy Studio on a wide screen, with
  Shards/Whole model/Tensor tabs, separate technical model and operational name,
  Simple/Complete strategies, a central map and a compact property inspector.
  Every peer shows up in the bottom strip and can be dragged onto the map; in
  the Globals tab, the inspector also lets you pick `cache_type_k` and
  `cache_type_v` for the installation; each change of the combos is applied
  through `POST /api/ui/cache-types`, with no save button, subtitle or `global`
  badge; the planner recomputes the suggestion considering existing cuts,
  memory, RTT and load. The coverage bar shows missing and overlapping layers;
  the map shows hops, capacity and known latency, plus alternative routes and
  the projected load distribution. Each card separates the loaded physical range
  from the sub-range actually executed. The inspector is its own component
  (`components/DeploymentInspector.tsx`), and it is **the same** one History
  uses read-only: a single property list, not two copies that diverge on the
  first new field. It has three categories, and the boundary between them is
  whose parameter it is - *Default settings* (what llama.cpp already has:
  context, sessions, max tokens, batch, prefill chunk, sampling, the
  installation's K/V cache, disabling thinking/embeddings and the speculation
  choice), *POM settings* (fit to memory, chunks in flight, merging neighboring
  slices and hidden-state precision, plus a *Beta* group inside it with MoE
  experts in RAM, the GPU expert cache and the automatic prefill chunk, shown
  to everyone) and *POM settings (DEV)* (inference engine and per-peer idle
  unloading). Default settings, POM settings and the Beta group each carry a
  help button by their title that opens their guide from Documentation in a
  dialog (`components/GuideDialog.tsx`, guides `config-padrao`, `config-pom`
  and `config-beta` in `guides/deploySettings.ts`). The DEV section only
  shows up when `NodeInfo.pom_dev_mode` is on; without the flag the UI removes
  its controls from the DOM. The `[i]` comes **before** the name, not after:
  with names of different widths, icons to the right of the text came out
  jagged, and before the name they form a straight column from top to bottom
  of the panel. The engine combo does not offer "Automatic" and assumes
  `llama.cpp` on mount - a presentation choice; the backend still accepts
  `auto` only when the request states it explicitly. Without a value, the
  default is `llama.cpp` (`dense`). The cards are a **preview of how the system
  will look after deployment**, not of the current state: each GPU of each
  machine gets a projected VRAM bar (expected usage against the total, with the
  current usage as a tick) that is recomputed on every change of context, batch
  or simultaneous requests. The projection comes from
  `POST /api/ui/deploy/capacity-preview`, an analytical estimate from GGUF
  metadata and free VRAM on the peer, without loading weights or calling the
  native fit (see `deployment.md` §3.1). The card turns amber when it only fits
  because "Fit to memory" would trim the context or the GPU layers, and pink
  when it does not fit even at the floor; both cases ask for a confirmation
  naming the machines on `Deploy` - it is a warning, not a block, so the
  operator can lower or raise the context as they like. Each parameter of the
  right panel has an `[i]` that opens on hover/focus/touch with what it means
  and how it affects inference; when the node reports a doubtful or limited
  capability, the same spot in the column shows the amber triangle with the
  reason. The edges come from the planner's execution graph: they only connect
  contiguous sub-ranges, keep equivalent entry branches and show the
  directional RTT used to choose between them. The canvas does not invent
  bridges from the card order, which keeps a fast entry when another loaded
  range overlaps. Tensor uses the whole model on every rank and does not show
  the old slice selection. The global properties (context, batch, sampling,
  sessions) stay separate from the per-machine properties (`n_gpu_layers`, the
  KV-on-GPU boolean and local capacity, on the same property line as the
  globals - `[i]`, name, control); when a machine is selected, the inspector also
  shows the layers it will run. The hidden-state precision
  (f32/f16/f8/i4g64/i4g128) only shows up when the state crosses a peer boundary
  - a pipeline with distinct adjacent ranges, of slices or of a whole model spread
  over its own slices; for a whole model on one range, Tensor or
  slices on a single machine, the state is never serialized and the selector
  would be inert. f16 is still the default; f32 is a lossless option with four
  bytes per value, while f8 and i4 are lossy. In the globals, the whole model
  facts section and its machine selector are not shown: the machine chosen on
  the map is the only one that counts for the deployment. In the globals, the
  execution section has one **Speculation** choice - None, MTP or lookup -
  because the gateway never runs both in the same round (with both stored, MTP
  wins); MTP's numbers and where it runs only show with MTP chosen. A new
  deployment starts with MTP on (`RuntimeParams` default, used only when the
  plan supports it) and the minimum probability at `0.75`, a field the operator
  can still edit. The vision projector and the MTP draft show one summary line;
  the path on every machine opens in a "More info" window. The **Inference engine**
  section only shows up when the GGUF's real metadata reports more than one
  expert. The UI presents `dense` (llama.cpp) and `moe`, and sends the choice in
  whole model, sliced pipeline and Tensor Parallelism modes. The backend also
  accepts `auto` only when it arrives as an explicit choice from an API or a
  persisted deployment. For a dense model the section stays hidden. If the chosen
  combination is not supported, the selector stays visible with the explicit
  reason and the deploy is blocked; telemetry is the source of truth about the
  engine that ran. The other conditional options get `DeployPlan.capabilities`
  from the backend: `no_thinking` uses `tokenizer.chat_template` and the GGUF
  vocabulary, MTP uses the selected file's `nextn` metadata/tensors and only uses
  the final peer's runtime as a limitation, and lookup uses the runtime
  capabilities of every target. Proven absence in the model hides the option;
  `unknown` keeps it visible and actionable, with source, reason and risk. A
  runtime limitation is also an actionable warning. The `no_embeddings` switch
  stays visible: on, provably unsafe peers refuse the deployment by name and
  atomically; an undetermined topology can only proceed with an explicit
  warning. **Shards** carries the "Merge neighboring shards on the same
  machine" switch (`merge`), which is what lets a machine hold two adjacent
  ranges: the node merges them into a single file (`splitter::merge`) before
  starting, because `split` renames the blocks of every slice to `blk.0..` and
  two cannot load side by side. Without it the planner refuses the pair, and the
  refusal says to "merge the neighboring slices on screen" - the switch exists so
  that sentence has a destination. It shows up under the same condition that
  decides whether `merge` goes in the request (`!isWhole`): a control without a
  send, or a send without a control, was exactly how it got lost once. There is
  no second legacy table below the canvas: the map and the bottom strip hold the
  topology, the directional RTTs and adding peers. Each edit goes back to the
  node for validation; the deploy (`POST /api/ui/deploy`) **rescans the
  cluster** instead of trusting the preview, and progress comes from
  `GET /api/ui/jobs`. The map uses the same primitives as the Dashboard canvas:
  dots in the background, draggable cards, zoom, Fit, Arrange and Reset, with
  positions persisted per deployment. The inspector is 19rem wide, can be
  collapsed from its left edge, starts directly with the Globals and Machine
  tabs, with no title of its own and no outer `p-3` padding, and visually
  separates global properties from the selected machine's. Capacity warnings sit
  as amber icons next to the control: the full text, including source and
  reason, opens on hover, focus or click in a fixed portal that breaks words and
  identifiers without spaces without creating horizontal overflow. The set of
  alerts stays spread next to the respective controls, instead of forming a row
  of cards. The inspector lets you assign to a peer a range it already holds by
  switching that peer's active range without moving another machine. The same
  range on two peers is a valid parallel route, not a swap. Clicking an already
  assigned range again removes the selection - the same button is the way there
  and back. Leaving only removes that range on that machine: replicas of the
  same range on other peers stay where they are, and the peer stays on the map
  even without any range, because removing a layer is not the same gesture as
  "Remove from deployment". Peers already on the map no longer show up in the
  bottom bar; dragging or double-clicking an available peer adds the card
  without redrawing the topology until a range is assigned. For a whole model,
  the map is left empty when switching from slices, so the operator picks the
  machine, which takes the whole file. More machines can be added: each gets a
  range it holds in the Machine tab (the whole file as a replica, or a slice of
  the same model), the coverage ribbon appears, and the node accepts the map
  while the ranges cover every layer. Under an inference service the whole
  model stays on one machine and a second one is refused with a warning. In Tensor, only
  peers on the map enter as primary/worker, even if others have the model.
  GPU, RAM and load are updated by the cluster view every 5 seconds; the
  Dashboard map shows the last effective RTT and transport observed in the
  inference run for each edge and, at the Gateway, every complete route of the
  model with the aggregated RTT of each leg, including the return; when there is
  a recent trace, the run's wire RTT beats the tunnel fallback probe. Routes are
  listed from lowest to highest cost, with the node ids as a tie-break; a whole
  model deployment with a single head is also queried, even with no edge in the
  graph. The planner's directional survey stays reserved for route choice and
  is not shown as if it were the run. A missing GPU shows as `no GPU`, without
  an indeterminate bar. In Complete, every peer enters the canvas and the
  candidate list in slice and Tensor modes; whole model keeps the one-machine
  restriction. Coverage is reduced to the footer next to the peer bar. The last
  deployment of each model stays in `localStorage` (`deployMemory`) and comes
  back filled in after the first plan, pruned against what still exists.
  Topology changes are validated by `POST /api/ui/deploy/plan` after the 260 ms
  debounce. Runtime parameters, per-machine GPU/KV overrides and the
  operational name do not change machines or ranges, so they stay local during
  editing and do not restart the distributed survey. The query key only
  contains ranges and machines; the deploy carries the current overrides. If
  any of them changes, the old capacity projection is hidden;
  `POST /api/ui/deploy` runs the full preflight with the current values before
  starting the deployment. The catalog's initial answer may only contain name,
  path, size and manifest data, while the node reads the GGUF headers in the
  background. The screen shows the inventory right away. The catalog only reads
  the KV header and `stat` of the parts in the fast scan: it does not walk the
  tensor table, does not hash whole models and does not map the weights. The
  enrichment task can validate a source's `pom_id` to inherit a projector,
  after the inventory was already published. The run button and endpoint only
  check the metadata needed for the current selection: for a pending whole
  model, the planner sends its already listed path to
  `POST /api/ui/model-metadata`, which enriches that item without waiting for
  the neighbors. Another model still being enriched, a silent peer or an old
  peer without `metadata_complete` does not block. When the selection itself
  cannot be resolved yet, the plan returns a single `blocked` naming the
  pending range or model. Enrichment logs its start and completion; a terminal
  error stays in the model's `metadata_error` and does not stop the others,
  while `metadata_complete` becomes `true` when the pass ends. The published
  answer is reused by the requests. The first call installs a placeholder and
  shares the single scan in flight, so F5 and repeated combo changes do not
  stack scans or enrichments. If the refresh changes the inventory, the backend
  reconciles the result by `path + size + mtime` and keeps the models already
  read; only a new or changed file goes back to the header. A new attempt only
  happens on a TTL refresh or invalidation, and dropped tasks are logged.
  **The grey button always says why.** The comparison between the node's plan
  and the screen's rows (`sameRows`) sorts both sides by layer, because the
  screen keeps the rows in that order and the node builds the answer its own
  way; and the explanation panels render under `answered` (the node answered
  about *these* rows), never under `validated`. The planner accepts several
  ranges on the same machine, but keeps `blocked` for global gaps, overlap and
  other real problems; `whyNot` puts the reason in one line next to the button,
  including the two cases without a panel (nothing selected, answer in
  flight). The map shows consecutive hops only forward, adds a virtual Gateway
  at the end with the return leg when the tail is not the gateway yet and shows
  every candidate route, highlighting in the total the one with the lowest RTT.
  Internal scrolling on a narrow viewport neither cuts cards nor creates page
  overflow.
- **Deployments** (`Deployments`): one row per `deploy_id`, "live" separate
  from "default", sorted by creation. Each row expands and lists that
  `deploy_id`'s versions (the history of `RuntimeParams` already run under the
  same id), with the active version marked. "Redeploy this version" sends
  `{ version }`; "Redeploy with new parameters" opens the form seeded from the
  active version's `RuntimeParams` and sends `{ params }` - the backend reuses
  the version with equal parameters or appends a new one to the same id.
  Redeploying the same shape from the Deploy Studio (same proven model, same
  machines and ranges) also lands on the same `deploy_id`; changing machine or
  range mints a new id. See `deployment.md` §7.1.
- **Transfer** (`Transfers`/`SendFile`/`Notifications`): remote model downloads
  with progress and cancellation, pending offers (accept/refuse straight from
  the bell), active transfers (bytes, speed, ETA, cancel) and finished ones.
  Sending ("send a file from this machine") offers a file from the local
  catalog through `POST /api/transfer/send`; the receiver only sees an opaque
  grant (`grant`), so "send anything" never becomes "read anything". Progress
  comes from the nodes, so reloading the tab loses nothing.
  `POM_DEV_AUTO_ACCEPT_TRANSFERS=1` (a variable of the receiving process, read
  at startup) accepts every offer without a click - dev only. Peers on the same
  machine share the hostname, so the destination and the availability label
  carry the start of the `node_id` when the name repeats (`uniquePeerName` in
  `lib/peerName.ts`, used by `routes/Shards.tsx`). Offers, active and finished transfers in `Transfers`
  are each a full-width table (it used to be a list of cards), with the
  accept/refuse/cancel action in the last column.

### Network information and joining materials

`/config/network` fetches `GET /api/ui/network/info` and shows the same card to
every profile. Node and infer see name, description, rules, organization,
channels, visibility, country, city and location read-only. The authorized
administrator edits those fields and saves through
`POST /api/ui/network/settings`; the backend writes the local configuration,
propagates the signed mutation to the peers and republishes the public geo
label.

Below the metrics, the "NAT traversal" panel (`TraversalPanel`) shows the
`traversal` field of `GET /api/ui/cluster`: the NAT report state, the published
direct addresses and relay URLs (split by `isRelay`), the relays connected and
the hole punch count. It exists because two nodes behind NAT that do not see
each other fail at one of a few steps, and before it finding out which one
required `RUST_LOG=debug` on the right machine. An old binary's node does not
send `traversal` and the panel simply does not show up.

The same administrator has, on that screen, the controls to copy and download
the `.pom-admin-trust.json` document. On a local or private network the
connection key also shows up to copy or download. The real wizard offers the
same materials on the invitation step, highlighting the connection key in the
first card. The raw content of the trust and of the public key starts
collapsed, but copying and downloading stay available without expanding the
cards. `SetupWizard.tsx` is still only the visual prototype and takes no part in
these calls.

The panel also exposes `Forget network`, with a destructive confirmation. That
action calls `POST /api/ui/network/leave`, which revokes the local trust,
empties `pom.json` of the network and restarts this node, which comes back in
its own network (restored from standby, or a fresh local one). It is deliberately different from `Leave network`, in the
`This machine` panel, which calls `POST /api/ui/leave` and only takes the
machine out of the cluster without erasing its local configuration. That action
is also available to a node profile without administrative authority and does
not depend on a loaded model or a Ready state: the handler stops the runtime,
clears the deployment journal and unannounces the machine before answering.

## First run wizard and guide

A machine that starts with no configuration creates its local network, and
its operator is walked through the first things to do: a wizard on a screen
of its own, then hints on the panel. Both are one list of steps kept by the
node (`GUIDE_STEPS` in `webui/guide.rs`), and only the `admin` profile sees
them.

### The wizard

`components/FirstRunWizard.tsx` replaces the whole shell, past the access
gate, while the current step is one of its two (`useFirstRunWizard` in
`App.tsx`). It asks nothing about the kind of network: the network is the
machine's own, and joining another one comes later, from the Networks screen.

1. **Names** (`names`) - two fields on one screen. The machine name is
   pre-filled with the name the machine goes by (the system hostname until
   it is given one) and saved with `POST /api/ui/network/node-name`. The
   network name is pre-filled with the name the network was created with,
   under a warning that it is advertised to everyone if the network is made
   public, and saved with `POST /api/ui/network/settings`, the rest of the
   settings untouched. The step is reported only after both saves.
2. **Follow the instructions** (`bootstrap`) - no field: it says that hints on
   the screen point at what to do next, and its button opens the panel.

Each name field has its own "Random" button that fills in a random name of two
words and a number (`lib/randomName.ts`). Neither name may be empty, a
failed save keeps the operator on the screen with the error, and "Back" only
changes the screen shown. A reload resumes at the first step not done. The
wizard draws on the `.pom-login*` / `.pom-setup*` shell of the access gate,
with the language switch in the corner.

### The hints

After the wizard the panel opens and the interface points at the next things
to do, one hint at a time (`components/FirstRunGuide.tsx`):

1. **Download a small model** - Find HF models. The guide fills the search
   with `Qwen/Qwen2.5-0.5B-Instruct-GGUF` (`GUIDE_MODEL_REPO` in
   `lib/guide.ts`) once, on arrival, and the balloon follows the clicks: the
   result row, a file in the details window (Q4_K_M when listed), then the
   confirmation. While the download runs the balloon moves to the corner and
   says to wait. Done when a download of that repository finishes or the
   local catalog holds a file of that model.
2. **Deploy the model** - Deploy to network: "Whole model", then this machine
   in the peer strip, then the Deploy button once it is enabled (and a "loading"
   text while the job runs). Done when the
   cluster reports an active model.
3. **Send a first prompt** - Chat, the message field. Done when an answer
   finishes streaming.
4. **Join another network** - the Networks screen, pointing at the first Join
   button. Joining ends the guide; so does "Finish", because staying on the
   machine's own network is a valid choice.

The first three have no save to report them, so the guide watches the node for
their outcome while each is the current step (`DownloadWatcher`,
`DeployWatcher`, `ChatWatcher`). A machine that already has the model, a
deployment or an answer passes the step at once. The download needs the
Hugging Face CLI on the node, like every download on that screen; without it
the search does not run and the step can only be skipped.

Each hint is a balloon that floats beside its target, with a ring around the
target. While the operator is on another screen it points at the menu entry
that leads to the step (the link, or the section that hides it); on the
step's own screen it points at the control. A step lists its targets from the
last click to the first, and the first one on screen wins, each with its own
text. The target is found by attribute -
`data-guide-nav` and `data-guide-section` in the Sidebar, `data-guide` (and
`data-guide-repo` on a search result) on the controls - and looked up again
five times a second, since scrolling, menus and windows move it. With nothing to point at (a narrow screen with the menu closed, an
empty list) the same text sits in the bottom right corner. Every balloon
offers "Skip this step" and "Hide these hints", and motion stops under
`prefers-reduced-motion`.

The progress lives in the node, not in the browser: `first_run_guide` in
`pom.json` lists the steps done, and is absent when there is no guide. Only
the startup that created the configuration from nothing sets it; a machine
that left a network and got a fresh local one is not new, and
`Config::prepare_for_setup` (joining or leaving a network) drops it. When the
last step is done or the operator hides the guide the field is removed and
the guide never comes back, and neither does the wizard. A step is reported
through `useGuideActions().complete(...)` (`lib/guide.ts`,
`GET|POST /api/ui/guide`).

## A machine has a single name

`node_name` in `pom.json` is how the operator names the machine, and it is the
name the node announces to peers (`Network::hostname()`). The operator sets it
on the My machine screen ("Machine name", `POST /api/ui/network/node-name`),
with no restart: the next announce carries it. The first run wizard asks for
it through the same route. Without a `node_name` the machine goes by the system hostname. Every
screen reads that
name; the "Send to..." selector of `/shards` used to read `sysmon`, the
operating system's raw hostname, and was the only place in the app that called
the machine by another name. On a machine with two nodes it was worse: both
reported the same OS hostname, the list saw a collision and broke the tie with
the `node_id` prefix. `/api/ui/models` and `/api/ui/cluster/models` answer the
announced name, and a peer that did not answer the survey still carries the
name it announced - without it the screen fell back to the `node_id`, a hex
prefix. The presentation lives in `lib/peerName.ts`.

Note on `AddNodeDialog`: it reseeds the choices **on the transition to open**,
not on every new prop: the Dashboard rebuilds `peers`/`slices` on every cluster
poll, and an effect depending on those arrays discarded the operator's choice
in the middle of editing (the combo "jumped back" to another machine on its
own). A second effect only replaces a choice that disappeared from the list;
the slice is identified by its range (`keyOf`), not by index. Tests in
`AddNodeDialog.test.tsx`.

## Defects noted

- DEFECT NOTED: `LeaveNode` reuses the trigger's label on the confirm button -
  the window shows two buttons with the same text ("Leave network"). Cosmetic,
  but a user can confuse the trigger with the confirmation. The component is no
  longer rendered by the machine card; the note stands for any future caller.
- DEFECT NOTED: in a narrow window (~420 px) the content column overflows
  horizontally and the panels come out cut on the right - `node_id`, help text
  and buttons fall outside the viewport. It is not a single screen: it
  reproduces the same on `/config/network` and on `/config/preferences`, so it is
  the frame's, not the panel's. Checked in a headless capture at 420x900 on
  both screens.
