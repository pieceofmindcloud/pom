# Goose in POM: generic ACP Chat, background services and plugin discovery

Date: 2026-11-21. Status: design, approved for planning after review.

## Intent

Bring the Goose agent (`aaif-goose/goose`) into POM, wired to the models the
node serves, in the two ways people use an agent:

- **In POM's own Chat.** Chat becomes a generic client of the Agent Client
  Protocol (ACP). Any plugin may announce an ACP agent; Goose is the first,
  served by a new `pom-plugin-goose` that only runs `goose serve`.
- **In a terminal.** `pom-plugin-harness`, the plugin for terminal agents (pi,
  OpenCode), gains a Goose tab running `goose session` - but only on a machine
  where `pom-plugin-goose` is installed too.

Why this shape: Goose dropped its browser UI (`goose web` existed up to v1.2x,
it is gone from v1.30.0; the only official UI, `ui/desktop`, is Electron). What
it ships is `goose serve`, an ACP server over HTTP and WebSocket, and the
official TypeScript client `@aaif/goose-acp-client` on npm, versioned with the
CLI. POM already has a Chat; making it an ACP client gives Goose a POM-native
interface and makes every future ACP agent free. A plugin that only installs a
CLI would duplicate Harness, so the CLI lives in Harness.

## Goals

1. Chat lists the ACP agents the machine's plugins announce, next to the model
   picker, and holds a full conversation with one: streamed text, tool calls,
   permission requests (approve/deny), sessions.
2. `pom-plugin-goose`: an on-demand plugin with no menu that downloads a
   pinned, verified Goose release, wires a `pom` provider to the node's `/v1`,
   runs `goose serve` on loopback and announces it as an ACP agent.
3. A **Background services** screen in Settings lists the running on-demand
   plugins and closes any of them, so a plugin with no menu (and so no X)
   never holds memory the operator cannot release.
4. A plugin can learn which plugins are installed on its machine and react
   when one is installed, updated or removed, in any order.
5. Harness shows a Goose tab exactly while `pom-plugin-goose` is installed.

## Non-goals

- No shared history between Chat and the Harness terminal: each plugin keeps
  its own Goose home and sessions.
- No automatic idle shutdown of `pom-plugin-goose`; it closes from the new
  screen, the existing close route or a POM restart.
- No ACP agent for visitors or workers reaching another node's agent: an
  agent runs on the machine whose Chat is open.
- No new transport between nodes; nothing here crosses machines.
- Not porting Goose's Electron UI.

## Anti-goals (a failure even if it works)

- A secret (node API key, plugin token, `GOOSE_SERVER__SECRET_KEY`) reaching
  the browser, a reply, `host.services` or a log.
- A visitor (or any non-operator session) driving an agent that runs commands
  on the machine.
- Plugins sharing files or state: discovery is read-only metadata, never a way
  to reach another plugin's binary, data or process.
- Breaking an existing contract: every change below is additive (compatibility
  policy in `AGENTS.md`).
- Harness killing an open terminal tab when Goose is uninstalled.

## Constraints

- Plugin contract: C ABI v1, `pom-plugin-ui/v1`, `pom-plugin-events/v1`;
  `query` answers within the 3 s IPC timeout; slow work on plugin threads;
  children die with the host (`creating-pom-plugins` skill, `plugins-ui.md`).
- Downloads are pinned and verified against the SHA-256 digest GitHub
  publishes for each release asset; nothing vendored.
- Platforms: the Goose CLI ships Linux gnu/musl x86_64 and aarch64, macOS x86_64
  and aarch64, Windows x86_64. The plugin targets the same set POM releases.
- The Chat proxy reuses `/api/ui/plugins/<code>/proxy/*`, which already carries
  WebSocket upgrades and adds `x-pom-plugin-token` server-side.

## Approaches considered

**Where `goose serve` runs.**
- A. Its own plugin, `pom-plugin-goose`, on demand with no menu. **Chosen.**
  Keeps Harness about terminal agents, installs and updates on its own, and
  turning it off leaves the terminal alone. Cost: a second Goose binary on a
  machine with both plugins, and separate histories.
- B. A Harness preference "Serve Goose to Chat". Shared binary and history, but
  Harness stops being only terminal agents and Chat depends on a terminal
  plugin. Rejected.

**How Chat knows an agent exists.**
- A new `host.services` kind `acp_agent`. **Chosen**: additive to an existing
  contract the node already polls and publishes.
- A new manifest section. Rejected: the agent's readiness is runtime state,
  which `host.services` already models.

**Which Goose UI.**
- Pin an old v1.2x for `goose web`: frozen ~40 releases back. Rejected.
- Terminal only: duplicates Harness. Rejected (it is the Harness piece).
- POM Chat as ACP client over `goose serve`. **Chosen.**

**How Harness knows Goose is installed.** A read-only `host_api` listing plus
lifecycle events. **Chosen** over Harness probing the filesystem or another
plugin's proxy, which would break the process boundary.

## Design

Six deliveries, in this order; each is shippable alone.

### 1. Plugin discovery (POM)

- `GET /api/plugin-host/plugins` on `host_api` (same token header as the other
  `plugin-host` routes) returns
  `{"plugins": [{"code", "version", "lifecycle", "running"}]}` for the plugins
  installed on this machine, the caller included. No license, path, token or
  preference.
- New events in `pom-plugin-events/v1`, delivered through `host.event` to every
  running plugin: `plugin.installed`, `plugin.updated`, `plugin.removed`, each
  `{"code", "version"}` (`version` absent on `removed`). A plugin that starts
  later learns the state from the listing, so order of installation does not
  matter.
- Docs: `plugins-ui.md` (host API, events), the skill's `reference.md`.

### 2. ACP agent contract (POM)

- Manifest field `proxy_profiles` (optional, list of profiles): the node
  refuses the plugin proxy (403) to sessions of other profiles; absent keeps
  today's open proxy. Documented in `plugins-ui.md`, checked by the
  contract tests.

A `host.services` entry of `kind: "acp_agent"` declares one agent:

```json
{"id": "goose", "kind": "acp_agent", "name": "Goose", "status": "ready",
 "version": "1.60.2", "acp": {"path": "/acp", "protocol_version": 1}}
```

- `acp.path` is a path on the plugin's own upstream (`ui.upstream`), reached by
  the browser as `/api/ui/plugins/<code>/proxy<path>`; it must start with `/`,
  no `..`, at most 128 bytes. `protocol_version` is the ACP major version.
- The node validates the new fields like the others (an invalid entry is
  dropped) and publishes agents in `plugin_services` of `GET /api/ui/node`.
- Agents of a **stopped on-demand** plugin must stay listable: the node keeps
  the last valid `acp_agent` entries per plugin in `pom.db` (as it keeps the
  saved manifest) and reports them with `status: "stopped"`. `stopped` is a new
  status value only for this kind.
- A new route `GET /api/ui/chat/agents` returns the agents for the Chat:
  `[{plugin_code, id, name, version, status, proxy_path}]`, operator and admin
  sessions only (others get an empty list).

### 3. Chat as a generic ACP client (POM frontend)

- An agent picker beside the model picker, shown only when
  `/api/ui/chat/agents` is non-empty. "Model" (today's behaviour) stays the
  default; choosing an agent switches the conversation to ACP.
- Choosing a stopped agent calls the existing `POST
  /api/ui/plugins/<code>/open`, shows "Starting <name>" until the agent reports
  `ready`, and an error with retry if it reports `error`.
- Transport: the browser opens the WebSocket at the proxy path with the admin
  session cookie; `@aaif/goose-acp-client` (pinned, bundled into the frontend)
  speaks ACP over it. If the client turns out to be Goose-specific in ways
  that break the generic promise, the builder wraps a thin generic ACP layer
  and uses the Goose client only where Goose extends the protocol.
- What the screen draws: streamed agent text in the existing message style;
  each tool call as a collapsible card (title, status, arguments, result);
  each `session/request_permission` as an inline card with the options the
  agent offers (allow once, always, reject); plans/thoughts if the agent sends
  them. The model for the session comes from POM's catalog and is passed to
  the agent when ACP allows it.
- Sessions: the Chat session list shows agent conversations with the agent's
  name; reopening one resumes it through ACP `session/load` when the agent
  supports it, otherwise the transcript is read-only.
- Never shown to visitors; no telemetry slot on agent answers.

### 4. Background services screen (POM frontend + small route)

- Settings gets **Background services**: every on-demand plugin that is
  running, with name, icon, version, its `host.services` statuses and, when
  the node can measure it, the host process's resident memory.
- Each row has **Close**, calling the existing `POST
  /api/ui/plugins/<code>/close` (confirmation first). Stopped on-demand plugins
  appear below as stopped with **Open**.
- Memory: the node already supervises `pom-plugin-host` processes; it adds
  `memory_bytes` (the host's process tree) to `GET /api/ui/plugins/ui` for
  running plugins. Omitted where the platform cannot measure it.
- A guide in `docs/guides/` for the screen, per the documentation duty.

### 5. `pom-plugin-goose` (new repository)

Scaffolded with `creating-pom-plugins`; structure like `pom-plugin-openmausbot`
minus Node.

- Manifest: `lifecycle: "on_demand"`, `proxy_profiles: ["admin"]`, no menu, no routes beyond what the
  contract needs; icon, description and docs so the Plugins screen and the new
  screen show it.
- Installer thread: downloads the pinned Goose CLI release asset for the
  platform, checks it against the pinned SHA-256, unpacks under
  `<plugin data dir>/goose/<version>/`. Pin: the latest stable release at least
  two weeks old when implementing.
- Configuration: a private `HOME`/`XDG_CONFIG_HOME` under the plugin data dir;
  a Goose custom OpenAI-compatible provider `pom` pointing at
  `gateway.openai_base_url`, the key passed only through the environment;
  models and limits from `/v1/models`, refreshed periodically; working
  directory the POM `workspace_root`.
- Process: `goose serve --host 127.0.0.1 --port <free>` with a random
  `GOOSE_SERVER__SECRET_KEY` per start, in the plugin's process group.
- Bridge: a loopback HTTP/WebSocket server (the `ui.upstream` target) that
  accepts only the POM plugin token, injects Goose's secret, and forwards to
  `goose serve` (`/acp` and whatever ACP needs). Same model as OpenMausBot's
  `src/bridge.rs`.
- `host.services`: one `acp_agent` entry, `installing`/`starting`/`ready`/
  `error` as the supervisor sees it.
- Responds to `preferences.changed`/`host.configure` changes (new key or
  gateway) by rewriting the provider and restarting `goose serve`.
- Release workflow and license-server upload from the template; added to
  `pom-cluster-config/scripts/clone-repos.sh`.

### 6. Goose tab in Harness (`pom-plugin-harness`)

- At start Harness calls `GET /api/plugin-host/plugins`; afterwards it follows
  `plugin.installed`/`plugin.removed`. Goose agent enabled iff a plugin with
  code `goose` is installed (any version for now; the version is there for a
  future minimum).
- Enabled: Harness downloads its own pinned Goose CLI (verified SHA-256, a
  host install answering `--version` reused like OpenCode), writes the same
  kind of `pom` provider in its private home, and opens a Goose tab in herdr
  running `goose session` in the workspace; `goose session --resume` works
  there. Added to `runtime/agents.mjs` beside OpenCode, listed in
  `pom-agents`.
- Removed: the `goose` command is removed from the private PATH; an open tab
  is left alone.
- Harness README/AGENTS.md updated.

## Resolved after review

- **Proxy access (security).** `/api/ui/plugins/:code/proxy/*` is open to every
  profile today (`plugins-ui.md`), so hiding agents in `/api/ui/chat/agents`
  alone would let a visitor session reach Goose. Fix (confirmed by the operator): an optional manifest field
  `"proxy_profiles": ["admin"]`, additive, enforced by the node before
  proxying; absent keeps today's behaviour. `pom-plugin-goose` declares it.
  "Operator" means a session of profile `admin` on this node.
- **Key and accounting.** Both plugins use `host.configure.gateway.api_key`,
  the node's chat key (`pom-<node_id>`), as every plugin does; usage is
  audited like the UI Chat's. No new key.
- **Model per conversation** (designer's call). The Chat's model picker stays
  visible with an agent selected; the chosen POM model is sent through ACP
  session configuration when the agent advertises it, and the picker is
  hidden for agents that do not. No restart of `goose serve`.
- **Agent history** (designer's call). POM stores no agent transcript: the
  session list asks the agent (`session/list`), reopening uses
  `session/load`; agents without them show only the current conversation. No
  schema migration in delivery 3.
- **Persisted agents cleanup** (designer's call). The saved `acp_agent`
  entries of a plugin are deleted with its saved manifest when it is
  uninstalled, and replaced whenever the running plugin reports a new list.
- **Windows.** The host already kills its process tree on Windows (Job Object,
  `enterprise-licensing.md`), so no platform is scoped out.
- **Background services access** (designer's call). Shown to `admin` sessions
  only; Close and Open only for on-demand plugins, as the existing routes
  enforce (409 for resident ones).

## Left to the builder

- Exact ACP messages and client API usage, after reading
  `@aaif/goose-acp-client` and the ACP spec at the pinned version.
- Goose's custom provider file format and how it takes context/output limits.
- Whether `goose serve` needs `--allowed-origin` behind the bridge (the bridge
  can strip `Origin` instead).
- Memory measurement per platform.
- Visual details of the cards and the screen, within POM's design kit.
- Test layout: route tests with `gateway::testkit`; plugin contract tests from
  the template.

## Calls made by the designer (check these)

- Discovery returns the caller too, and no data beyond code/version/lifecycle/
  running.
- `stopped` status and persisted agents so Chat can wake a stopped plugin.
- Agents only for operator/admin sessions on the same machine.
- No idle auto-close.
- Harness keeps its own Goose copy instead of reusing the Goose plugin's.
