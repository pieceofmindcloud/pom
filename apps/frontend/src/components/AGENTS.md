# components/

Reusable UI building blocks, split into two kinds. The small primitives live in
`ui.tsx` (the design system: panels, buttons, inputs, badges, meters, states,
formatters) and everything is composed from them. The bigger pieces are the
app chrome (`Sidebar`/`Topbar`/`Notifications`/`nav.ts`), the cluster
visualizations (`PipelineRail`, `ModelNetworkCanvas`, `DeployNetworkCanvas`), and the dialogs/guards
for destructive or remote actions (`Confirm`/`Modal`, `ErrorBoundary`,
`AddNodeDialog`, `ReleaseSlice`, `RemoveNode`, `LeaveNode`,
`UndeployPipeline`), and `PeerDetailCard`, the one card for a machine shown in
the Dashboard details tab and in the window the Flow canvas opens. Test files
present: `AddNodeDialog.test.tsx`, `Confirm.test.tsx`, `DeploymentInspector.idle.test.tsx`,
`GpuTemperature.test.tsx`, `LeaveNode.test.tsx`, `UndeployPipeline.test.tsx`,
`ModelNetworkCanvas.test.tsx`,
`PeerDetailCard.test.tsx`. Fake peers for these tests come from
`../test/fixtures.ts` and must have the *full* shape the node serves. A partial
`node` can slip past one component's `?.` and crash the next:
`PeerDetailCard.tsx` still reads `n?.status.state`, which guards `n` but not
`status`.

## Chat head choice

- `InferenceHeadChoice.tsx` - in the Chat, on any profile, it shows nothing
  when there is a single head available; with two or more it uses the same native combo as the model
  selector to inspect the layer-zero candidates. The options carry the name,
  id, RTT and freshness, the in-flight load, the range, the capacity and the mark of the chosen
  one; the criterion, the fallback and the incomplete pipeline stay in the menu's disabled
  options. An unknown load appears as "unavailable", never zero. With no candidate it
  keeps a clear warning. The component presents no peers outside that cut.
  The choice is controlled by the `ChatScreen`: `onChange` returns the selected `node_id`
  and the next call sends that value in `inference_head`.

- `setup-wizard/` - the reusable visual components of the first-access wizard: a layout with
  progress and actions, the intention, hardware, node, model and network cards, plus each
  step's screens. The navigation and the state live in
  `../routes/SetupWizard.tsx`; the components only receive state and callbacks.

Per-file list:

## First access

- `GpuTemperature.tsx` - a shared GPU temperature badge used by Flow and machine
  details. One machine may have more than one board: both cards show **one
  row per GPU**, named by `gpuDisplayName` (which appends `#index`
  when two boards have the same name, the case of a server with two RTX 5060 Ti).
  It uses `SystemLoad.gpu_kind` as the source of truth: `none` returns
  no markup, while a real GPU with a missing reading shows only the unavailable
  sensor message. Flow and machine details pass through this component so the
  no-GPU guard cannot be forgotten by a caller. Normal, attention, critical and
  unavailable-sensor readings use the existing design-system tones.

## Design system

- `ui.tsx` - the design system in one file on purpose. `Panel`/`PanelHeader`/
  `PageHeader` (surfaces), `Button` (4 variants, sizes, loading spinner),
  `Input`/`Select`/`Field`, `Badge`/`Dot`/`Progress`/`Meter` (the shared `Tone`
  palette: neutral/iris/aqua/amber/rose/sky/lime), `Empty`/`Skeleton`/
  `ErrorState` (renders the error message as the node wrote it),
  `Mono`/`ScrollX`/`Footnote`/`KeyValue`. Everything is token-based class names
  (e.g. `bg-iris`, `border-line`) with `clsx`, no runtime styling.
  `FullBleedHeader`/`FullBleedToolbar` are the shared contract for a
  full-viewport table screen (the one `Deployments`/`PublicNetworks`/
  `DeploymentLogs` already used before these existed): a fixed edge-to-edge
  header bar and, optionally, a filter strip under it, above a body that
  scrolls on its own. `WholeModels`, `Shards`, `SliceModels`, `RemoteModels`,
  and `Transfers` (`../routes/`) all build on these two instead of
  repeating the bar's markup; `Deployments`/`PublicNetworks` keep
  their own hand-written bar and were not touched, since refactoring a
  screen nobody asked to change is its own risk.
  `Tabs` is the one tab row (POM-JEV's folder tabs, theme tokens `--tab-*` in
  `index.css`, classes `pom-tab*`); every `role="tablist"` in the panel uses it
  (`fill` spreads the tabs over the row), and plugins get it as `ui.Tabs`. An
  `actions` puts controls at the right end of the row (the tabs scroll, the
  controls stay). An
  item with `onClose` gets a close button beside the tab (`pom-tab-closable`,
  siblings, never a button inside a button). A label that can be longer than a
  `fill` tab (the machine name in the Deploy inspector's tab) goes in `MarqueeText`:
  clipped to the tab, it scrolls right to left in a loop only when it does not
  fit, rests at the start of each lap, pauses under the pointer and stays still
  with an ellipsis under reduced motion (`pom-marquee*`; the rest is
  `MARQUEE_REST`, in step with `@keyframes pom-marquee`).
- `AlertTooltip` (ui.tsx) - `compact` makes the icon the size of the `[i]`, without
  a frame, so it fits in the `DeploymentInspector`'s icon column; `Meter` and
  `KeyValue` gained `compact` for the same reason in the machine card. It is the compact
  warning used in the inspector: the amber icon is
  focusable and triggerable by focus or click, and the full text is rendered in a fixed
  portal, with an explicit word break for long messages and for ids without
  spaces. `InfoTip` is the neutral version (an `[i]` icon, without the weight of a warning) for
  explaining a parameter on hover/focus/touch; the two share
  `usePositionedPopover` (state + positioning of the portal bubble), and so
  does `IconTip`, whose trigger is the icon it explains (a click on it does
  not reach a clickable row). `Field`
  gained `tip?: string` - it renders an `InfoTip` glued to the label's line,
  **outside** the `<label>` (a `<button>` inside a `<label>` breaks the control's accessible
  name), so the `<label>` keeps wrapping only text and control. `hint` (visible text) keeps
  existing for the other screens.
## Plugin screens

- `EnterpriseFeatureShowcase.tsx` - the advertising fallback of `/logs` and
  `/statistics` **when no active plugin provides them** (rendered outside
  `<Routes>` in `App.tsx`, like the chat). It receives `kind: "logs" | "statistics"`.
  It is the sales page of the logs plugin (Request Logs, Statistics and Quota, one purchase, aimed at the network's admins) in the pieceofmind.cloud look (the
  `.pom-store*` styles in `index.css`): the name, description,
  icon, official screenshots and price come from the license-server's public catalog
  (`usePublicPluginCatalog`), with local SVG text and previews when it does not
  answer; the price is formatted in the interface's language
  (`lib/pluginPricing.ts`) and follows how the site sells the plugin: with a
  running promotion, the full price struck through above the discounted one,
  a "Sale -N%" tag and the promotion's terms; with `sales_mode: "contact"`
  ("talk to sales", set in the license server's console), no price, the
  first step reads "Talk to our team", and the button opens the site's
  contact form about the plugin (`/contato/?plugin=`) instead of the wizard.
  "Buy now" and the footer link open the site's purchase wizard (`/comprar/`,
  Paddle) in a new tab with the plugin, the billing period and `lang`; sold by
  contact, the footer link goes to the plugin's card in the site's store. While no plugin
  serves `/logs` or `/statistics`, `nav.ts` folds the Logs section and
  Statistics into one "Business Pack" entry. Tests in `EnterpriseFeatureShowcase.test.tsx`.
- `PluginBridge.tsx` - mounted once in the shell (`App.tsx`). It publishes
  `locale.changed`/`theme.changed`, forwards the node's events read every 3 s
  in `GET /api/ui/plugins/events`, runs the plugins' commands (a local toast, a `plugin_notice`
  warning to the network through the network Chat, a confirmation in the bell or in a dialog with
  `display: "dialog"`) and turns the plugin warnings received in the network Chat's snapshot into
  a toast and a `notification.received`. See `lib/pluginEvents.ts`.
- `PluginScreen.tsx` - loads the ES module of a plugin screen (an asset served
  by the node) and mounts it in POM's tree, with the plugin's styles and an error card
  if the loading or the rendering fails. The normalized route is part of the
  mount's identity so switching between screens of the same plugin does not keep the previous
  component. The screen is mounted under `PluginLoading` and stays out of sight
  (`.pom-plugin-pending`) until the plugin signals it finished rendering -
  `__POM_HOST__.ready()`, `lib/pluginReady.ts` - or the short wait ends; the
  wrapper then becomes layout-neutral, so the screen is never remounted. A plugin
  without the signal is opened by the wait. While the plugin holds a state in
  POM's loader (`lib/pluginLoader.ts`, armed in an effect of the first commit, before the
  screen module can mount) the screen
  stays covered whatever the gate says, and `PluginLoader` draws that state.
  See `docs/subsystems/plugins-ui.md`.
- `PluginLoader.tsx` - POM's loader and installer over a plugin screen, in the
  state the plugin asked for: the generic pieces (`PluginLoading`, with the
  plugin's title and message when it sent them), a `progressbar` with the
  percentage and an `aria-live` message, the error (`MissingPiece
  variant="failed"`) with "Try again" that publishes `loader.retry`, and the
  success (`variant="done"`). Tests in `PluginLoader.test.tsx`.
- `PluginUpdatesDialog.tsx` - the Plugins screen's "Check for updates"
  result: one row per update found (`UpdateItem`: POM, a licensed plugin or a
  GitHub install, with from/to versions, changelog, an unverified badge and,
  for POM, why it cannot be installed here), Install per row and Install all.
  POM is always last, since installing it restarts the node; each row keeps
  its own installing/installed/failed state. The checks themselves live in
  `../routes/Plugins.tsx` (`checkAllUpdates`); tests in `Plugins.test.tsx`.
- `PluginLoading.tsx` - the puzzle drawn in place of a plugin screen that is
  still opening (`MissingPiece variant="loading"`, named with `plugins.loadingNamed`),
  and in place of the advertising card while the node has not answered
  `GET /api/ui/plugins/ui` yet (`App.tsx`), so a plugin's screen never flashes
  behind a fallback.
- `TelemetryLine.tsx` - the open-source telemetry of a Chat answer: one compact
  line with generation speed and tokens, TTFT, end-to-end total, prompt tokens
  and their processing speed, and the pipeline (`source`). `basicTelemetry`
  derives those numbers from the timings or, without them, from the trace.
  Children render at the end of the line; the Chat puts the `chat.telemetry`
  plugin slot there. The detailed, visual telemetry is not part of the POM.
- `PluginSlot.tsx` - mounts the component an active plugin declares for a
  native extension slot (`slots` in `pom-plugin-ui/v1`), through the same
  module loader as `PluginScreen`. `usePluginSlot(slot)` resolves the claimant
  from the plugin registry; while loading, without a claimant, or when the
  module fails to load or render, the slot shows its fallback (nothing by
  default) so a broken plugin never breaks the host screen.
## Chat, history and shared tables

- `Markdown.tsx` - a memoized assistant-answer renderer: `ReactMarkdown` with
  `remark-gfm` + `rehype-highlight` (auto-detect language), inline vs fenced
  code split, every fenced block gets a copy button (answers here are mostly
  commands). Used by `Chat`. `compact` shrinks text, headings and block gaps
  together for side panels (the Networks screen's network panel).
- `ChatSessionList.tsx` - shared conversation history list. The Chat screen
  uses the wide variant; the `visitor` Sidebar uses the compact variant. The
  list owns the bounded `overflow-y-auto` region, active `aria-pressed` state,
  delete action and localized new-conversation button.
- `ApiKeysTable.tsx` - the full-bleed API keys table. Every key belongs to the
  node that minted it and works there only, so there is no node picker.
- `DeploymentLogDock.tsx` - the Dashboard's collapsed bar for the admin, which
  expands upwards to half the area and follows the deployment log in
  real time with auto-scroll.
- `OpenNativeChatWindowButton.tsx` - a macOS button used only when the
  WebKit bridge `pomOpenWindow` is available; it sits in the `Topbar`'s toolbar, not in the
  Chat's body. It is an icon-only control: the full name stays in the accessible
  name and in the tooltip.
  `QuitNativeAppButton`, in the same file, quits the macOS menu-bar app through
  the `pomQuit` bridge (the app is an accessory with no visible menu); it sits
  at the end of the `Topbar` and renders only inside the macOS app.
## App chrome

- `nav.ts` - `NAV`: the menu as data. The Deployments section uses `Rocket` (it was
  `Boxes`, the same icon as Models), the History uses `History` and Logs has separate
  leaves for requests and deployment; Network Chat is its own
  entry. The submenu does not
  draw a leaf icon, so `Rocket` in the section and in the `/deploy` leaf do not
  collide. The table itself is an ordered array of `NavSection`
  (icon + `to` or `children: NavLeaf[]`), each leaf with `labelKey`, route and
  `end` (whether the sidebar should match child routes too). The sidebar and the
  App router both consume it; the routes themselves are declared in `../App.tsx`.
  `navForProfile(profile)` returns the menu already pruned by the active profile, reading
  the same table (`../lib/access.ts`) that `App.tsx`'s route guard consults -
  a section with children disappears only when **no** child is left. It copies the section
  before filtering; rewriting `NAV` would leave the other profile with the pruned menu.
  `networkChat: false` drops the Network Chat entry (a visitor the network
  keeps out of its Chat, `useNetworkChatOpen`).
  Statistics is a first-level entry after Logs, keeping the operational
  sequence of consulting events before their aggregates. The same table marks the entries that go
  to the `visitor` footer and promotes the network to the main menu; `navForProfile` and
  `navFooterForProfile` are its two projections. Request Logs and Statistics stay visible to admin
  and node even without a license, for product discovery; the `visitor` profile sees them only
  once an active plugin serves them.
  Tests in `nav.test.ts`.
  The main My Network section (`nav.network`, "My Network", so it does not read
  as a twin of "Networks") contains Information, Participants, My Machine,
  Administrators, Sharing and API Keys. Usage is not in the menu: it is
  `UsageButton`, beside the bell. Settings contains Preferences,
  Storage, Administrative Preferences and About, always in this order; About
  is the last leaf and opens `/config/about`. Sharing is the single screen
  that reuses `TrustAnchor` for the trust root, this machine and the connection
  key.
  A plugin manifest may replace the Lucide icon with `icon_image`, a PNG
  served by the assets endpoint; `Sidebar` keeps the icon declared in the menu
  as a fallback if the image fails.
- `Sidebar.tsx` - `Sidebar` (desktop aside) and `SidebarContent` (reused by the
  mobile drawer in `Topbar`). `SidebarContent` reads `useProfile()` and renders
  `navForProfile(profile)`, not the raw `NAV`. `Section`/`Leaf` render it with an animated
  active pill; a section holding the current path stays open. A plugin item
  whose manifest is `on_demand` (`NavSection.pluginCode`, `usePluginMenuItem`)
  starts its process on click when it is stopped, and shows a `PluginCloseButton`
  X beside the link while it runs; the button sits next to the link, never
  inside it (a button inside an anchor is invalid). `PluginScreen` shows the
  paused puzzle piece (`MissingPiece variant="paused"`) with "Open <name>"
  instead of loading the module while the process is down. The desktop
  sidebar can be minimized and persists that choice in `localStorage`; the
  minimized state renders icon-only links and child flyouts on hover, focus or
  click. The admin/node nav is a `ScrollableNav` in both states: when the items
  do not fit the window it scrolls, with up/down chevrons shown only on the side
  that still hides items (`ResizeObserver` on the scroller and its content, a
  window `resize` fallback). The flyouts of the minimized state are `fixed` and
  anchored to their icon on enter/focus/click, clamped to the window bottom: an
  `overflow` ancestor clips an `absolute` flyout, but not a `fixed` one. Keep
  the scroller `overscroll-y-contain` so reaching its end cannot move the document. `SidebarBrand` is a 64px header and, on desktop, the lockup itself is
  the accessible expand/collapse button: only an explicit click or touch toggles it.
  In the mobile drawer it remains a passive lockup. The menu below it keeps a
  5px top gap so the first item does not touch that line - the hash sits **on the identity** because in
  a multi-machine cluster "which build is that?" is unanswerable without a
  shell, and a peer on an old binary looks healthy until inference.
  `SidebarUtilityControls` is the footer: beside the controls it shows the
  release the node runs (`NodeInfo.pom_version`, `v0.5.6`), with
  `· <build_commit>` only when `pom_dev_mode`; a newer release from
  `usePomCoreUpdateStatus` turns it amber and adds the "New version" link
  (`sidebar-new-version`) to About, where the update is applied. `SidebarUtilityControls` shows language as **one flag**, the current locale
  (`brand.flags[locale]`, from `brand.json`), rather than "EN"/"PT-BR" or a row
  of both flags: a click opens a submenu (`LanguageFlags.tsx`, `useLanguageMenu`)
  with the options, and picking one switches the language and closes it. The
  login gate and first-run wizard reuse the same menu in pill form
  (`BrandLanguageSwitch.tsx`). A text label only helps someone who already knows
  what the code means and the target here is whoever opened the panel in the
  wrong language - the language name stays in `aria-label`/`title`, because a flag is not a language to a
  screen reader. Plus the dark/light toggle (`../lib/theme`). `TlsIndicator.tsx`
  currently renders no TLS icon in the Sidebar. For `visitor`, the footer also renders documentation
  and API keys, while the main column is replaced by `ChatSessionList` and
  the network link. The history keeps the new-conversation button and the network outside the
  scrollable area, so a long list does not push any action out.
  The `Chat da rede` (network Chat) entry shows the counter of messages received since the
  last local read marker; opening the chat screen updates that marker.
  For an administrator (`authority_level` set) on a network with the lobby on,
  an amber badge beside it (`LobbyBadge`, `network-chat-lobby-unread`) counts
  the lobby messages since the lobby room was last open here
  (`useLobbyUnread`, its own read marker in `lib/networkChatUnread.ts`; the
  room is polled only in that case).
  Since that menu is seen from every route, choosing a conversation or starting a
  new one also **navigates to `/chat`**: without that, a click made from the
  settings or the documentation only swapped the active session and left the screen
  where it was. Whoever is already in the chat is not sent again.
## Slicing, first run and network panels

- `SlicingJobsPanel.tsx` - Embedded slicing activity panel used by
  `SliceModels`. It reads `useJobs` (`GET /api/ui/jobs`) and renders one card
  per job with state, named phase, byte heartbeat, rate/ETA when available and
  output paths. Completed and failed history remains on the slicing screen so
  a failure cannot look like an eternal spinner; partial outputs are shown.
- `FirstRunWizard.tsx` - The wizard of a machine's first run, rendered by
  `App.tsx` in place of the whole shell while the current guide step is one
  of `WIZARD_STEPS` (`useFirstRunWizard`, `admin` profile only): one screen
  with one name (pre-filled with the hostname) given to the machine and to its
  network (with the warning that it is advertised if the network goes
  public), then a "Welcome to POM" animation that reports `bootstrap` by
  itself after `WELCOME_MS`. The field has a "Random" button
  (`lib/randomName.ts`). The name screen saves it through the routes the two
  settings screens use, then reports the one `names` step. It reuses the
  `.pom-login*` / `.pom-setup*` shell. Tests in `FirstRunWizard.test.tsx`.
- `ChangePassword.tsx` - The panel password screen (`/config/password`),
  drawn like the wizard in place of the shell, on every profile: the new
  password twice, plus the current one once the default is gone; live checks
  (`passwordChecks`) mirror the node's rule. Saves with
  `POST /api/ui/access/password` and the tab goes on with the new password.
  Tests in `ChangePassword.test.tsx`.
- `FirstRunGuide.tsx` - The hints of a machine's first run, mounted once in
  the shell (`App.tsx`, `admin` profile only), for the steps after the
  wizard. Shows one balloon for the
  current step of `lib/guide.ts`: beside the menu entry that leads to the
  step (`[data-guide-nav="<path>"]`, else `[data-guide-section="<labelKey>"]`,
  both set by `Sidebar.tsx`), or under a control on the step's own screen
  (`[data-guide=...]`). A step has an ordered list of targets, last click
  first, and the first visible one wins and may bring its own text
  (`hint`), which is how the balloon follows the download and deploy steps.
  The steps without a save end by themselves: `DownloadWatcher` (catalog or
  finished download job of `GUIDE_MODEL_REPO`), `DeployWatcher`
  (`ClusterView.active_model`) and `ChatWatcher` (`lastStreamResult`), each
  mounted only while its step is current. On arriving at the download screen
  it puts the repository in `?q=` once. It
  re-measures the target every 200 ms and on scroll/resize, draws through a
  portal above dialogs, and falls back to a corner card when there is no
  target. `PLAN` maps each step to its path, section and targets; adding a
  step means a new entry there, in `MESSAGES`, in `guide.*` messages and in
  `GUIDE_STEPS` on the node. Styles: `.tour-float`, `.tour-ring` in
  `index.css`.
- `LocationEditor.tsx` - Where a network is, used by Network settings and by
  the setup wizard. Inline it shows the chosen city, the precision select
  (approximate, city centre, exact) and a preview of the point the node will
  publish. Its dialog has the map (`LocationMap.tsx`), a "Use my browser's
  location" button, the cities around the placed point and a search by country
  and name. Three rules: `navigator.geolocation` runs only on that button
  (and needs HTTPS or localhost, which the dialog says when it is missing);
  the city is always a row returned by the node (`lib/geo.ts`), there is no
  field to type one; and the pin is what is sent, the node reduces it to the
  chosen precision. Tests in `LocationEditor.test.tsx`, with the map mocked.
- `LocationMap.tsx` - Leaflet map with OpenStreetMap tiles and one draggable
  pin; a click or a drag reports the point. Loaded with `React.lazy`, so
  Leaflet is its own chunk and the tiles (the only third-party request of the
  admin UI's location flow) are fetched only while the dialog is open. A tile
  error is reported so the dialog can say the map is unavailable.
- `RelayOptions.tsx` - Whom this machine relays for: three cards
  (`RelayChoices`: any network, with the "Recommended" badge, this network,
  file transfers of this network) read from `GET /api/ui/network/relay`
  (`useRelayStatus`). `RelayOptions` saves each change at once with `POST`,
  which the node applies live, inside a panel or, with `section`, inside the
  caller's own section (Network settings); the quick setup uses
  `RelayChoices` as a draft and saves it at the end. Nothing is drawn while
  the report says `reachable: false` (`canRelay`): the NAT report has not confirmed
  a public address and none is configured, so behind a NAT the machine
  cannot relay for anyone and there is nothing to choose. An option the command line fixed for this run is
  shown locked, with the reason; file transfers are locked while the machine
  does not relay for its network. `relayStateKey` turns the answer into one
  line (relaying for any network, for this network, not relaying, or not
  reachable from the internet). The panel stays out when the node does not
  answer. On a visitor a fourth card, "Relay anonymously for this network"
  (`anonymous`, `anonymousOffered`), lets anyone with an admin's pass relay
  through it without it learning who; every save posts all four options, so
  one card never resets another. An admin sees how many visitors relay that
  way (`anonymous_relays`, `relay-visitors`), even behind a NAT. Tests in
  `RelayOptions.test.tsx`.
- `NatBadge.tsx` - the top bar's amber "Behind NAT" badge, drawn only while
  `ClusterView.relayed_peers` is not empty: a peer reached through a relay that
  carries the traffic because the hole punch it coordinated did not close.
  Behind a NAT whose hole punch worked it draws nothing. It took the place of
  the node's short id. A click opens a `Modal` naming those machines and their
  relay kind, explaining the relay detour (public relays cap it at about
  2 MB/s), why it makes the pipeline slow and how to give the machine a direct
  path: IPv6, router port forwarding (this node's P2P and endpoint UDP ports,
  `p2p_port` and `p2p_port + 10000`) or UPnP, and the firewall. Tests in
  `Topbar.test.tsx`.
- `HttpsCertificatePanel.tsx` - "Node certificate" on Preferences, every
  profile: whether this computer trusts the node's local CA
  (`GET /api/ui/tls/ca`), its fingerprint, a refusal remembered from the
  desktop app's prompt, the `.crt` download (`/api/ui/tls/ca.crt`, fetched
  with the session and saved as a blob), "Install on this computer"
  (`POST /api/ui/tls/ca/install`, only when `can_install`, i.e. the request
  came from the node's own machine) and the manual steps per system and for
  harnesses. Tests in `HttpsCertificatePanel.test.tsx`.
- `ClusterVersion.tsx` - Panel with the trust chain version this node holds
  (`NodeInfo.cluster_version`, formatted by `lib/clusterVersion.ts`), an
  outdated badge with the hosts announcing a newer one, and "Sync now"
  (`POST /api/ui/network/trust-sync`). When outdated it asks for one sync by
  itself per version. Mounted on Network settings and Administrators.
- `AdminAuthorityPanel.tsx` - Readable authority roster for the principal,
  secondary admins and promotion chain. It exposes promotion, demotion and
  succession controls only when the presentation contract says this viewer is
  the principal; refusal states remain explicit. Inference nodes are left out
  of the promotion candidates (the node refuses them as well).
- `Topbar.tsx` - Sticky header: `NodeChip` (the network this machine is in,
  from `NodeInfo.network_name` and linking to `/config/network`; then
  `ConnectionBadge` on a member; then hostname,
  the `NatBadge`, peer count, coverage; a machine alone in a network named
  after it shows only the network badge; below `md`, a phone or the macOS
  menu-bar panel, those badges fold into one Info icon, `NodeChipButton`,
  that opens them in a popover like the bell's), the i18n badge of the node's
  persisted profile (not drawn for a visitor, and inside that popover below `md`), the macOS button of
  the larger window, `UsageButton` (a gauge icon drawn only when
  `GET /api/ui/quota` says `configured: true`, i.e. this node has a budget set
  for it, with an amber/rose dot near or over a ceiling; a click opens a
  popover like the bell's with each plan's spend and when it turns over, the
  reset credits and a link to `/config/usage` to reset or ask for more; the
  drawing is `QuotaParts.tsx`, shared with that screen) and the bell (`Notifications`). The `Este nó` ("This node") button is not
  rendered.
  Mobile drawer renders `SidebarContent` inside an animated
  panel whose fixed backdrop unmounts synchronously when navigation closes it,
  so an invisible exit layer never blocks the next hamburger click. The action of
  leaving the network is not exposed in the menu.
## Chat states and notifications

- `ChatWaitingLine.tsx` - the Chat's line between sending and the first
  token: pulsing dots and a message that rotates (`lib/chatWaiting.ts`), with
  the "starting our engines" set while the deployment wakes from idle. It is
  not drawn while the model is thinking, which has its own line.
- `ConnectionStatus.tsx` - `ConnectionBadge`, on every member (worker or
  visitor), never on an admin or a node off the network (`standalone`): an
  amber "Connecting…" with a spinner while `useConnection` says `discovering`
  or `authenticating`; once connected, how many machines of the network it
  sees (lime, amber when no admin is among them) or an amber "Network out of
  sight" when it sees none. Shown in the `Topbar` and on this machine's row of
  the Networks screen. Tests in `ConnectionStatus.test.tsx`.
- `MissingPiece.tsx` - the puzzle screen (`compact` shrinks mark, puzzle and
  title for a card inside a screen, as on the Networks discovery-off card), `variant="paused"` for a stopped
  on-demand plugin (fourth piece grey, dashed, with a pause sign), `variant="loading"`
  for a plugin screen still opening (fourth piece hovering over its slot, and the
  whole assembly faster because the host opens the screen within a couple of
  seconds), `variant="failed"` (fourth piece in its slot, rose and still) and
  `variant="done"` (fourth piece dropped into its slot) for `PluginLoader`, and, by default, the Chat's screen while the network serves no model:
  an SVG 2x2 puzzle whose three pieces (`piecePath`, knobs that agree on
  shared edges) fly in and lock, leaving the bottom-right slot empty and
  pulsing, under the POM mark (art per theme), over a title, a
  detail and the caller's actions (the Chat's next steps and links).
- `ChatHero.tsx` - the Chat's opening screen over an empty conversation: the
  site's POM mark for the active theme (`assets/pom-mark-dark.png` /
  `pom-mark-light.png`), which rises from below after a pause and then
  breathes, followed by the line that opens the initials "P O M"
  into each of `POM_PHRASES` and closes back, and the `.pom-login-link`
  nodes; styles `.pom-chat-hero*` in `index.css`. Under reduced motion it
  holds PIECE OF MIND open.
- `Notifications.tsx` - The activity bell: carries the default-password
  notice (once per start of the node, `lib/defaultPasswordNotice.ts`; it
  replaced the fixed `DefaultPasswordNotice` banner), counts running slicing jobs
  (`useJobs`), transfers worth notifying (`useTransfers` +
  `isWorthNotifying`) and pending offers (`useOffers`), and shows progress bars
  inside the popover. Jobs, transfers and offers are not polled on a
  `visitor`, whose node refuses `/api/ui/jobs` and every `/api/transfer` route. Offers can be accepted/rejected right there
  (`POST /api/transfer/offers/:id/accept|reject`). It also lists admission
  (`lib/admissions.ts`): for the network's admin, each machine asking to join
  with its role, direct or relayed connection, latency and hardware, and
  "Open the conversation", which goes to `/network-chat?lobby=<node_id>`:
  the decision is taken in that thread, not in the bell; for a candidate, the answer to its own requests, with
  "Join now" when approved. All state is read back from
  the node, so a reload in the middle of a transfer shows the same bar.
  It also renders principal-transfer offers from the same `useOffers()`
  response, and the pending plugin confirmations
  (`usePendingPluginConfirms`, `lib/pluginInbox.ts`) with Aceitar and
  Recusar (Accept and Refuse); answering resolves the plugin's `notification.confirm`. Plugin
  notices (`usePluginNotices`) are message-only rows that count as unread
  until the bell opens, then get marked read and can be removed. Node-to-infer changes are amber notices without
  action buttons. Authority offers keep their pending state in the bell and
  link to `/config/administration` for the roster.
  Finished jobs (a deploy, a slicing run; not a model download, whose panel
  has its own outcome) stay with their outcome for `RECENTLY_FINISHED_MS`
  (`lib/finishedJobs.ts`): unread until the bell opens, removable with an X,
  both remembered in `localStorage`. Before, a deploy that ended between two
  polls left the badge at (1) and the tray empty.
  `DeployProgressModal.tsx` - the window the Deploy screen opens on the job it
  started (or on the running one the node names in a 409 `deploy_in_progress`):
  its steps and progress, then ready or failed with the reason; Close leaves the
  deploy running, Switch to dashboard goes to `/`.
  It lists **both directions** - an upload icon for `sending`, a download icon for
  `receiving` - and also the **recent outcome**: completed, failed and refused
  stay in the bell for `RECENTLY_FINISHED_MS`. Before, it only showed what was
  underway, and the result vanished the instant it happened: whoever was receiving
  saw the bar move and then nothing, whoever was sending saw no notice of acceptance nor of
  refusal. A finished transfer draws no bar (a bar stopped at
  100% suggests it still moves), and the node's `detail` is shown
  as it arrived. A slice's title is **model + layer range**
  (`transferLabel`), with the short sha256 as a secondary identification beside it:
  the notification showed the hash, which says neither which model it is nor which piece it
  carries. In an offer, what is offered is the title and who offers it is the subtitle - the other way
  around, the layer range was exactly what the ellipsis ate.
## Cluster visualizations

- `PipelineRail.tsx` - the cluster as the path a request takes: `PipelineRail`
  draws peer cards in execution order connected by `Conduit` wires labelled
  with the layer boundary the hidden state crosses; `LayerRibbon` is one tick
  per layer colored by owning peer, exposing coverage holes. `pipelinePeers`
  lived here until 5423852 and now comes from `../lib/networkPeers` (the rail
  imports it); only `trackOf` (the repeating color palette, `TRACKS`) is still
  exported, reused by `PeerDetailCard`, `Dashboard` and `DeployModel`.
  Currently rendered hidden on Dashboard.
- `ModelNetworkCanvas.tsx` - each machine card also shows the measured TPS of
  decode and of prefill (`machine-tps`, from the `peer_perf` of
  `/api/ui/chat/telemetry`) as badges (`speed-decode`/`speed-prefill`)
  colored by `rateSpeed` (`../lib/peerSpeed`): the ratio to the fastest slice of the same model
  (green >= 75%, amber >= 40%, red below), neutral
  when only one slice was measured; without a measurement it says "velocidade não medida" ("speed not
  measured").
  Each card can be minimized (`machine-compact-toggle`; name, state,
  range, badges and one VRAM line in `COMPACT_CARD_SIZE`), and the bar's
  `cards-compact-all` button minimizes or expands all of them and clears the
  exceptions; the state lives in `readCardModes`/`writeCardModes`. Edges,
  `autoLayout` and the drag limits use each card's real size, and
  "Organizar" ("Organize") records only what was dragged, so the arrangement recomposes when
  the cards change size. The `PipelineGatewayNode` is a small, draggable card (its position in
  `GATEWAY_LAYOUT_ID`, in the same map as the machines;
  without a drag it follows the last stage): it shows the cost per token, the tok/s and the relay
  warning; the `pipeline-gateway-open` button (or the double click) opens the window with the
  summary, every route (`pipeline-route`) leg by leg, the return mode and the legs by relay. On the
  deployment map it stays
  fixed. Each route's total is the backend's `total_ms`
  (`pipelineRouteTotalMs`): the network in the return mode the planner would choose plus the
  compute of one token, with the equivalent tok/s (`pipeline-throughput`) and the
  network/mode/compute split of the best route (`pipeline-route-breakdown`). The
  interface never sums the legs: that would count whole round trips of each
  edge. The edges' RTT labels come from the planned routes, identical on
  any node. An edge with `relayed` (a tunneled path through a relay) turns strong
  orange (token `--relay`, `stroke-relay` dashed), and the
  `RouteRttLabel` swaps the transport for "RELAY" with an information icon and the
  explanation in the `title`; if the best route has `relayed` or
  `relayBypassedByRing`, the gateway shows a compact line
  (`pipeline-relay-warning`, full orange when the outbound goes through a relay,
  outline when the ring avoids the return relay) and the icon opens the
  `pipeline-relay-popup` card (it closes with Esc, an outside click or the X) with the legs by
  relay, whether POM bypasses it through the ring and how to improve it.
- `ModelNetworkCanvas.tsx` - a machine whose peer announces a role gets a
  badge: "Prefill first" (`prefill`) or "Prefill/Decode" (`shared`, the
  other machines of a whole model on several); the prefill machine heads the
  layout, the backend's candidate edge prefill -> the others is drawn, and
  the gateway sits after the other machines.
- `ModelNetworkCanvas.tsx` - the main operational view: an SVG graph with
  pan/zoom of nodes (range cards with grouped RAM/load/VRAM meters,
  followed by the compact temperatures when there is a GPU) and curved edges, guided by
  `graphFromCluster` (`../lib/networkGraph`). Edges have three states via
  `edgeStyle` (principal / paralela / inativa - principal / parallel / inactive) from `cluster.routing_edges` and
  `cluster.candidate_edges` (replicas = ready alternate routes). Positions are
  persisted per model-deployment key through `../lib/networkLayout`
  (`readPositions`/`writePositions`/`clearPositions`/`fallbackPosition`/
  `deploymentLayoutKey`), drag-restrained to the canvas. Double-clicking a card
  opens the machine's details in a window: `NetworkNode` guards the gesture
  with `isControlTarget` (a click that lands on a `button`/`a`/`input`/etc is
  not a click on the card), and `detailNodeId` stores the `node_id`, not a peer
  copy, so the window shows fresh numbers when the cluster list refreshes
  under it. Exports `edgeStyle` and `isControlTarget`. That gesture is gated by
  `canSeePeerDetails` (`../lib/access`): outside `admin` the card takes no
  `onOpen`, so it neither opens the window nor advertises a double-click in its
  `title` - the same rule that removes the Details tab and its view switch from
  the Dashboard. The graph also renders directional RTT badges using the same
  label component as the deploy map. The cluster coverage badge is an admin
  summary; node and infer profiles use their own model and slice data instead.
  A missing peer load stays visibly unknown in the card meter and is never
  rendered as zero. The selected pipeline is drawn only forward; a virtual
  `Gateway` is placed after the last layer stage (until dragged) and receives
  the return-leg arrow. Its window lists every complete deployed route,
  including the return leg, ordered by time per token and then node ID. The
  card uses the least-cost route for the headline; an absent leg remains
  unknown and is never treated as zero. An observed execution cost still
  includes peer compute when that measurement exists.
  When the Dashboard has an active deployment, its properties toggle is a
  compact icon button inside the map's upper-right control area, next to the
  zoom and Fit controls; the dashboard toolbar remains reserved for cluster
  status and actions.
- `ModelNetworkCanvas.tsx` / `PeerDetailCard.tsx` - a machine whose llama.cpp left layers on the
  CPU while it has real GPUs (`warnsLayersOnCpu` in `../lib/layerPlacement.ts`: `engine` null,
  `gpu_kind` nvidia/amd/mixed, `reported_gpu_layers` below the slice) gets an amber `AlertTooltip`
  beside its layer range (`machine-cpu-warning`, `peer-cpu-warning`) saying how many layers spilled
  and what to do. Never on `unified` (a Mac) or `none`, nor under a plugin engine, which reports no placement.
- `RouteRttLabel.tsx` - the shared directional RTT visual badge used by the
  Dashboard map and by the deployment map. On the Dashboard it also identifies the
  effective transport (`HTTP`, `HTTPS` or `QUIC`) observed in the execution; when
  the measurement does not exist yet, it keeps `RTT -` instead of using the tunnel probe.
- `PeerDetailCard.tsx` - The card of one machine, in the same drawing for every
  machine. Each GPU is one tile (`gpu-telemetry-row`: name, VRAM, usage,
  temperature, power). `variant="dialog"` is the Flow window: no frame, larger
  header with the speed badges (`SpeedBadge.tsx`, shared with the canvas),
  named Resources/Deployment sections, `Modal size="xl"` with `titleHidden`. It lives outside `Dashboard` because it shows in two places: the
  details tab stacks one card per machine (`networkPeers` splits the whole
  network into the active deployment first, in layer order, then the rest), and
  the double-click on a Flow canvas card opens the same card in a
  `PeerDetailDialog` window over the `Modal` shell (`size="lg"`). One component
  is what keeps the two surfaces from diverging. `cardRange` returns `null`
  when the machine serves nothing - `hasSlice` reads `model` empty with zero
  `total_layers` as absence, never the range "0-0" - and the card then shows a
  "sem fatia" ("no slice") badge plus `dashboard.noSliceDetail` instead of a range, and does
  not offer "Remover" ("Remove") (`RemoveNode` renders only for `serving && group &&
  range`). `cardRange` falls back to `node.config` when the peer has loaded a
  slice it has not announced yet. `LoadParamsSection` renders the llama.cpp
  context parameters from `health.load_params` (Flash Attention,
  `n_batch`/`n_ubatch`, context, KV sequences, threads, GPU layers, KV cache
  type); it is skipped entirely when the peer's `/health` carries no such
  field, and `flash_attn: null` reads as "I do not know", never as off.
  An unreadable peer has **two** states, and the card separates them: `unreachable` is the peer
  that answered on the announced address and whose read broke (a dead end, a pink
  dot), `rediscovering` is the peer whose addresses all died and whose current address the node has
  already gone to ask for (work in progress, a pulsing amber dot,
  `dashboard.rediscovering`). Temporary IPv6 addresses run on their own, so the
  second state is the common one and it resolves on the next gossip round - showing it
  as a dead end sent the user looking for a defect that did not exist. See
  `docs/subsystems/p2p-network.md`, "The address that stopped answering".
  `PipelineRail` and `ModelNetworkCanvas` make the same distinction.
  `RemoveNode` and `ReleaseSlice` render from here - both in one
  **toolbar at the top of the card**, beside the MTP badge,
  instead of scattered through the footer. `RemoveNode` releases this machine's slice of the model
  and keeps it in the cluster. `LeaveNode` is no longer rendered on the card: leaving
  the whole network is done from the machine's own My Machine screen. The update-and-restart action, development-only, is not
  rendered. The card is dense on
  purpose (`compact` in `Meter`/`KeyValue`, `px-3 py-2.5` sections) so it fits
  on one screen; what is useful without needing to be always visible lives in an `InfoTip`
  (the reason for "no slice", the `gpu_note`, the fit note, the model path, the pid
  and the restarts), and "llama.cpp load parameters" and "Processes on the GPU" are closed
  `<details>`. The metrics' sequence follows the same pattern as the compact card: GPU telemetry
  (usage, temperature and optional watts in one row per GPU), RAM, load and disks. The disks are
  **one meter per mounted partition**
  (`system.disks`), with a fallback to `system.disk` on a node that does not publish
  the list yet. Another member's sample arrives trimmed by its node (this machine's own card is untrimmed and shows
  `SharedSummaryNote`, a lock note under the disks and in the GPU processes, saying only the summary is shared): one volume with an empty
  `mount_point` (labelled `dashboard.modelStorage`) and a `pid` 0 GPU process with no name, the
  total of every non-POM process (labelled `dashboard.otherGpuProcesses`). `PeerPlacement` (internal) is the only part of the card that
  makes an extra call: it mounts only with the machine serving, and the `enabled` of
  `usePeerRuntime` is what prevents one `/health` per card on every Dashboard cycle.
  When the configuration asked for MTP, the same card also shows
  `mtp_ready`/`mtp_allocation`/`mtp_unavailable_reason` from the runtime, including the
  context's measured location and the reason of the attachment failure. The thermal section shows one row per GPU and declares an unknown
  temperature when the driver provides no reading; the power stays
  unknown when the platform does not publish watts.
- `PluginServices.tsx` - what a machine's plugins serve to the network
  (`node.plugin_services`, e.g. Qdrant as the RAG server). `PluginServiceBadge`
  is the badge, toned by status (`compact`: icon only, name and kind slide out on
  hover or focus); `PluginServiceTether` hangs compact badges under
  the machine's card in `ModelNetworkCanvas` with a connecting line
  (`SERVICE_TETHER_HEIGHT` is the room the canvas arrangement leaves for it), and
  `PeerDetailCard` lists them in a Services section. `servicesInUse` picks what
  the Flow draws: with a model loaded, only the inference service that is the
  engine in use, plus a `llama.cpp` pseudo badge when the engine is llama.cpp
  (`PeerView.engine` null). Tests in
  `PluginServices.test.tsx`.
- `KvCache.tsx` - the KV cache block of `PeerDetailCard`: disk used by the
  machine's snapshot directory (`node.kv_cache`), the host-memory cache from
  `/health` (`cache_ram`) and "Clear cache" with a confirmation, sent to the
  local node with the target `node_id` (`clearKvCache`). Tests in
  `KvCache.test.tsx`.
- `ModelPlacement.tsx` - the "How the model was loaded" section: the layer
  strip (one cell per block, green on the GPU, amber split, grey on the CPU),
  the weight totals per side, where the KV cache stayed and the output head.
  It exports `placementOf`, `isCpuBuffer` and `kvCacheBuffers`. The rule that matters
  lives in `placementOf`: a layer is classified by its **bytes** in each
  buffer, never by the block's `device` - with a buffer-type override (a MoE's experts
  on the CPU) the two diverge, and that is exactly the case the screen exists to
  show. Without `placement` the section says it does not know, instead of
  deriving it from `n_gpu_layers`.
- `AddNodeDialog.tsx` - the "Add a machine to a live model" dialog:
  `POST /api/ui/deploy/node` applies to the target only (no redeploy of the
  cluster). `POST /api/ui/deploy/node/options` is re-read for the selected
  target, so its own slices and whole GGUF are offered instead of a fixed
  cluster plan. The machine's whole configuration is edited with the same
  `DeploymentInspector` as the deploy screen, starting from the active version of
  the deployment (`listDeployments`); it is sent as the machine's own `overrides.params`
  and the version keeps its parameters. The K/V cache choices come from the target's
  options (`cache_types`) and are sent as the install values. The machine's engine is
  chosen among the inference services it runs (`services`, from `inferenceServicesOf`):
  llama.cpp, or a plugin's service, which goes as `inference_service` in the same params.
  A service that does not run layer slices offers the whole model only. The inspector
  shows that service's engine settings and its POM settings for this machine, which land in
  `engine_settings` (the values the plugin receives). The node validates the engine with
  `deploy_block` and refuses a slice on a whole-model service (`deploy/node.rs`).
  Overlap is the point - it creates a parallel route - so it is flagged, not
  blocked. The dialog can add a whole-model head to a sliced deployment and
  vice versa. Exports `rangeOverlap` (shared layers between a chosen range and
  existing ones).
  `Dashboard` mounts this dialog and its trigger only for the `admin` profile;
  the server repeats that authorization and returns a clear localized refusal
  for other profiles.
  Re-seeding the two selects happens on the *open* transition only (`wasOpen`
  ref): `Dashboard` rebuilds `peers` on every cluster poll, so an effect keyed
  on those props threw the operator's choice away mid-edit. A second effect
  only replaces a choice that disappeared from the list. The catalog request
  is aborted when the machine changes, and the option select is keyed by kind
  plus `start-end` (`keyOf`), not by array index, so a reordered list cannot
  silently point at another range.
## Joining, guards and destructive actions

- `LeaveNetworkButton.tsx` - The visitor's "Leave network", an icon beside
  the theme switch at the foot of the Sidebar. Same confirmation and act as
  the Networks screen (`POST /api/ui/network/leave`): the node goes back to
  its own network and the page reloads.

- `LobbyChat.tsx` - Chat with a network this machine has **not** joined
  (`docs/subsystems/p2p-network.md`, 6.1). A modal that polls the node every
  3 s (`GET <endpoint>?key=<publisher_key>`) and posts through it; the browser
  never talks to the other network. `endpoint` is
  `/api/ui/network-chat/lobby` on a configured node and `/api/setup/lobby` in
  the wizard, where requests go without a session. Shows each message as
  member or candidate, the principal's refusal codes as sentences, and the
  hardware card the network sees. On a configured node its side panel also
  requests admission (`POST <endpoint>/admission` with the role, limited to
  the network's `accepted_enrollments`), shows the request's state and, once
  approved, joins behind a confirmation (`admissionsApi.join`). A `standing`
  approval - the machine was admitted to that network before - is not an
  answer to anything: the panel says it already holds the credentials and its
  button switches at once, and the bell does not list it. A declined request
  shows how many minutes are left before asking again (`retryWaitMinutes`
  over `retry_at`), and the request form comes back when the wait is over.
  Once the request is approved or declined the conversation is hidden and
  only the request side stays: an approval erased the thread on the admins'
  side. Asking again brings it back.
  `layout="page"` fills the hosting screen instead of a window; a back
  button is drawn only when `backLabel` is given (the Networks screen gives
  none: its tabs stay on top and are the way back); `headerActions` adds
  controls at the right of that header. `focusAdmission` puts the focus on the role picker once the
  request form shows. A lobby whose network answers `lobby_disabled`, page or
  window, shows only that notice (`lobby-disabled`): the network refuses the
  admission request as well; the optional
  `onUnavailable` still reports it, though the Networks screen no longer uses
  it. When the network answers `messages_closed` (its Chat is off) the
  conversation column is not rendered: the window, or the full-screen page
  centered, is only the hardware card and the request panel (`lobby-request`).
  A pending request keeps a "Send the request again" button (same role): the
  principal only refreshes a pending record, so the candidate is never stuck
  waiting on an admin who missed it.
  Test: `LobbyChat.test.tsx`.
- `NetworkQuickSetup.tsx` - step-by-step window that publishes this
  machine's own network from the Networks screen: discovery (public, hidden,
  off; public suggested), name/organization/description/rules/channels, who
  can join and the accepted roles, the location (`LocationEditor`, public
  only) and a review. `quickSetupSteps` drops the steps a visibility does not
  need. It saves the whole `NetworkSettings` through
  `POST /api/ui/network/settings`, the same route as Network settings, and
  the node stays the authority. A relay step (this machine's
  `RelayChoices`, suggesting relaying for any network unless the command line
  fixed it) comes before the review only when the internet reaches this
  machine (`canRelay`); the
  relay is posted after the network and only if it changed, and a failure
  there is reported apart (`onSaved(saved, relayFailed)`). Its steps are exported (`SetupSection`,
  `DiscoveryChoices`, `InfoFields`, `AccessFields`) and Network settings
  draws its edit form with them, so both ask the same questions the same way.
  Also exports `ENROLLMENT_ICONS` (icon of each enrollment role: `Server` for a network node, `MessagesSquare` for an inference node) and
  `VISIBILITY_ICONS`/`VISIBILITY_LABELS` (globe, eye-off, wifi-off), which the
  Networks screen draws before each network's name. Tested through
  `../routes/PublicNetworks.test.tsx`.
- `LobbyHardwareCard.tsx` - The hardware a lobby candidate declares: hostname,
  OS, CPU threads, RAM and GPUs with VRAM. Same facts the Dashboard card opens
  with, without live usage, because nobody measures a non-member. Used by
  `LobbyChat` (own card) and by the lobby tab of `routes/NetworkChat.tsx`.
- `Confirm.tsx` - Two exports. `Modal` is the shell (veil + panel + header +
  footer) rendered via `createPortal` straight into `document.body`. The portal
  is not preference: `position: fixed` stops being relative to the viewport when
  any ancestor carries `transform`, `filter` **or** `backdrop-filter` - that
  ancestor becomes the containing block. The top bar and the Dashboard header
  have `backdrop-blur`, so dialogs opened from there were centered inside a
  ~76 px strip, title off-screen (measured at `y = -66` on a 2029 px viewport).
  `Confirm` is the question wrapper on top of `Modal` (Escape cancels, `busy`
  disables both buttons, `tone` selects primary/danger); the split exists
  because not everything in a modal is a question - the undeploy report window
  reuses the shell and has no "cancelar" ("cancel"). A window can open another window (the
  "Sair da rede" ("Leave the network") confirmation is born inside the details window), and all of
  them listen for Escape on the `document`, so `Modal` keeps a module-level
  stack of the open ones (`openModals`, keyed by `useId`) and only the top one
  answers Escape; without it one Escape closed both. `size` picks `md` (a
  paragraph), `lg` (a whole panel, for `PeerDetailDialog`), `xl` or `full`
  (nearly the whole window, the Networks screen's full details); the panel is a
  flex column capped at `max-h-[calc(100vh-2rem)]` whose body scrolls
  (`min-h-0 flex-1 overflow-y-auto`) instead of the old `max-h-[70vh]`.
- `TrustTransferGuide.tsx` - the dialog behind the Networks screen's "Share"
  and "Add network": the same two-machine diagram (`TrustTransferDiagram`,
  the trust file travelling, `.trust-travel` in `index.css`, still under
  reduced motion) with "You are here" on the side the operator stands, three
  steps and the action (download the file, or choose it). The file is public
  but must arrive unaltered; the text asks for a trusted channel.
- `LeaveNode.tsx` - Take **one** machine off the network. Not rendered by any
  screen today (the machine card dropped it); kept with its test for a future caller. Same act as the top bar's "Este nó › Sair da rede" ("This node - Leave the network"), but that one
  only ever detached the machine the panel is open on; reaching each machine's
  own interface to switch it off was a detour, so the request goes to the local
  node with the target's `node_id` and it forwards (`POST /api/ui/leave`,
  `webui::forward_leave`). `isSelf` sends `{}` (local, no relay). Explicit
  confirmation because re-entry happens from that machine's own `Rede` tab ("Network"), not
  from here.
- `UndeployPipeline.tsx` - The inverse of "Implantar na rede" ("Deploy on the network"): take the whole
  pipeline out of service at once via `POST /api/ui/undeploy` (`{group}`), the
  counterpoint to the per-machine `ReleaseSlice`. "Descarregar modelo" ("Unload model") only ever
  freed one box, and undoing a four-machine deploy was four visits to four
  panels; a machine left behind keeps RAM/VRAM busy and serves nothing, since
  the pipeline only plans with closed coverage. Two promises to the operator:
  ask before acting (naming each machine and its layer range) and report
  machine by machine - a peer that did not confirm still holds its memory, and
  the report shows `released: false` with the reason instead of a rounded-up
  "pronto!". Nothing is deleted from disk (`undeploy_endpoint` in `deploy/node.rs`).
  `Dashboard` renders it only for the `admin` profile, while the backend is the
  final authority and its refusal text is shown as it arrives.
  Renders both a `Confirm` and a result `Modal` (no cancel). `inProgress`
  (with `label` and no targets) is the deploy still starting: the Dashboard
  draws it from a running `deploy` job's `deploy_id`, and the node stops that
  deploy before releasing anything (`cancelled` in the report). `RemoveNode` stays
  separate and keeps the per-machine button available to every profile.
- `ErrorBoundary.tsx` - Class boundary placed around the routed page in
  `App.tsx` (the chrome survives a broken screen). Clears on `resetKey` change
  (the route path); `fallback` lets the chrome collapse to a line; retry button,
  a reload-the-page button (the only cure for a lazy screen whose module failed
  to load: `lazy` rethrows the cached rejection) and the error message in a
  `<pre>`.
## Brand and machine marks

- `Logo.tsx` - `LogoFull` draws the lockup from `brand.logo[theme]`: two arts,
  one per theme, which is why it needs **no plate behind it** - each theme gets
  art drawn for its own ground. The dark one is derived from the light one by
  `scripts/derive-dark-logo.py` (luminance of the flat teal wordmark mirrored,
  brain illustration untouched) until hand-drawn art exists; replacing
  `assets/brand/pom-logo-dark.png` is the whole change. `variant` is only about
  filling: `"card"` pads (setup screen), `"band"` fills edge to edge (the
  sidebar header, which is the band). `LogoMark` (the real artwork, `<img>` from `/brand/`, served as a static asset because it is a detailed raster shown small) + `Logo` with wordmark. Name, tagline and image URL all come from `lib/brand` (`brand.json` at the repository root), never from literals here; callers size it through `size` only, so sidebar, setup screen and tab icon follow one source. The mark carries its own glow on transparency, so it gets no plate behind it.
- `PlatformIcon.tsx` - Windows, Linux and macOS marks drawn inline in
  `currentColor` (lucide has no brand icons), each with its name as the
  accessible label and title. The Plugins cards use them for the systems a
  plugin publishes builds for.
- `MachineIcon.tsx` - `machineKind(system)` classifies a machine from its
  `SystemLoad` (nvidia GPU box, Apple Silicon Mac, Windows, Linux) into a
  label/icon/accent color; `MachineIcon` renders it. Reused by Dashboard,
  NetworkSettings and PipelineRail to make a heterogeneous cluster readable.
## Slice and node actions

- `ReleaseSlice.tsx` - "Give this machine's memory back": probes
  `GET /api/ui/release/candidate` to say truthfully whether another machine
  picks the slice up (or the layers go down), then `POST /api/ui/release`. Only
  rendered on the machine's own card - the node owns its memory.
- `RemoveNode.tsx` - Take a machine out of a live model without touching the
  others: `removeNode` (`../lib/api`, `POST /api/ui/deploy/node/remove`), with a
  confirmation that distinguishes "loses a parallel route" from "these layers
  go uncovered". Exports `layersLeftUncovered` and `rangeText` (compact range
  rendering), computed the same way the node does.

Key logic carriers:

## Key symbols of the shared primitives

- `ui.tsx:Button` (ui.tsx:100) - the single button primitive: variant/size/
  loading/icon, `active:scale` press feedback, disabled while loading. Every
  other component here builds on it.
- `ui.tsx:ErrorState` (ui.tsx:369) - turns any error into a panel with a
  localized title; the message itself is the node's English text.
- `Markdown` (Markdown.tsx:32) - `memo`ized so unchanged message bubbles do
  not re-parse their transcript during a stream; the active answer is published
  at a 50 ms cadence by `ChatSessionProvider`. Fenced-code detection decides
  inline vs block + copy button.
- `nav.ts:NAV` (nav.ts:40) - the source of truth for the menu, its order and
  its routes; the section that is "its own entry" (`/deploy`, deploy) is a
  deliberate first-class navigation item, not a leaf under the shards section.
- `networkPeers.pipelinePeers` (networkPeers.ts:13) - execution order: peers
  on the active model sorted by start layer (falls back to all peers);
  `Dashboard`, `PipelineRail` and `ModelNetworkCanvas` derive "the pipeline"
  from it. Moved here from `PipelineRail` in 5423852.
- `PipelineRail.trackOf` (PipelineRail.tsx:38) - the per-index color cycle used
  to keep a peer's color consistent across rail, ribbon, and deploy rows.
- `ModelNetworkCanvas.edgeStyle` (ModelNetworkCanvas.tsx:25) - encodes the
  three edge states (principal = aqua arrow, paralela = solid iris, inativa =
  dashed faint) that make replicas legible.
- `ModelNetworkCanvas.isControlTarget` (ModelNetworkCanvas.tsx:69) - the
  double-click guard: a gesture that landed on a control is not a gesture on
  the card, so opening details never becomes a trap for a future button.
- `ModelNetworkCanvas.beginDrag`/`drag` - pointer capture drag shared by
  machine cards and the gateway, keyed by layout id and clamped by card size;
  a pointer that starts on a control is left to the control. Positions are
  persisted on every move (`writePositions`).
## Deploy-time views and inspectors

- `DeployNetworkCanvas.tsx` - in a prefill/decode split (a whole model on
  several machines, each row with its `inference_role`), the prefill
  machines are a stage of their own before the decode ones (`stagesOf`,
  `roleOf`), so the principal edge goes Prefill -> Decode and the gateway sits
  after the others. `DeployModel` lets a service that declares
  `inference_role` take a second machine for a whole model, with the default
  role (the node turns it into `shared`); the role of each machine is edited in
  the POM settings card (`DeploymentInspector` `machineRoles`, one row per
  machine), never in the machine card. Back to one machine, `DeployModel`'s
  `rows` drops the row's role, so the deployment's role the inspector shows
  is the one deployed.
- `DeployNetworkCanvas.tsx` - the compact, deploy-time topology view. It
  reuses the visual contract of the `ModelNetworkCanvas`: the dotted background, machine
  cards, `autoLayout`, positions persisted per fingerprint, pointer drag,
  zoom, Fit, Organizar (Organize) and Redefinir (Reset). Cards already on the map do not
  repeat the bottom strip; dragging or double-clicking an available peer
  adds it to the preferred set and makes the card appear on the map. The plan is still the source of
  truth: stages are identified by the range, not by the node, and `route_options`
  draws only bridges between consecutive stages, with
  principal/parallel/inactive paths and a directional RTT; the geometry anchors the origin and the
  receiver on the sides that face each other, including when the receiver is on the left;
  the canvas also filters
  old answers that try to skip a stage. The bottom strip keeps
  accepting dragged peers to recompute the suggestion. A card's `X` removes every range of that peer from the selection
  and returns it to the bottom strip. If the backend does not publish an edge, the canvas
  leaves that edge's RTT absent instead of copying the measurement of the destination node; the
  card can still show the local RTT announced by the machine itself. Exact replicas may appear stacked in
  the same column. In whole-model mode, the first machine takes the whole file and the others enter without a range
  (an inference service keeps the map to one machine); in Tensor mode, the map limits the available ranks. The in-flight load
  of the card comes from the machine's own live report; when that report
  does not arrive, the meter shows unavailable and does not invent zero.
  `DeployModel` computes `mapNodeIds` once and passes that same population
  to the cards and to the primary/workers selectors; pre-sliced TP accepts
  a mapped worker without the local rank, while whole-model TP demands the local
  GGUF. The Tensor card exposes the accessible action of marking a primary, removing the
  same node from the workers.
  An absent GPU shows a static track with the text `sem GPU` ("no GPU"), with no
  indeterminate bar. When the plan carries `machine.capacity` (a post-deploy projection
  coming from `/api/ui/deploy/plan`), the card swaps the single GPU bar for one
  `GpuProjectionBar` per device: the fill = the projected use
  (`vram_projected_mb / vram_total_mb`), a tick = the use right now, pink when
  the projection goes past the board, with an `AlertTooltip` listing the numbers when
  `!fits`. Without `capacity` and with an assigned range, the bar says "projection after the
  copy". The hydration of positions keeps the existing cards when a new
  peer enters without a range, so the topology is not reorganized because of
  an inclusion not configured yet. On a narrow viewport, the map uses
  internal scrolling to preserve the cards' size and align the curve to the
  endpoints even when the content is wider than the panel. The separation exists because this graph
  represents a deployment that has not been applied yet.
- `SidecarPicker.tsx` - the vision projector / MTP draft picker shared by the
  three screens: a select naming each compatible file with its size and
  machines, and below it one line (size and machine count) with "More info",
  which opens a window with the full name and the path on every machine - the
  per-machine list used to fill the inspector. On the deploy screen it also says whether the target already has
  the file, copies it from another machine, or why it cannot. `sidecarOptions`
  builds the options from the cluster catalog with the backend's
  compatibility rule.
- `GuideDialog.tsx` - a Documentation guide (`?guide=`) opened in a `Modal`
  (`size="xl"`) next to the fields it explains. Nothing in it navigates away:
  a reference link (`deep`) opens that document inside the dialog (loaded on
  demand from `src/docs/`), with "Back to the guide". `GuideHelpButton` is the
  round "?" button a section heading uses.
- `SidecarLinkEditor.tsx` - the model's vision / MTP link, edited in place
  (Shards) or in a window (Models) and saved on the principal through
  `POST /api/ui/model-sidecar-links`; read-only outside the `admin` profile.
- `EngineSettings.tsx` - draws the settings an inference service's engine
  publishes (`PluginService.settings`) in the interface's language: one row
  per setting, an empty field or "Engine default" keeps the engine's default,
  and the values go to `RuntimeParams.engine_settings`. Mounted by
  `DeploymentInspector` when a plugin's inference service is chosen. The POM
  settings that service declares (`PluginService.pom_settings`:
  `prefill_max_inflight_chunks`, `hidden_state_dtype`) are not in that card:
  the inspector draws them in a separate "POM settings" card below it, like
  under llama.cpp, when the slices cross machines (`showHiddenStateDtype`).
  `inference_role` (the prefill/decode split of a whole-model machine:
  prefill and decode, or prefill first, `RuntimeParams.inference_role`) is the opposite:
  that card draws it only when the model is not split across machines. A
  setting the service publishes with `placement: "pom"` (the Strata's
  `session_save`) goes to that same card in both modes, whole model and slices,
  and is not in the engine's card. A service that declares none has no such card.
- `DeploymentInspector.tsx` - the inspector of a deployment's properties, and
  **one single** one for the two screens: the deploy screen uses it editable, the History
  uses it with `readOnly` to show with which parameters a version came up. In
  read-only there is no disabled `<input>`: there is the value written out. Three
  categories, and the boundary between them is whose parameter it is - *Default
  settings* (what llama.cpp already has: context and batch; the sampling in llama.cpp's order -
  temperature, top-p, top-k, min-p, repetition penalties,
  presence and frequency, DRY - followed by the seed; base, allowed size and
  the DRY window only appear with the multiplier above 0, which is what turns DRY on;
  the installation's KV cache combos; MoE experts in RAM and the GPU expert
  cache (`--cpu-moe`, `--moe-cache-mib`); the switches; and the
  **Speculation**, one choice only - None, MTP or lookup -, because the gateway
  never runs both in the same round (`lib/speculation.ts`); the MTP's numbers
  and where it sits only appear with the MTP chosen, and the lookup's only with it), *POM settings* (disable embeddings, memory fitting,
  chunks in flight, the automatic prefill chunk, slice merging and the
  hidden-state precision; its *Beta* group is commented out while it has no
  setting) and *POM settings (DEV)* (the inference
  engine and the idle unloading per peer). Default settings and POM
  settings each have a help button by the title that opens
  their guide in a `GuideDialog`. The `[i]` comes **before** the name,
  in a straight column: after names of different widths it came out jagged.
  The engine combo does not offer "Automatic" and assumes `dense` when it mounts - that is
  presentation, the backend keeps accepting the three modes. Floating-point
  numbers go through `showNumber`, otherwise the temperature comes out as
  `0.699999988079071`. The KV cache, slice-merging and hidden-state precision controls
  only appear when whoever mounts them passes them. In the precision edit,
  f8 and i4 stay disabled until every selected peer announces support,
  and the warning uses the reason of the chosen format's capability. The Deploy Studio's compact
  combo offers f32, f16, f8 and i4; f16 stays the default, and `f32` does
  not depend on a capability. `VisionControl` follows
  the same boundary: it shows and toggles the pre-selected mmproj for whole models
  and slices, offers the compatible `mmproj_candidates` for a format choice and
  shows `mmproj_reason` when there are no candidates. With
  `sidecars` (`DeployPlan.sidecars`), the vision section and the MTP section
  render `SidecarPicker` instead, over the network's compatible files. The idle
  test provides `candidates`, `reason` and `onPathChange` of the full contract. The
  `POM settings (DEV)` section is only mounted when
  `NodeInfo.pom_dev_mode` comes true from `/api/ui/node`; when the flag is
  off, the controls disappear, but the values received stay in the state
  and keep traveling in the request when a visible control is edited. An absent mode is
  presented as `dense` (`llama.cpp`).
## Key symbols of the cluster views

- `PeerDetailCard.cardRange` (PeerDetailCard.tsx:102) - the range a card may
  show, or `null` when the machine serves nothing; never invents a "0-0".
- `PeerDetailCard.ContextReservationNotice` (PeerDetailCard.tsx) - when the
  live `/health` reports multiple sessions with KV measured on a device, the
  card shows the effective total and per-session context and warns that the KV
  reservation can raise per-call latency under tight VRAM. A single session or
  an unmeasured/CPU KV cache does not create this warning.
- `ModelPlacement.placementOf` (ModelPlacement.tsx:39) - "gpu" / "cpu" /
  "mixed" / "unknown" by the weight bytes in each buffer; `isCpuBuffer` is what
  recognizes the host (`CPU`, `CPU_Mapped`), and it is by name because that is how
  ggml names each backend's buffer.
- `Sidebar.sectionHoldsPath` (Sidebar.tsx:49) - decides whether a section is
  "where you are" (`end` exact match vs prefix) so the active section opens
  itself and the current page is never hidden.
- `RemoveNode.layersLeftUncovered` (RemoveNode.tsx:95) / `AddNodeDialog.rangeOverlap`
  (AddNodeDialog.tsx:136) - the two shared range-arithmetic helpers; the
  confirmation wording in both dialogs depends on them.
- `Confirm.tsx:Modal` (Confirm.tsx:71) - the `createPortal(document.body)` call
  that keeps `position: fixed` honest for every dialog and confirm in the app,
  not just the ones opened from `backdrop-blur` ancestors. The `openModals`
  stack (Confirm.tsx:27) is the companion rule: only the top window answers
  Escape.
## Remote models and downloads

- `RemoteModelDetails.tsx` - an on-demand window for a provider's model, with
  priority for GGUF, quantization, size, license, revision and the
  installed/downloading states. `RemoteModels.tsx` (`../routes/`) lists the results
  in a full-width table and opens this window when a row is activated;
  `RemoteModelCard.tsx`, the card the table replaced, was removed for going unused.
- `RemoteModelProviderStatus.tsx` - the full presentation of the Hugging Face CLI's
  structured state. `RemoteModels` uses only the compact login link inside the search
  bar; the trending discovery is public and does not depend on the CLI.
- `HuggingFaceSettingsPanel.tsx` - a password field for the optional token. The
  saved value is never pre-filled nor rendered: the API provides only a mask and
  the effective source, and the panel invalidates status/config after saving.
- `DownloadModelDialog.tsx` - the confirmation of a remote download; it calls only the
  structured API, shows the available space and the model folders resolved by
  the backend, without accepting a path from the browser.
- `ModelDownloadsPanel.tsx` - the progress, local destination, cancellation and retry of
  `model_download` jobs coming from `useModelDownloads`; it is reused in Remote
  Models and Transfers. The source of truth is the node and the panel creates no
  local downloader.

Links to other folders: everything imports `../lib/*` - `api.ts` (HTTP client
with connection-key auth), `queries.ts` (all React Query hooks: `useNode`,
`useCluster`, `useJobs`, `useTransfers`, `useOffers`, `useAdminNodes`,
`useInvalidate`), `types.ts` (`ClusterView`, `PeerView`, `SystemLoad`),
`format.ts` (`isServing`, `layerRange`, `serverStateLabel`, `since`, `bytes`,
`rate`, `duration`), `theme.tsx` (`useTheme`, used by Sidebar), `toast.tsx`
(`useToast`). i18n via `../i18n/provider.tsx` (`useI18n`, `LanguageSelect`) and
`../i18n/locales.ts` (`formatMessage`, `getActiveLocale`).
Backend endpoints touched directly: `POST /api/ui/unload`, `POST /api/ui/leave`,
`POST /api/ui/undeploy`, `POST /api/ui/deploy/node`,
`POST /api/ui/deploy/node/remove`,
`GET/POST /api/ui/release/candidate` + `/api/ui/release`,
`GET /api/transfer/jobs`, `GET /api/transfer/offers`,
`POST /api/transfer/offers/:id/accept|reject`; the rest go through the hooks.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
