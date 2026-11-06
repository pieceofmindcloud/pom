# routes/

Each screen of the panel, one file per route, wired up in `../App.tsx`. Every
screen is backed by React Query hooks from `../lib/queries.ts` and speaks to the
node's HTTP gateway (`crates/node/src/backend/webui/`, `backend/gateway/`); none of them keep
cluster state in the browser. All copy goes through the i18n provider
(`../i18n/provider.tsx`). Text written by the node (error messages, refusal
reasons, job steps) is English at the source and is shown as it arrives.

`PublicNetworks.tsx` carries the `data-guide` attribute the first run guide's
balloon points at (`components/FirstRunGuide.tsx`). So do `RemoteModels.tsx` (search field,
result rows by `data-guide-repo`), `DeployModel.tsx` ("Whole model", the peer
strip, the Deploy button while enabled) and `Chat.tsx` (the message field);
those three steps report nothing, the guide watches for their outcome.

Per-file list:

## First access
- **`SetupWizard.tsx` and `Setup.tsx` are deprecated and unreachable.**
  `App.tsx` neither imports nor routes them, there is no `/setup` path and the
  node does not serve `/api/setup/*`. The files and their tests are kept until
  the wizard is removed for good; the two entries below describe that dormant
  code. Do not link to it.

- `SetupWizard.tsx` - a navigable prototype of the first access. It uses `setupWizardTypes.ts` for
  the central state and `setupWizardMocks.ts` for simulated hardware, nodes, models,
  capabilities, public networks and health; the six paths end
  in the same summary and make no calls to the backend.

- `Setup.tsx` - the real first-access wizard, in the identity of the access
  gate (a dark panel with the mark and the animated step trail like the site's
  nodes). Three steps for the administrator (Start, Network, Invitation) and two
  for node and inference (Start, Join), defined in `setupWizardFlow.ts`.
  It only asks for the role, the machine name, the network's name and visibility (private or
  public; the location, through `LocationEditor`, only on the public one) and, for whoever joins, the root and the key -
  brought by the local discovery (`LocalDiscoveryPanel`) or pasted in
  `TrustFields`, which only mounts when the manual block is opened. In the invitation, every
  computer looking for the network appears in `DiscoveredMachineCard` with name,
  system, IP (from the LAN, only in the local wizard), CPU, RAM and GPU. The admin's
  preferred address is optional and never pre-filled: the network is found by
  the principal's key, and neither the trust root nor joining from Public
  networks brings an IP bootstrap. When the wizard was opened from Public
  networks, `SetupState.joinAccepts` holds that network's
  `accepted_enrollments` and `StartScreen` disables the `worker`/`visitor` roles
  it does not accept (`admin` founds its own network and stays available).
  `PublicDirectoryPanel`, in the join step, lists public networks from
  `GET /api/setup/public-networks`. A machine in setup mode has nobody to ask,
  so the panel has a field (bound to `publicDirectory`) for a relay key
  (`pk:...`) or a node address to ask, on top of the relays the installation
  already knows; Enter or Refresh asks again. `directoryReplicas` keeps the
  keys out of the `public_directory` sent on save: "Join" fills the trust fields like the manual block and clears any
  connection key, and "Chat" opens `LobbyChat` against `/api/setup/lobby`
  without a session. A listed network with `public_stats` shows a line with
  its nodes, VRAM and models (`DirectoryNetworkStats`). Folders, disk, reserves, password (`admin`) and
  workspace (`~/pom_workspace`) are defaults adjusted afterwards. The screens talk
  to `/api/setup/*`; do not confuse it with the prototype `SetupWizard.tsx`. See
  `docs/subsystems/web-ui.md`, "First-access wizard".

## Network, plugins and this machine
- `PublicNetworks.tsx` - Full-bleed directory of public networks (this
  machine's row carries `ConnectionBadge` for a member still connecting): a
  scrolling table with name, organization, place, reach, nodes, latency, VRAM,
  models and the action. The list is a page of the node's table of known
  networks (`GET /api/ui/networks/known`, 25 per page, prev/next under the
  table): it paints from there at once, then `refreshLive` asks
  `/api/public/networks` (which records what it finds) and reads the page
  again. A row the live listing did not return shows "Not seen now - last
  seen ..." (`network-cached-<fingerprint>`). There is no page title:
  the tab row (`Tabs` with `actions`) is the header, with Add network, Quick
  setup and Refresh on its right while the list is shown. The table has six
  columns: name, place, reach with latency below, nodes with VRAM below, the
  served models one per line (three at most, then "and N more", all of them
  in the tooltip) and the action. A count the network does not publish (nodes, VRAM,
  models) is a dash. The distance sits under the place; the accepted roles
  (or "closed") have a column of their own with no title, right before the
  right-aligned actions, each role an `IconTip` that explains it on hover,
  focus or touch. Once "Measure latency" ran, the page is ordered by the measured
  latency (unanswered last) unless the order chosen is by name. An unmeasured
  latency draws nothing; a measured one is colored by `latencyBand` (under 60
  ms green, under 150 amber, under 300 orange, above red), with the tooltip
  saying how much inference slows, and "no answer" is red: the network may be
  down or out of reach. The organization and the full model list
  are in the side panel. No footer notes. The search box (debounced 300 ms, or applied at
  once by its Search button/Enter) sits alone in the toolbar; country, city
  (narrowed by country), distance radius, order, browser location, latency and
  the distance-origin line are folded behind a "+" Filters toggle that counts
  the active filters. Below 768 px (`useNarrowScreen`) the table, which forced
  a sideways scroll, becomes a list of cards (`NetworkRow layout="card"`) and
  the header actions are icons. The filters toggle uses a sliders icon (the
  "+" is Add network). "Measure latency" is a toolbar action beside the
  search, not a filter, and "Use my browser's location" is drawn only in a
  secure context (HTTPS or localhost), since POM serves plain HTTP by default.
  The list and the open lobbies use the shared `Tabs` (as Network Chat), with
  a close button per lobby tab (`TabItem.onClose`). The join action reads
  "Talk". "Share" (own network) and "Add network" both open
  `TrustTransferGuide` (`../components/`), one animated diagram of the trust
  file going from a machine of the network to the other machine, with "You
  are here" on the operator's side; the download or the file picker is its
  primary button. The country, city, distance radius and order go
  to the node as query parameters, and any change returns to page 1; the
  country and city options come from the answer. **No IP is shown**:
  reachability comes from `usePublicLocator` (`lib/publicLocator.ts`), which
  asks the node (`/api/public/networks/locate?key=<publisher_key>`) whether
  the publisher key has a direct route, only a relay or no pkarr record, and
  reports a check that failed as `unknown`. The place column is what the node
  resolved from each network's coordinates. Distances are measured from this
  network's location (`useNetworkInfo`) or, after "Use my browser's
  location", from the browser, and sent to the node only as the sort and
  radius origin; the screen never asks the browser by itself. Latency comes
  from `usePublicLatencies`, only after "Measure latency" or the latency
  order, which reorders the current page.
  Clicking a row selects it (iris band, `aria-selected`) and opens
  `NetworkSidePanel` on the right, in the inspector's look and small print:
  the name, the enrollment state (`network-enrollment-state`, one line per
  role with its icon from `ENROLLMENT_ICONS`), description,
  channels and rules (always open), only the fields the owner filled,
  nothing the row already shows. Its footer (`network-panel-actions`) holds
  the actions: "Quick setup" (see below), "Ask to join" and "Lobby" for
  another network (closed ones only get the lobby), "Leave network" for a
  member, "More details", the trust document export (own network only) and,
  for an imported network, remove. "More details" opens
  `PublicNetworkDetails`, a `Modal size="full"` with every field, technical
  ones (fingerprint, publisher key, relays, trust document) included.
  A lobby opens in place of the list and the page header (`LobbyChat
  layout="page"`, no back button). A network whose Chat is off
  (`lobby_chat` false) has nothing to talk in: "Ask to join" opens the
  request alone in a dialog (`LobbyChat` modal), never a tab. When a lobby
  answers `lobby_disabled` the network refuses admission too (the serving
  node gates every lobby request, admission included), so the dialog or tab
  shows only that notice. Open lobbies are
  kept in a tab strip that stays on top of the list and of every open lobby
  ("Networks" plus one closable tab per lobby; going back is picking the
  Networks tab) and stay
  mounted and polling while hidden, so the operator can talk in several at
  once. "Ask to join" opens the lobby with the focus on the admission request.
  When the operator's own network is the only row (no search) and it can
  still take the quick setup, a card under it (`network-discovery-off`) says
  the machine is not connected to other networks, names the two ways to use
  POM (several of your own computers, or hardware shared over the internet)
  and opens the quick setup. It is centered, carries the compact puzzle of
  `MissingPiece` (the fourth piece hovering) and has an X that hides it for
  good in this browser (`pom.networks.discovery-off-card.dismissed` in
  `localStorage`); the quick setup stays in the tab row.
  The principal of a network whose discovery is off or hidden gets "Quick
  setup" (`../components/NetworkQuickSetup.tsx`) in the header, on its row
  and in the panel.
  The action column depends on membership: the network this node is in never
  offers Join. A member gets "Leave network" (`POST /api/ui/network/leave`);
  the principal (`authority_level === "primary"`) gets no action there.
  Pinned rows come from `GET /api/ui/networks/pinned` and stay on top of every
  page, outside the order but filtered here by the same search (accents
  folded), country, city and radius: this machine's network
  ("own") and the networks imported here ("custom", amber band). The node
  leaves both out of the known list. "Add network" reads a JSON file and posts
  its text to `/api/ui/networks/custom`; only the own network's row exports
  `trust_document` as `<name>-trust.json`. A network with an empty `accepted_enrollments` is
  marked closed and can still be talked to. A network with `public_stats`
  that this node is not in gets its nodes, VRAM and models from
  `usePublicStats` (`../lib/publicStats`).

- `NetworkSettings.tsx` - it keeps the network's operational portrait separate from the
  local preferences **and** from this machine's identity, which lives in
  `MyMachine.tsx` (route `/config/my-machine`). Node and infer get a
  read-only read with the name, description, rules, organization, channels,
  visibility and location; an authorized administrator may edit the whole
  block. Editing writes on the node, propagates the signed envelope to the peers and
  updates the public discovery. The editable block asks the questions of the
  Networks screen's quick setup, in the same order and drawing, with the
  components `../components/NetworkQuickSetup.tsx` exports (`SetupSection`,
  `DiscoveryChoices`, `InfoFields`, `AccessFields` with `full`, then
  `LocationEditor`): discovery, information, who can join, location, and
  last this machine's relay (`RelayOptions` with `section`, saved at once,
  not by the Save button), only when the internet reaches this machine. Who
  cannot edit the network gets the relay as a panel below the read-only
  block, under the same condition. It
  includes "Accepting new members", one card per `EnrollmentRole`
  (`ENROLLMENT_ROLES` in `lib/types.ts`) with its icon; unchecking all of them
  advertises the network as closed. "Visitors can use the network Chat"
  (`chat_visitors`, off by default) sits beside it.
  There is no lobby option: the lobby follows discovery. "Who can join"
  (`access_policy`) and "Accepting new members" are shown only with discovery
  on or hidden, and the published stats only on a public network; with
  discovery off they are not rendered at all. The location editor and the
  geographic label are rendered only on a public network, the only kind the
  directory places on a map. "Who can join" offers "Approved
  by an admin", "Anyone without approval" (`auto_approve`, its warning is the
  choice's own text) and "This machine only"; a network still on the legacy
  open mode sees it as a fourth choice. Discovery offers
  "Hidden" (`private`) again: no relay lists the network, and another machine
  reaches it by importing its trust document on the Networks screen. The leave
  action ("Return to my own network", `POST /api/ui/network/leave`) is not
  drawn on the machine's own network (`NodeInfo.own_network` with no
  `standby_network`): that network is inherent to the machine. An admin also
  sees `AdmittedMachines`, the approved admission records, with "Revoke"
  behind a confirmation (`admissionsApi.decide(id, "forget")`). "Published
  stats" (`public_stats`) follows the same rule. The location is edited with
  `../components/LocationEditor` (map, browser location on request, city from
  the node's offline database, and what the network publishes); the discovery
  label is shown read-only below it. `TraversalPanel` shows the
  `traversal` field of `/api/ui/cluster` - the NAT report state, the direct addresses and
  the relay URLs, the relays connected and the hole punches - because two nodes behind
  a NAT that cannot see each other fail at one of three steps, and before this knowing which one
  required debug logs on the right machine; an older node does not send the field and
  the panel does not appear. The screen also aggregates nodes,
  the average latency, VRAM and known models. The participants list was
  moved to the route `/config/network/participants`, for `admin` and
  `node` only. The information panel also offers `Esquecer rede` ("Forget network"), which asks
  for confirmation and calls `POST /api/ui/network/leave` to clear the local configuration
  and restart in the setup - it stays here because it is about the network, not about
  this machine. On Windows, when supported, the same screen shows the state of the
  four firewall rules derived from the effective P2P port and offers `Tentar
  novamente` ("Try again") after a UAC refusal; that action is independent of the deployment.
  Description and rules are edited as plain Markdown text with no live
  preview, and the channels in one line; the read-only view renders them with
  the Markdown renderer the Chat uses.
- `Plugins.tsx` - `/plugins` combines the public catalog, read by the node
  (`/api/ui/plugin-catalog`), with local state from
  `GET /api/ui/plugins`, using `code` as the identity. The public client lives
  in `lib/pluginCatalog.ts` and accepts plugins
  with a compatible commercial release. Missing image data uses placeholders;
  up to four screenshots open in an Escape-dismissible lightbox that restores
  focus. Descriptions use Brazilian Portuguese when available and otherwise
  fall back to English. License validity, granted plugins, activation
  candidates and usage by `plugin_code` come from the node's authenticated
  state, never the public catalog. The full-bleed screen follows the header and
  inner container used by `/models`. `state` represents license validity;
  `active` is true only after the host completes its handshake and is ready.
  Local state may publish `installation_error`, shown on the card with an
  actionable detail; a failed activation refreshes status before the screen
  exits. Active local plugins remain visible even when absent from the public
  catalog.

  Deactivation requires confirmation. The license-key form appears whenever
  any public entry requiring a license is not active locally, even if another
  plugin already has a license or context. It also appears when local status is
  missing or expired/revoked, and is hidden when all known licensed plugins
  are active. Purchase sends licensed plugins to the site's purchase wizard
  (`/comprar/`) with an encoded `plugin=<code>` parameter via
  `lib/pluginPurchase.ts`, in a new browser tab (a desktop shell hands it to
  the system browser and POM keeps running); the wizard asks for the account
  and billing details before Paddle and shows the license key at the end. Cards are
  compact, two per row from `xl`, and grouped by `plugin_code`: the commercial
  catalog, GitHub repository and local state for the same code share one card.
  The screen has two tabs, on the right side of the title bar
  (`FullBleedHeader` `actions`). "Plugins" searches and pages the store on the
  license server (`GET /v1/plugins?q=&page=&per_page=10`, featured and POM-team
  plugins first; the full unpaged catalog is still read to decide whether the
  license form shows and to name the plugins a key granted). Plugins known only
  to this node (a registered repository, an active plugin outside the store or,
  in development mode, a mounted source) are appended to the first page and
  filtered by the same search text. The example plugin (`base`) is left out of
  this list. Its right column is license activation and the "Add from GitHub"
  card (public repository URL), plus a development notice only when the source
  mount is missing, empty or unreadable. The header's "Check for updates" asks
  every installed plugin (a GitHub install refreshes its repository, a licensed
  one calls `update/check`) and POM (`/api/ui/pom-update/check`) at once and
  opens `components/PluginUpdatesDialog.tsx` with what is newer: Install per
  row and Install all, POM last because it restarts the node. "Develop your plugin"
  highlights `pom-plugin-base` (the example plugin, as its own card or an
  install button) and links to `https://pieceofmind.cloud/plugins/publicar/`,
  saying that a submission is reviewed before it is listed. Cards no longer
  show the plugin code; one Version field shows the installed version or else
  the newest one this node can install, and the supported systems are icons
  (`components/PlatformIcon.tsx`). A catalog card with a `github_url` shows Install: it
  registers the repository and installs its newest release compatible with
  this node, at once for a `pom_team: true` plugin and after a confirmation
  with an unverified-code warning for anything else (the install is at the
  operator's own risk). A repository card shows its manifest description
  and screenshots (`raw.githubusercontent.com`), refresh/remove icon actions
  and Install (the newest compatible release). A version is only chosen for an
  update: once installed from that repository, the card shows a selector with
  the compatible releases newer than the installed one and Update, behind a
  confirmation. This source does not check `requires_license` or contact the
  license server. In development mode a plugin with a mounted source gets
  "Build and install locally" / "Rebuild and reinstall" on its own card, with
  the result or the build stage and exit code below it. The card does not show features or the last update check. Activation, installation, update and deactivation
  invalidate `keys.pluginUi` so the plugin menu updates immediately; 30-second
  polling covers changes made outside this tab. One form activates every
  `plugin_code` granted by the server in a batch (`POST
  /api/ui/plugins/activate`, response `{activated}`); updates and deactivation
  remain per-plugin. Preferences open in a modal from the card's settings
  icon, only when the plugin is active and its installed release has a schema.
  The modal displays typed boolean/directory defaults, sends values only to the
  node's local route and shows the node's reason on failure.

  POM's own update goes only through `/api/ui/pom-update/*` (see
  `crates/node/src/pom_core_update.rs`), here in the updates dialog and in
  `About.tsx`. With
  `NodeInfo.pom_dev_mode=true`, on any profile, the screen queries local sources and allows
  building and installing even inactive plugins. A local build is separate
  from license state, grants no licensed features and does not activate a
  license; an existing licensed/GitHub installation keeps its entitlement.
  The action calls the PrincipalOnly route and shows success or the build
  stage/exit code without compiler output, and does not use the GitHub release
  route. The page has its own Settings > Plugins menu entry. Commercial release
  data is selected for the local node's `system.os`/`system.arch`; if the
  catalog does not publish that target, the screen shows no release rather than
  suggesting one for another platform.
- `MyMachine.tsx` - "My Machine" (route `/config/my-machine`, its own
  entry in the Network menu, also accessible to `visitor`). It contains exactly what
  left `NetworkSettings.tsx`: the "This machine" panel (hostname, `node_id`,
  ports, profile, effective authority, and `Sair da rede`/`Voltar para a rede` ("Leave the network"/"Return to the network") via
  `POST /api/ui/leave`/`/api/ui/rejoin` - abdication, not privilege, available
  to every profile) and "Announced address" (`POST /api/ui/network/advertise-addr`,
  it shows the effective address the peers use). Between the two sits "Machine
  name" (`POST /api/ui/network/node-name`): the field holds the configured
  `node_name` and, while empty, shows the hostname in use as its placeholder.
  The relay options moved to `NetworkSettings.tsx`. `Sair da rede` ("Leave the network") here is different
  from `Esquecer rede` ("Forget network") in `NetworkSettings.tsx`: one disconnects the machine from the cluster,
  the other erases the local configuration and restarts in the setup.

- `MyBudget.tsx` - "Usage" ("Uso", route `/config/usage`; `/config/budget`
  redirects there; reached from the Usage popover beside the bell, every
  profile, drawn only while this node has a budget configured; the meters and
  the quota shape live in `components/QuotaParts.tsx`): this machine's token budgets from `GET /api/ui/quota` - one panel per
  plan with input/output bars and the window's turnover, the session-reset
  credits (`POST /api/ui/quota/reset`) and the request for more budget
  (`POST /api/ui/quota/request`). Without an admin's logs plugin answering it
  says so instead of showing zero; while a machine that runs the plugin has
  not answered (`pending`) it keeps loading instead of "no budget". The query
  is `useMyQuota` (`lib/queries.ts`), shared with `components/UsageButton.tsx`. The admins' side is the logs plugin's Quota
  screen.

## Chat
- `Chat.tsx` - The chat: `POST /v1/chat/completions` with `stream: true`,
  with no title bar on an empty conversation, `../components/MissingPiece`
  and no composer or RAG while the network serves no model. While this node
  is still on its way into its network (`useConnection`, `lib/connection.ts`)
  that screen is "Connecting to <network>" instead (`MissingPiece`
  `variant="loading"`, `ConnectingSteps` - find the network, get access, see
  the models - and, past 60 s on one step, why it may be stuck with the last
  refusal and a link to the Networks screen), so a visitor that just joined
  is not told the network serves no model. Once the token arrives the screen
  stays on "see the models" for 90 s (`awaitingModels`) while `/v1/models` is
  polled every 3 s, and only then falls back to the missing piece. Otherwise it shows the missing piece (with
  `NoModelSteps` - download, deploy, talk, current step from
  `useLocalCatalog` - for a profile that may deploy, and "See earlier
  conversations" only when one exists), capabilities
  asked for the chosen model (`useChatCapabilities(model)`), otherwise
  opening an empty conversation on `../components/ChatHero` with the composer
  centred (one `layoutId` element that moves to the bottom bar on the first
  message; the conversation list and its New chat are not drawn while no
  conversation has messages, `hasConversations`, in the infer menu as well) and showing a model's thinking closed (`ThinkingBlock`: label and
  thinking-token count, text on click),
  exactly what a harness gets. Imports `../lib/chatStore` (sessions, localStorage
  and the shared active-session controller) and `../components/ChatSessionList`,
  `../lib/chatStream` (leitura de deltas SSE, erros e métricas), `../lib/chatScroll`
  (`isNearBottom`), `../lib/api` (`streamSse`, `ApiError`), `../lib/queries`
  (`useCluster`, `useNode`, `usePeerRuntimes`, `useChatCapabilities`,
  `useChatTelemetryNetwork`, `useServableModels`, `useInferenceHeads`),
  `../components/Markdown`,
  `../components/ui`, `../lib/toast`. The `/v1` call is signed with the
  connection key by `api`. When the stream finishes, it captures the cluster
  snapshot, directional RTT for each adjacent execution pair and each peer's
  inference health. It also measures browser dispatch-to-first-token TTFT
  separately from the server-reported pipeline TTFT and stores it only in the
  client-side timing view. `ExecutionEnvironment` keeps requested configuration
  separate from effective layer placement and shows `POM_PROFILE_SYNC` only
  from the explicit per-peer health field; health shows the selected path,
  while the request trace shows the executed engine and mixed/mismatch flags.
  The runtime record keeps stable identity between unchanged polls so the
  telemetry snapshot cannot trigger a render loop that freezes navigation.
  At the end of a stream, `telemetryCapture` carries the trace, timings and
  source to `Bubble` while the immutable transcript patch is settling;
  `telemetrySnapshots` keeps that complete capture keyed by message id, and
  `message.trace`/`message.timings` remain the preferred values once available.
  The quick timing summary stays outside the closed details card, so every
  assistant message keeps its summary and full telemetry without a reload.
  Each hop's backend telemetry separates the llama.cpp evaluation call from
  explicit backend synchronization.
  The `visitor` profile hides the source, the timing and throughput summaries and the
  detailed telemetry panel on this screen; that is presentation, not
  authorization.
  In the same composer, `InferenceHeadChoice` shows through which layer-0 head
  the next inference will enter for any profile with a model selected,
  but it only appears when there is more than one candidate. It makes it explicit when there is
  no candidate or when the selection fell to the fallback without a fresh RTT.
  The combo's choice is controlled by the `ChatScreen` and travels in the next call
  as `inference_head`; the backend validates the `node_id` and keeps the head in
  the plan and in the telemetry's `x_pom.source`.
  The `Think` button only appears when `/api/ui/chat/capabilities` returns
  `thinking: "supported"`. Its value is saved in `ChatSession` and sent as
  `thinking_enabled` on every message, persisting until a new user action.
  Missing snapshots stay visible as missing data. The environment card also
  renders the effective MoE executor and its concrete reason from `/health`.
  Phase rows also render hidden-state bytes sent/received and the effective
  f32/f16/f8/i4 dtype. The conversion row separates peer and client f32/f16
  encode/decode work from the general frame timings. Its picker uses
  `../lib/chatModels` to hide deployment-qualified
  ids from chat presentation only; deployment screens and backend model
  resolution retain those ids. An OpenAI-compatible error frame inside
  the stream sits beside any partial answer and shows the server's exact
  message; the generic empty-answer text is only used when no
  error was sent. Long errors wrap lines, stay selectable and have
  a local copy control.
  The route `/admin-ui/chat` reuses the same screen without the administrative shell to
  fit in the native popover of 390 x 844 points. `OpenNativeChatWindowButton` is
  deliberately invisible in a common browser and it only publishes `open` on the
  WebKit bridge `pomOpenWindow`, which opens the larger window without creating a parallel flow of
  authentication.
- `NetworkChat.tsx` - its own page `/network-chat`, with the same visual pattern
  as the AI Chat. It reads the
  authenticated snapshot and sends text, file and typing actions through the
  node's endpoint-backed gateway. It resolves `node_id` to the node hostname/name
  map used by the dashboard, downloads persisted file blobs on demand, and
  polls typing state without touching the AI session store. Message and typing
  ids come from `lib/id.ts`, so sending over plain HTTP does not call the
  secure-context-only `crypto.randomUUID` API. The message viewport follows new
  messages while it is near the bottom, and the page uses the full-bleed
  `PageHeader` pattern instead of a rounded outer panel. On the
  phone, the participants stay hidden until the `Ver online` ("See online") button is pressed;
  the Sidebar entry receives the counter of unread messages and opening
  the route marks the current snapshot as read. When the snapshot says
  `lobbyEnabled`, a tab pair switches between the `network` and `lobby` rooms
  (`useNetworkChat(room)`); only an admin gets the lobby (the snapshot's
  `lobbyEnabled`). The lobby tab lists the candidates (`LobbyThreadList`,
  `LobbyHardwareCard` for one present, an "Asks to join" mark for a pending
  request) and shows one candidate's thread at a time, sending with
  `room_id: "lobby/<candidate>"`; it offers neither files nor typing, because
  the candidate reads that thread. The selected candidate is outlined and
  `aria-current`, and `LobbyThreadHeader` above the messages names it and
  carries Approve / Decline for its pending request (`useAdmissions`,
  `admissionsApi.decide`). `?lobby=<node_id>` (the bell's link) opens the lobby
  on that thread and is then dropped from the address.
## Information screens
- `Documentation.tsx` - Two audiences, one screen, switched by a tab pair at
  the top of the navigation column (`?tab=guides|reference`).
  **Guides** are written for people: markdown under `docs/guides/` and
  `docs/human/` (see `../guides/AGENTS.md`), listed in groups (`GUIDE_GROUPS`:
  how it works, screens, deployment settings, inside the code) and rendered by
  `GuideView`. A reference page whose `path` is a guide's `reference` shows a
  "Read the explanation for people" button to it (`humanFor`). **Reference** is the
  repository's living markdown, written for agents, self-contained and
  build-time embedded: `scripts/build-docs.mjs` copies the folder `AGENTS.md`
  files, the `docs/subsystems/*.md` READMEs and the root README into `../docs/`
  (gitignored) plus `manifest.json`, and this screen reads them via
  `import.meta.glob` + the manifest - no node route, so it renders on any
  machine serving `dist/`, even one that never had the repo.
  Three decisions that are not cosmetic:
  - **The screen is full-bleed** (`App.tsx` lists `/documentation` alongside `/`
    in `isFullBleed`) and owns its scrolling: the nav column and the reader
    scroll independently, each a flex child with `min-h-0`. Without that
    `min-h-0` the child grows to its content and the overflow never engages -
    that is exactly how the search results used to run off the screen with no
    scrollbar.
  - **No file names in the navigation.** The manifest carries `label` (short
    human name) and `kind` (`overview` for a folder's `AGENTS.md`, `doc`
    otherwise). A folder's `AGENTS.md` is that section's own page, so the
    section *header* opens it - clicking "docs/" shows `docs/AGENTS.md` - and it
    is never listed as a leaf. `nameOf` never falls back to a file name;
    `withoutLeadingH1` drops the document's own `# ` title because the reader
    header already states the subject.
  - **Collapsing is honest.** The chevron toggles, the label opens the
    overview, and holding the open document no longer forces a section back
    open (`collapsed` state, explicit `true` wins).
  - **Mobile is two screens sharing one address.** Below `sm:` the nav column
    takes the full width and the reader is hidden (`mobileReaderOpen` state);
    picking a guide/doc entry opens the reader full-screen with a `Voltar`
    ("Back") button pinned at its top (`sm:hidden`, only mounted while open). `Voltar`
    only flips that local state - it never touches `?doc=`/`?guide=`, so the
    selection survives the round trip. A deep link present at mount
    (`?doc=`/`?guide=` already in the URL) counts as that same selection and
    opens straight into the reader. From `sm:` up both columns render side by
    side regardless of the flag, exactly like before.
  The search box serves both tabs. In Reference it filters label, path *and*
  body, and replaces the tree with a flat result list whose hint is the section
  name, not the file; in Guides it filters every guide by everything it shows
  (`guideText`) and keeps the groups. Test file
  `Documentation.test.tsx` covers the contract: guides first (a person must not
  land on agent markdown), the guide->reference deep link, no `.md` or
  `AGENTS.md` in the list, section header opens the overview, collapse works
  while holding the selection, the two scroll containers, `?doc=`/`?guide=` deep
  links, search + clear, empty state, and the mobile nav/reader/`Voltar`
  contract. Loading the embedded set requires
  `src/docs/` to exist, which `npm test`'s `pretest` hook guarantees.
- `About.tsx` - an information screen at `/config/about`, the last leaf of
  Settings. It shows the repository's MIT `LICENSE` and the Open Source
  Initiative page, the risk/LGPD notice and links to the site (`pieceofmind.cloud`), contact
  (`contact.html`) and the official repositories. It shows the running POM release
  (`current_version`) and checks POM's releases on demand through
  `/api/ui/pom-update/check` (`usePomCoreUpdateStatus`; GitHub first, the site
  as fallback, no license). It says when POM is up to date or no source
  answered, and when a release is newer it shows it with its source and applies
  it right there (`/api/ui/pom-update/apply`), or says why this node cannot (a
  container, or a site download that still needs an invitation). The Plugins
  screen's updates dialog uses the same routes; activating/deactivating plugins
  is done in `/plugins`, never here. It also lists the frontend's and the Rust workspace's direct
  dependencies grouped by the declared license, with version, ecosystem,
  a text filter and the repository link. The catalog is
  `../lib/licenses.generated.ts`, generated by `scripts/build-licenses.mjs`; the
  test in `About.test.tsx` covers the license, the notice and links, the two
  ecosystems, the filter and the on-demand check/application of the update.
- `Dashboard.tsx` - Cluster overview: the admin sees cluster
  coverage/machines/model/in-flight stats (`ClusterStat`: two short lines, the
  label over the value with its detail beside it and a hairline bar, so the
  toolbar leaves the height to the map; one row that scrolls sideways, the
  whole width on a phone with the actions below); node and infer see local model,
  slice and in-flight requests instead. Admins see the Add button only when
  `ClusterView.active_model` is nonempty. With no active model, Details is the
  initial view; with an active model, Flow remains the initial view. Manual
  view selection overrides that default. For administrators, the deployment
  properties inspector selects the `is_default` row for the active model,
  preferring a serving default and falling back to the live or latest local
  record so old deployment history cannot hide the map toggle. A graph view
  ("minima",
  `ModelNetworkCanvas`) or a details grid ("estendida") is used where the
  profile permits it.
  The model graph receives directional transport measurements and
  `pipeline_routes` for every complete deployed route. It renders the
  distributed cycle as Gateway -> peers -> Gateway, orders route cards by
  the backend's per-token `total_ms` (network in the chosen return mode plus
  compute; never the raw sum of legs) with node ID as a stable tie-breaker, and uses the least-cost
  route for the headline while keeping alternatives visible. An observed
  `pipeline_cost` from a completed chat execution remains available when every
  execution leg and compute measurement is known.
  The local detail distinguishes `idle` from `serving`: the first keeps the node
  active, but it says the model was unloaded and will be reloaded on the
  next request.
  The details grid lists the **whole network**, not just the pipeline:
  `networkPeers` (`../lib/networkPeers`) splits `c.peers` into the active
  deployment first (in layer order, `dashboard.groupDeployed*`) and the rest of
  the network after (sorted by hostname, `dashboard.groupOutside*`) - a machine
  connected without a slice is exactly the one that disappears when only the
  pipeline is looked at. The cards come from `PeerDetailCard`
  (`../components/PeerDetailCard`), the same component the Flow canvas opens on
  double-click; `GroupHeading` renders the two group labels and `otherRangesFor`
  feeds `RemoveNode` the ranges of the *other* pipeline peers. Empty state is
  `c.peers.length === 0` (was `peers.length`) with `dashboard.noMachines`
  ("Nenhuma máquina na rede"). Derives the active `deploy_id` from
  `useDeployments` and passes it to the add/remove dialogs. The add dialog
  fetches the selected target's catalog and cache options from
  `POST /api/ui/deploy/node/options`; it no longer uses a fixed cluster plan.
  The add trigger, dialog and `UndeployPipeline` are
  rendered only for `profile: "admin"` (from `useProfile()`, the one source -
  the context is fed by `/api/ui/node`). The details view itself is gated the
  same way, by `canSeePeerDetails` (`../lib/access`): outside `admin` the view
  is forced to the Flow **and the `ViewSwitch` is not rendered at all**, because
  a single available view is not a choice. Uses
  `../components/{LayerRibbon,PipelineRail,PeerDetailCard,UndeployPipeline,AddNodeDialog,ModelNetworkCanvas,ui}`,
  `../lib/api`, `../lib/networkPeers`, `../lib/queries`, `../lib/types`
  (`PeerView`, `DeployPlan`). The per-machine actions that used to live here
  moved to `PeerDetailCard`: `RemoveNode` (out of the model, only when serving
  a slice); leaving the network is not offered there. The
  development-only update-and-restart action is not rendered in the card. Next to
  "Adicionar nó" ("Add node") there is a `UndeployPipeline` button for an administrator
  when a model is live - `undeployTargets` is the list the confirmation shows
  (peers on `group` with `total_layers > 0`, sorted by start layer). `group` is
  the model selected in the Flow's bar (`pickedModel`, resolved by
  `selectedCanvasModel`), not always the active one: Add, Undeploy, their
  `deploy_id` and the properties panel follow the model on screen; the node
  re-scans the cluster before acting, so the list is just the preview. The flow
  canvas receives the
  directional RTT measurements for its adjacent routing edges and complete
  pipeline routes from `useNetworkRtt`, with the same badge presentation used
  by deployment. A whole-model deployment with one head still queries this
  endpoint even though its graph has no routing edge.
  The deployment's active version can also be opened in a read inspector
  on the right, using the same layout and the same values configured in the Deploy
  Studio. On the `admin` profile, `DeploymentLogDock` stays collapsed in the footer and
  expands upwards with the logs in real time and auto-scroll.
- In `Dashboard`, node and infer profiles do not reuse the administrator's
  cluster verdict: they show the local model, its real layer slice and local
  in-flight request count. A missing `/api/ui/node` response is rendered as
  unavailable, never as an empty model, incomplete pipeline or zero.
## Deploy
- `DeployModel.tsx` - "Deploy on the network". Full-bleed Deploy Studio with a
  top mode bar, separate technical model and operational deployment name,
  central peer map and compact property inspector. Tensor mode, Simple/Complete
  strategy and "Voltar ao plano do nó" ("Return to the node's plan") are visible only when `pom_dev_mode` is true.
  (`../components/DeploymentInspector`, the same component the History uses
  in read-only) and a draggable bottom peer strip. What stays in
  `DeployModel` and did not move to the inspector are the sections only the deploy
  screen has: the TP group, tensor parallel and the machine tab. Three shapes, chosen by the
  mode bar at the top and carried to the node as `kind`: **a set of shards**
  (below), **a whole model** (one model picker from `../lib/wholeModels`, with
  the deployment map as its machine choice and no "included"/merge; the
  coverage ribbon shows once a second machine is on the map), or **tensor parallel** (`tensor_parallel`): when the catalog
  has `tp_parts`, the picker groups the round by TP width and the inspector
  shows one-based ranks such as `1of2` and `2of2`; otherwise it uses the
  whole-model runtime path. Canvas membership is canonical: rows plus preferred
  peers, with every live peer added by the Complete strategy except in whole
  mode, where the operator adds each machine. Tensor selectors
  reuse those map cards; pre-sliced TP accepts any mapped peer because the node
  can transfer the needed rank, while whole-model TP filters mapped cards to
  peers that hold the local GGUF. Eligible Tensor cards expose the accessible
  primary toggle, which removes that node from workers.
  `size` is derived from the selected TP group or `workers.length + 1`, never a
  typed number, and the node derives the reduce URL from the primary's
  inference port (the operator no longer types `rank0_url`). `tp` is in the
  plan queryKey so changing primary/workers re-runs validation. A plan with no `kind`
  is read as `"shards"` (`kindOf`), so an older peer mid-upgrade does not
  silence the screen. Deploy memory is namespaced `whole:<name>`, because a
  shard set and a model routinely share a name. Tests:
  `DeployModel.whole.test.tsx`, `../lib/wholeModels.test.ts`.
  In whole-model mode, the inspector does not repeat the GGUF's facts nor offer a
  machine selector: the map is the only choice. When switching from slices to that
  mode, the screen clears the map and lets the operator add the machine they want.
  The first machine takes the whole file. A second one enters without a range
  and gets one in the Machine tab, among the ranges it holds: the whole file
  again (a replica) or a slice of the same model. The node accepts the map while
  the ranges cover every layer, and with two distinct ranges the plan is a
  pipeline (hidden-state precision included). Under an inference service the
  whole model stays on one machine: a second one is refused with a warning.
  On the first tensor request, `tpPayload` uses the first rank-0 holder as a
  provisional primary so the map can load; workers remain opt-in map members.
  The model list may appear before the reading of the GGUF headers finishes.
  That global state does not disable the screen: the plan is what decides whether the current
  selection has the metadata it needs. If a selected range still cannot
  be resolved, the backend returns a single `blocked` specific to that
  selection; a model not chosen, an absent peer or a peer without the
  `metadata_complete` field does not block the deployment.
  For a real MoE model, identified by `DeployPlan.is_moe` from GGUF metadata,
  the Global inspector shows the inference engine selector and sends
  `RuntimeParams.inference_mode` in whole-model, sliced and TP requests. Dense
  models hide it. A MoE Tensor Parallelism plan keeps the selector visible but
  is blocked with the backend's explicit unsupported reason, so the UI cannot
  imply that the request will use another engine silently.
- `DeployModel.tsx` - "Inference service": when a machine publishes a plugin
  service of kind `inference` (`lib/inferenceServices.ts`), a select next to
  the mode bar offers it beside llama.cpp. Choosing it fixes the whole-model
  mode, lists only the whole models of the architectures the service declares,
  keeps only the machines running it in the peer strip, sends
  `params.inference_service` with the plan and the deploy, and reduces the
  Globais inspector to the settings the engine reads
  (`DeploymentInspector.inferenceService`) plus, in a separate card, the POM
  settings the service declares (`pomSettings`). The Machine tab leaves out
  GPU layers and KV cache on the GPU, which only llama.cpp reads. Deploy memory is keyed per
  service. Test: `DeployModel.service.test.tsx`.
- `DeployModel.tsx` - the slice shape: picks a slice set (`group`), lets the
  planner place ranges on the central map, and exposes a 19rem collapsible
  inspector. Its colored global section includes context, batching, sampling,
  repetition, cycle detection, fit-memory, hidden-state dtype, execution toggles and the installation-wide KV cache
  types. The hidden-state selector appears only for a sliced pipeline crossing
  peer boundaries and offers f32/f16/f8/i4, with f16 as the default; the route
  test checks that f32 reaches the deploy request. The two K/V cache combos apply changes immediately through
  `POST /api/ui/cache-types`; the save button, subtitle and `global` badge are
  intentionally absent. The MTP toggle seeds `mtp_min_probability` at `0.75`; the operator can
  then edit that probability before deploying. The machine section includes
  The MTP section also exposes the safe-by-default RAM fallback switch, so the
  operator can refuse CPU drafting after measuring it. The machine section includes
  GPU/KV overrides, machine capacity and the available layer ranges that can
  be assigned to the selected peer. Choosing a range replaces that peer's
  active range; the same range on a second peer remains a replica. Coverage is
  a compact footer indicator. The inspector uses Global/Machine tab bars.
  The inspector starts directly with the Global and Machine tabs, without the earlier
  title and without external padding; capacity alerts appear as amber icons
  beside the control and open the whole message by hover, focus or
  click, including when the source carries identifiers without spaces.
  `fit_memory` is on by default for shards and whole-model deployment, and the
  screen disables it for tensor parallelism because llama.cpp cannot auto-fit
  `split-mode tensor`.
  Each numeric button of the inspector carries an `InfoTip` (`[i]` on hover/focus/
  touch) with what the parameter does and how it affects the inference; the controls that
  used `<label>` became a `<div>` with an `aria-label` on the `<input>` because a
  `<button>` inside a `<label>` breaks the accessible name.
  The map's cards are a **preview of the post-deploy**, not of the current state: with
  `machine.capacity` in the plan, they show the projected VRAM per GPU, recalculated
  on every context/batch/sessions adjustment (same debounce). Three states:
  green (it fits), amber (`degraded` - the raw count does not fit, but `fit_memory`
  can trim the request; it is a heuristic), pink (`!fits` - the count does not fit without
  the automatic adjustment). Amber and
  pink open a `Confirm` (`capacityAsked`, separate from the partial-coverage one)
  naming the machines and the reason, with a title/text per case;
  `canDeploy` does not change - it is a warning, not a block.
  A third window (`inServiceAsked`, a `Modal`) comes last, when the live
  cluster shows machines this deploy touches: the machines of the plan that
  serve any model are listed as taken over ("Replace and deploy"), and the
  machines outside the plan that serve this same model get the choice between
  "Undeploy and deploy" and "Keep in parallel", which sends
  `keep_existing: true`. Before it, when `DeployPlan.copies` is not empty,
  "Authorize the copies" lists each file, range, source and target; only its
  confirmation sends `allow_transfers: true`, and a 409
  `transfers_not_authorized` reopens it with the node's list. The confirms
  chain through `attemptDeploy`.
  Topology edits are debounced 260 ms and validated by
  `POST /api/ui/deploy/plan`. Runtime parameters, per-machine GPU/KV overrides
  and the operational name stay local while editing: their changes do not
  restart that cluster-wide survey or reuse its capacity projection. The plan
  query key contains only layer ranges and target machines, while the deploy
  request carries the current overrides. `POST /api/ui/deploy` re-surveys the cluster and
  validates the current parameters before acting, returning a job shown through
  `useJobs`. A failed preflight keeps the backend's reason visible in the rose
  job banner, including the peer that stayed occupied.
  Restores the last deploy from localStorage via
  `../lib/deployMemory` (`recallDeploy`/`rememberDeploy`/`usableSelection`),
  after the first plan, pruned against what still exists.
- The TP inspector never offers pipeline layer buttons. It lists available and
  selected ranks, while `DeployPlan.tp_targets` is the backend source of truth
  for the exact pre-sliced file and rank destination.
- `DeployModel` sends `strategy` and `preferred_node_ids` to the planner;
  `deployment_name` and `RuntimeParams` are sent with the deploy request. The
  map is explanatory: capacity/load come from the live
  cluster and RTT comes from the directional survey in the plan;
  adding a peer without a range only adds its card and keeps existing positions;
  in whole-model mode, another peer joins the map without a range, except under
  an inference service, where it is refused with a warning;
  in pre-sliced tensor mode, adding a peer makes it eligible for a rank even
  without a local rank file, while whole-model tensor mode still requires the
  local GGUF; neither path adds an off-map peer implicitly;
  assigning a range is what validates a new selection. Choosing a range removes
  an empty placeholder for that same range, while an already assigned copy on
  another machine remains as a replica. Removing a card deletes
  that peer's routes and returns it to the bottom strip. `DeployNetworkCanvas` reuses the controls of
  the Dashboard's canvas, draws nodes per range, bridges only between consecutive
  stages and alternatives with a directional RTT, steers the curve by the facing
  sides of the cards, keeps the GPU/RAM/load metrics updated by the cluster and does
  not repeat peers already drawn in the footer. On a narrow viewport, the map gains
  a minimum height and internal scrolling so it does not cut cards nor create overflow on
  the page. Dragging or double-clicking adds
  an available peer to the plan; assigning an available range edits
  only that selection, keeps the ranges already on the peer as explicit
  replicas and goes back to the backend for validation.
- The list of slicing rounds uses `ShardSet.group` as the technical key and
  `model_label` as the text; when there are two rounds of the same model, the selector
  shows the `pom_id` prefix to disambiguate without changing the group sent to
  the backend.
- The global options conditioned by the model use `DeployPlan.capabilities`:
  `unsupported` stays hidden; `unknown` stays visible, disabled and
  explains the source and the reason. `no_embeddings` is the product exception:
  it stays visible, and the backend validates the topology atomically per peer before
  applying the control.
- The inspector's closing edge is a visual bar 3px tall at full height, with a
  larger and invisible click and focus area. It sits outside the content's scroll,
  so it stays anchored to the screen height during scrolling and resizing; when closed,
  the reopening control stays in the existing compact column.
- `ImageDeploy.tsx` - `/deploy/image` (admin, full-bleed), reached from the
  Deploy screen's mode bar ("Image model"): an image model by component. Per
  component (diffusion, text encoder, VAE) a file picker over the network's
  `image_files` (`useClusterCatalog`; a file is name + size, a `checkpoint`
  fits all three) and a machine picker of the machines that hold that file and
  publish a kind `image` service (`useCluster` `plugin_services`). Choosing the
  diffusion file pre-fills the other two when its `<org>/<repo>` folder holds
  exactly one of each, and the name. Deploys with `POST /api/ui/image-deploy`;
  below, `useImageDeployments` lists what is served with each component's
  status and Undeploy behind a `Confirm` (`POST /api/ui/image-undeploy`).
- `Deployments.tsx` - it lists the deployments registered in the cluster through
  `useDeployments`, tells "no ar" ("live") apart from "padrão" ("default"), orders the history by the
  last activation (`deployment.activated_at`) and shows the "Última
  ativação" ("Last activation") column. A full screen, like Logs':
  a fixed header, the table scrolling inside and, when a version is chosen, an
  inspector on the right (`lg:grid-cols-[minmax(0,1fr)_minmax(19rem,24rem)]`).
  An active deployment with every live peer idle appears as
  `idle`, separated from `noAir`.
  Clicking a version's row opens the `DeploymentInspector` in `readOnly` with
  `version.params` - the same inspector of the deploy screen, without duplicating the
  property list. The KV cache does **not** enter there: it belongs to the installation and to
  the moment, not to the version, and showing today's value beside what came up yesterday
  would be inventing a record nobody kept. Each row expands
  (`VersionList`) and shows `deployment.versions` from the newest to the
  oldest, with the "Ativa" ("Active") badge on `active_version` and a button "Reimplantar esta
  versão" ("Redeploy this version") (`redeployDeployment(id, { version })`). "Reimplantar com novos
  parâmetros" ("Redeploy with new parameters", on the row) and "Editar e reimplantar" ("Edit and redeploy", in a version's
  inspector) open on the right, in place of the read inspector, the `RedeployPanel`: the same
  editable `DeploymentInspector` of the deploy screen, seeded with the
  active version or with the version opened (not with `nodes[0].config`), plus the per-machine
  adjustments (`n_gpu_layers`, KV on the GPU) and "Recarregar com estes valores" ("Reload with these values"), which
  sends only `params` + `per_node` - the backend decides whether to reuse or append a version. The
  right side shows one thing at a time. It also
  activates a deployment and undeploys it by `deploy_id`. The redeploy job's
  panel shows `job.outputs` (`"<host>: <motivo>"`) below the message.
  Test: `Deployments.test.tsx` (expansion, the active badge, `{version}` in the API).
## Logs, keys and the audit
- `DeploymentLogs.tsx` - the full-bleed console of the cluster's process log on
  `/deployments/logs`. It queries the admin's local replica, refreshes every
  second, keeps the arrival order the API returns and can filter by
  `deploy_id`; while the node reports `cluster_logs_enabled: false` it shows a "Collection off" badge and a notice (with a link to Administrative preferences for the principal) instead of "Live", since no new line arrives; it keeps only the black terminal, follows the end while the
  reader is at the end, pauses the follow when scrolling up, offers a visible state button to
  reactivate it and copies the last 500 lines. It is not the
  request audit, which belongs to the `enterprise` plugin.
- `Keys.tsx` - Two screens in one file. `TokenKeys` delegates the full-bleed table
  `ApiKeysTable`: per-node API tokens for
  harnesses - `GET/POST /admin/api-keys`, `DELETE /admin/api-keys/:id`. The
  list carries only `key_hint` and the tracking id; the secret appears once,
  in the `IssuedSecret` window right after creation.
  On the `visitor` profile, it shows only this node's active key
  and, when generating a new key, revokes the previous one. The API key routes are
  `infer+node+admin`: they are the credential of whoever serves `/v1`, and every profile
  serves. `ConnectionKeys`: the cluster operator key, read from
  `NodeInfo.connection_key`. Seeing the key belongs to whoever operates this machine - it already
  is in that machine's `pom.json` and the screen only exists behind the administrative boundary.
  **Changing it** (`POST /admin/regenerate-key`) is another thing: the key is the same
  across the whole cluster and rotating it cuts every other node, so the button only
  appears when `authority_level` is present. A token belongs to the node that
  minted it: it works there only, never leaves it, and survives a change of
  network, so the screen has no node picker.
- Request Logs and Statistics **do not exist here**: they are screens of the
  `enterprise` plugin, delivered by `pom-plugin-logs` through the plugin UI
  mechanism (`docs/subsystems/plugins-ui.md`). `App.tsx` shows the fallback
  `../components/EnterpriseFeatureShowcase.tsx` when the plugin is not active.
## Settings leaves
- `ModelPaths.tsx` - edits `model_dirs`, `shard_dir` and `shard_max_gb` via
  `POST /api/ui/paths`. The Storage screen (`Armazenamento`) rejects a slice folder equal to
  or inside a whole-model folder, shows current usage and the slice limit, and
  keeps `/config/folders` as an alias.
- `NetworkSettings.tsx` - This node's membership and network information
  screen. Its `NetworkParticipants` export reuses the known-node area at
  `/config/network/participants`; the route is allowed only to `admin` and
  `worker`, and block/unblock mutations remain admin-only. Each peer row shows
  the route actually in use (`AdminNode.route`: `RouteBadge` + `routeAddress`),
  never the advertised `addr`, which only goes into the row's `title`, and the
  POM release it announces (`PomVersionBadge`: amber when it is not this
  node's, the build commit beside it only in development mode). A
  relayed route names the relay kind (`relay_kind`: dedicated relay or member
  relay, the last rung of the ladder).
  (`POST /api/ui/leave` / `/api/ui/rejoin`, reachable by every profile: leaving
  is abdication, not a privilege) and the known-node list from
  `GET /admin/nodes` (`node+admin`: reading the roster is what this screen
  shows). Block/unblock (`POST /admin/nodes/:id/block|unblock`) stays
  `admin-only` and the buttons are rendered only when `authority_level` is
  present - `profile: admin` alone grants nothing. The "Papel" ("Role") row was replaced
  by the effective authority: the old `NodeInfo.role` left `/api/ui/node` with
  the trust model and was rendering an empty cell.
  The `visitor` profile also opens `/config/network`, but the backend returns only its
  own machine. The screen no longer offers turning a `visitor` into a
  `worker`: the role is what the network admitted the machine as, and changing
  it means leaving and asking for admission again. The card says so and has
  no action. A `worker` has no step down to `visitor` either. Both node routes
  are gone: roles are set at admission and changing one means rejoining. O
  administrador principal pode promover um nó via
  `POST /admin/nodes/:id/promote-admin`.
- `AdminAuthority.tsx` - Admin authority screen backed by
  `GET /api/ui/network/admin-authority`. It shows the principal, secondary
  administrators, promotion chain and resignation eligibility. Its mutations
  are isolated in `lib/adminAuthority.ts`; pending principal-transfer offers
  are answered by `Notifications`.
- `TrustAnchor.tsx` - the machine's trust root and the two sides of anchoring
  the authority, behind `GET/POST /api/ui/network/trust-anchor`. It shows the
  anchor's fingerprint in groups of eight with a copy button, and the state
  **anchored** or **to anchor** at a glance. On a node still without a root, it shows the
  form that pastes the signed chain, computes the fingerprint and keeps the
  explicit confirmation of the check through the trusted channel. Outside loopback the form gives way to the
  explanation; the decision stays with the server, which returns a stable `reason`
  (`not_loopback`, `already_anchored`, `invalid_document`,
  `fingerprint_mismatch`, `install_failed`) so the screen translates the refusal
  instead of repeating the server's text. It lives outside the profile gate - see
  the file's header and `lib/access.ts`. Test: `TrustAnchor.test.tsx`.
- `Preferences.tsx` - Local settings screen under Settings (`Configurações`): owns the
  administrative exposure mode and gateway port (`POST /api/ui/access`, the
  principal's; the panel hides where the node refuses it), and refuses the
  network mode until a password exists, linking to the password screen. The
  "Panel password" card, on every profile, opens `/config/password`
  (`components/ChangePassword.tsx`). It also mounts `HttpsCertificatePanel` (the node's
  local CA, for every profile), `ThermalPanel` and
  `HuggingFaceSettingsPanel`; there is no TLS panel, since TLS between nodes is
  always on. It also mounts the cluster logs switch,
  available to every admin (`cluster_logs_authority`) and applied in real time, and `QuicPanel`, which
  toggles the HTTP/3 preference in `POST /api/ui/network/quic` with the same
  authority control. The access password is a human credential and is not the
  cluster `connection_key`. The route `/config/preferences/admin` renders
  only QUIC, process logs between nodes and the network chat, besides the
  local discovery switch, off by default; the normal route keeps
  exposure, Hugging Face and thermal protection; Storage only has a menu entry.
- `NotFound.tsx` - 404 fallback with a link back to `/`.
- `Forbidden.tsx` - the screen the active profile does not open. It is not `NotFound`: the
  address exists, what is missing is the profile, so the text names the active profile and
  tells you to use the authorized node instead of telling you to look for a route fault. Who
  picks it is the guard in `../App.tsx`, through the table in `../lib/access.ts`.
## Models, slicing and transfers
- `SendFile.tsx` - offers a `.gguf` from the local catalog via
  `POST /api/transfer/send` (`{to_node_id, filename}`). The UI displays the
  friendly name and full path, but sends only the basename; the backend resolves
  it again and the receiver sees only an opaque grant. It is `SendFilePanel`,
  the first panel of `Transfers.tsx`, not a screen of its own; `?filename=`
  preselects the file, and `/transfers/send` redirects to `/transfers` keeping
  the query.
- `Shards.tsx` - "Fatias": every local slice always listed (the Nodes screen
  only shows the difference), with who is missing it. Pipeline `parts` and TP
  `tp_parts` are separate sections; TP groups are ordered by group size and
  rows use the one-based labels `1of2`, `2of2`, and so on, with the zero-based
  rank shown alongside. Sends via `POST /api/transfer/send`; deletes via
  `../lib/deleteModel`. `uniquePeerName` (`../lib/peerName`) disambiguates same-hostname peers with
  the node_id prefix. Full-bleed shell; each `ShardCard` carries the set's
  vision / MTP link editor (`SidecarLinkEditor`), with the compatible files of
  every machine from `useClusterCatalog`.
- `SliceModels.tsx` - "Fatiar": full-bleed shell. Pipeline and Tensor
  Parallelism are a segmented tab pair (`role="tablist"`), not a checkbox -
  each mode owns its own controls (Pipeline: size list, one explicit layer
  range, or Tensor Parallelism's rank count). The Tensor Parallelism tab only
  mounts when `useNode().data.pom_dev_mode === true` (the same
  `POM_DEV_MODE=1`-gated pattern as `DeploymentInspector`);
  without it the screen never offers or sends `mode: "tensor_parallel"`, and an
  effect snaps a stale selection back to `"pipeline"` if the flag turns out
  false after the first render. Pipeline builds a by-size plan
  (target GB per slice, empty = "the rest"); a single range uses the GGUF
  architectural starts and ends; TP chooses 2..64 ranks and previews one full
  layer range per rank, labeled `1ofN`. The modes use the debounced
  `POST /api/ui/slice/plan` and start through `POST /api/ui/slice`; TP sends
  `mode: "tensor_parallel"` and `tp_size`, while pipeline sends `slices` or
  `range`. `SlicingJobsPanel` keeps running, completed and failed jobs visible,
  including named phase, bytes and partial output paths.
  For architectures with a required non-final head, the plan includes
  `required_head_bytes`, and the preview identifies the real payload repeated
  in each intermediate slice. `asPlan` keeps this field optional for older
  nodes and for tensor-parallel plans.
  `asPlan` validates both response shapes. Only whole models (`isWholeModel`)
  can be sliced. Multipart models show the summed physical size, every
  declared part and an explicit instruction to keep the set together before
  POM slicing; incomplete sets surface the backend reason and leave the start
  action disabled.
- `Transfers.tsx` - The one Transfers screen (a single menu entry): the send
  panel (`SendFilePanel`, from `SendFile.tsx`) first, then remote model downloads from `useModelDownloads` in
  the same in-progress screen as pending offers (accept/reject via
  `POST /api/transfer/offers/:id/accept|reject`), active transfers (bytes +
  speed + ETA + cancel via `POST /api/transfer/jobs/:id/cancel`), and finished
  ones. Model downloads reuse `ModelDownloadsPanel`, so their backend progress,
  configured destination path and cancel/retry actions stay visible here.
  State classification from `../lib/transferStatus`. `cancelled` is
  shown as **refused**, in amber, and not as a red failure: it was someone's decision,
  not an error. Slices appear by model and layer range
  (`../lib/transferLabel`), with the short hash beside them. Full-bleed shell; offers,
  active and finished ones are each a full-width table instead of the earlier
  list of cards, with a fixed header and actions in the last column.
- `WholeModels.tsx` - Full-model catalog from `useLocalCatalog`, one full-width
  table row (`ModelRow`) per model with actions: slice (links to
  `/models/slice?path=`), send (`/transfers?filename=`), delete
  (`../lib/deleteModel`). A dedicated "Vision (mmproj)" column shows the
  projector's basename (`../lib/format::optionalBasename`) when
  `DiscoveredModel.mmproj_path` is set, or "no projector" otherwise - the
  catalog already resolves this file from GGUF identity and projection dimensions,
  the screen only renders it; the cell's "Vision and MTP" button opens the
  same `SidecarLinkEditor` as Shards in a window. `ScannedRoots`
  lists the scanned folders and per-folder file counts. Full-bleed shell.
  A `Tabs` pair under the header switches between "LLM (GGUF)" and "Image"
  (`?kind=image` keeps the choice across a reload); the Image tab lists
  `CatalogResponse.image_files` (`ImageModelRow`: name, path, source,
  translated category, family, dtype, size, delete only - image files are not
  sliced, and Send stays GGUF-only until the transfer gate accepts them).
- `RemoteModels.tsx` - it shows trending GGUF models through the public Hugging
  Face API on opening, without credentials; the authenticated search uses debounce in
  `/api/ui/model-provider/search`, with a filter, sorting, pagination by URL,
  lazy details and a download confirmation. The login status stays compact
  inside the search bar, with an explicit Search action. The
  detail resolves the license through the provider and marks gated models before the
  download. The dialog shows the model folders resolved by the backend. The
  jobs appear in the `ModelDownloadsPanel`, also in Transfers, so changing
  the route or reloading does not lose a transfer. Full-bleed shell; the
  results are a full-width table (name/tags, state, downloads,
  likes, updated, action), no longer a grid of cards - `RemoteModelCard`
  was removed for being unused.

Key logic carriers:

## Cross-cutting behaviour
- `ChatSessionProvider.startStream` (`lib/chatStore.ts`) - snapshots turns
  before the placeholder reply, streams deltas into `consumeChatDelta` (keeps
  reasoning/content split), merges `x_pom` metrics via `finishChatStream`,
  measures browser-local first chunk/token and delivery timing, and persists
  the partial session on a 50 ms scheduler while the stream is active. The
  final or interrupted state is flushed immediately. Route unmounts do not
  abort it; `stopStream` and a new message do.
- `components/TelemetryLine.tsx` plus the `chat.telemetry` slot - each answer
  shows the basic one-line telemetry; when an active plugin claims the slot,
  `Bubble` mounts its component at the end of that line with the props of
  `lib/chatTelemetry.ts` (trace, timings, source, captured environment,
  `onAsk`). Only then does `ChatScreen` fetch the per-node network RTT and
  runtimes for the environment. `onAsk` sends the plugin's question as a turn
  whose visible text differs from the request content.
- `DeployModel` and `Deployments` both edit the runtime knobs through
  `DeploymentInspector` (the Deploy Studio inspector, and the History redeploy
  panel), so a new property cannot reach one screen and miss the other.
  Both screens read `NodeInfo.pom_dev_mode` from `useNode`; the Deploy Studio, the history's
  inspector and the redeploy panel fail closed. The flag only
  hides the local development controls; the state of planning,
  launching and redeploying keeps the values received from the deployment,
  even when this node cannot edit them.
- `DeployModel.rowsOf`/`sameRows` (DeployModel.tsx) - the selection model:
  merged steps expand back into per-slice rows and the planner answer is compared
  with the displayed rows after sorting by layer.
- `DeployModel.validated`/`syncing`/`answered`/`whyNot` (DeployModel.tsx) - why
  the button is grey, and who is allowed to say so. `sameRows` compares both
  sides **sorted by layer**: the screen keeps its rows in layer order and the
  node builds its answer its own way, and comparing position by position made a
  plan that described exactly these rows look like a different plan. The
  explanation panels hang off `answered` (the node has replied about *these*
  rows), never off `validated`, so every planner answer can explain the disabled
  button. `whyNot` puts the one-line reason next to
  the disabled button, including the two states with no panel at all (nothing
  selected, answer still in flight). Regression test: `DeployModel.test.tsx`.
- `DeployModel.deploy` (DeployModel.tsx:602) - posts the current rows/settings
  (from `rows`/`eff`, not the nullable edit state), remembers them via
  `rememberDeploy`, and tracks the returned job id.
- `SliceModels.asPlan` (SliceModels.tsx:56) - shape guard on pipeline and TP
  plan responses; the one screen rendering straight off a POST body.
- `Setup.save` (Setup.tsx) - posts the complete first-access payload, including
  trust document, typed fingerprint, resource policy, storage and network
  visibility, then reloads after the node respawns.
- `ModelPaths.save` (ModelPaths.tsx:55) - trims/filters the folder list, posts,
  and re-syncs state from the server's answer (which is authoritative).

Backend endpoints used (see `backend/webui/` / `backend/gateway/` / `backend/openai.rs`
in the repo root): `/v1/chat/completions`, `/v1/models`, `/api/ui/cluster`,
`/api/ui/cluster/models`, `/api/ui/node`, `/api/ui/models`, `/api/ui/paths`,
`/api/ui/jobs`, `/api/ui/requests`, `/api/ui/requests/stats`, `/api/ui/deploy/plan`, `/api/ui/deploy`,
`/api/ui/deploy/node`, `/api/ui/deploy/node/remove`, `/api/ui/leave`,
`/api/ui/rejoin`, `/api/ui/release`, `/api/ui/release/candidate`,
`/api/ui/unload`, `/api/ui/undeploy`, `/api/ui/dev/update`, `/api/ui/slice/plan`, `/api/ui/slice`,
`/api/transfer/send`, `/api/transfer/jobs`, `/api/transfer/jobs/:id/cancel`,
`/api/transfer/offers`, `/api/transfer/offers/:id/accept|reject`,
`/admin/nodes`, `/admin/nodes/:id/block|unblock`, `/admin/api-keys`,
`/admin/regenerate-key`, `/api/setup/hardware`, `/api/setup/models`,
`/api/setup/admin-anchor/preview`, `/api/setup/health`, `/api/setup/save`,
`/api/public/networks`, `/api/public/network`,
`/api/ui/model-provider/search`, `/api/ui/model-provider/details`,
`/api/ui/model-provider/storage`, `/api/ui/model-provider/downloads` and
`/api/ui/model-provider/downloads/:id/{cancel,retry}`.

## Multimodal deploy and chat

The deploy side (the head's `mmproj_candidates`, `vision_choice` / `mtp_choice`, what the plan and
the deploy carry) is `docs/subsystems/http-api.md`. The Chat side (the file field, the resize to fit
the effective budget, the "Resized automatically" label, the `Think` button and dictation) is
`docs/subsystems/web-ui.md`, "Chat".
## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
