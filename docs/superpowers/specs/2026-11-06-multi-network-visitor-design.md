# Visiting several networks at once

Date: 2026-11-06 - Status: proposal for review

## Problem

A node belongs to one network at a time. `Config` (`crates/node/src/utils/config.rs`)
holds one `connection_key`, one `network` block and one profile; the node holds
one trust chain, one membership token and one peer table. Joining another
network puts the node's own network in standby (`backend/webui/standby.rs`)
and leaving brings it back.

A person who uses models from several networks (their company's, a partner's,
a public one) has to leave one to use another. Nothing about a visit needs
that: a visitor never serves layers, never receives model files and is never a
deploy target. It only discovers the network's peers, plans a pipeline over
them and is the gateway of its own requests.

## Goals

- A node is a **visitor of any number of networks at once**, while its own
  network stays live (not in standby).
- Every model served by any of them is addressable as `network:model`, on the
  OpenAI-compatible routes and in the UI.
- Each membership keeps its own credential, trust chain, token, peer view and
  route plans; nothing learned in one network is used in another.
- Existing clients and `pom.json` files keep working.

## Non-goals

- Serving, deploying or holding models in more than one network. Joining as
  **worker or admin** keeps today's single-membership flow with standby.
- One request split across networks (a pipeline spans one network).
- Merging peers that appear in two networks into one entry.

## Decisions

### 1. Memberships are a list

`Config` gains `memberships: Vec<Membership>`, one entry per visited network:
network id and display name, slug, connection key, trust root, membership
token, bootstrap and relay hints, and the admission state (`pending`,
`active`, `expired`, `revoked`). The node's own network stays where it is
today (the top-level fields), so a node that never visits sees no change.

Compatibility: an older `pom.json` has no `memberships` and loads as an empty
list. A node currently visiting one network (own network in standby) migrates
on load: the visited network becomes the first entry of `memberships` and the
standby file is restored as the live own network. The standby file stays the
format for worker/admin joins. An older node reading a newer `pom.json`
ignores the unknown field and sees only its own network; that downgrade is
written in the release notes.

The connection keys in `memberships` are secrets like the own network's key:
`pom.json` keeps its owner-only mode, and no screen or API ever returns them.

### 2. One runtime per membership

Each active membership gets its own instance of what is a singleton today:
trust chain view, membership token refresher, discovery (gossip topic and
announce target derived from that network's key), peer table and route plan
book. They share the process, the node key, the iroh endpoint and the mutual
TLS identity, which are already network-independent (`node_id` is the
machine's name everywhere).

A visitor membership runs the visitor subset only: it announces to admins and
workers as today, never admits, never relays for the network unless the
anonymous visitor relay work says otherwise, and refuses every serving route
for that network as a visitor already does.

A peer that belongs to two networks appears once per network. Calls to it
carry the credential of the network the request belongs to; the route plans
are kept apart so a measurement in one network does not pin a route in
another.

### 3. Model ids are `network:model`

The network's **slug** (lowercase, `[a-z0-9-]`, unique among this node's
memberships and own network) prefixes the model id with `:`. A colon never
appears in a slug, so the id splits on the first `:` only; model names that
contain `/` (`org/model`) or `:` keep working.

- `/v1/models` lists every model of every live network as `slug:model`, with
  the added fields `x_pom.network` (slug, display name, role) and
  `x_pom.model` (the bare name).
- A request with `slug:model` plans only over that network's peers.
- A request with a bare `model` resolves as today when exactly one network
  (own included) serves it; served by several, it fails with a message listing
  the qualified ids. Bare names on a node that visits nothing behave exactly
  as before.
- Slug collisions (two visited networks with the same name) are solved at
  join time: the second gets a suffix (`acme-2`) the operator can rename.

### 4. API keys stay local, with an optional network allowlist

Keys never leave their node, as today. Each key gains an optional `networks`
list; empty means every network this node reaches. A request whose network is
not in the list is refused with `403 network_not_allowed`.

### 5. Leaving and losing a network

Leaving a visited network removes its entry, stops its runtime and drops its
peers and route plans. A revoked or expired token puts the entry in `revoked`
or `expired`: its models leave `/v1/models`, a request naming them fails with
the reason, and the entry stays on the Networks screen until the operator
removes it or rejoins.

## Changes by area

### Backend

- `utils/config.rs`: `Membership`, `memberships`, load-time migration from the
  standby form, tests for both directions.
- `backend/webui/standby.rs` and `networks.rs`: a visitor join adds a
  membership instead of swapping networks; worker/admin joins keep standby.
- `network/` (discovery, gossip, iroh, route plan): one runtime per membership,
  keyed by network id; no global "current network" read on the hot path.
- `membership.rs`: token storage and refresh per network.
- `backend/gateway/` and `backend/openai.rs` (`list_models`): resolve
  `slug:model`, plan within the network, the bare-name rule, `x_pom.network`.
- `backend/webui/routes/mod.rs`: every network-scoped UI route takes a
  `network` parameter (defaulting to the own network, so today's UI keeps
  working); new routes to list, join, leave and rename memberships.
- Request log and audit: an optional `network` field on each record.

### UI

- **Networks** (`PublicNetworks.tsx`, `NetworkSettings.tsx`): a list of
  memberships (name, role, status, peer count, Leave, rename slug); Join stays
  available while visiting; pending admissions show per network.
- **Network context switcher** in the header (`components/nav.ts`): the
  profile becomes per network (admin of the own network, visitor elsewhere)
  and the menu follows the selected network. Screens that support it offer
  "All networks".
- **Chat** (`Chat.tsx`): model picker grouped by network with its status;
  sends `slug:model`, shows "network / model"; the telemetry and the saved
  conversation name the network, and a conversation whose network is gone
  says so.
- **Models served** (a new view, or a section of the Chat picker; the existing
  `RemoteModels.tsx` is the Hugging Face search and stays as is): grouped or
  filtered by network, with the copyable `slug:model` id.
- **Dashboard and map** (`Dashboard.tsx`): follow the selected network.
- **Network Chat** (`NetworkChat.tsx`): one thread per network.
- **My budget** (`MyBudget.tsx`): one section per network.
- **API keys** (`Keys.tsx`): the optional network allowlist; examples use
  `slug:model`.
- **Notifications**: every event names its network.
- Guides under `docs/guides/screens/` for each screen above, and a concepts
  guide on visiting several networks.

### Documentation

`p2p-network.md`, `http-api.md`, `security-model.md`, `web-ui.md`,
`inference.md` and the root `AGENTS.md` invariant on the own network
(standby now applies only to worker/admin joins).

## Testing

- Config: load of an old single-network file, of a visiting-in-standby file
  (migrated), and round trip of `memberships`.
- `gateway::testkit` with two fake networks sharing one peer: model listing,
  `slug:model` routing, bare-name ambiguity, credentials per network, a key
  with an allowlist, a revoked membership.
- Isolation: a route plan or peer learned in network A is never used for a
  request to network B.
- Live: one node visiting two networks in isolated containers (see the
  two-node container recipe in `docs/subsystems/operations.md`).
- UI: Vitest for the switcher, the grouped picker and the Networks list.

## Open questions

- Should a visitor's own network be visible to the networks it visits (as a
  second identity of the same node)? Proposed: no, nothing changes in what a
  visitor announces.
- Limit on concurrent memberships (each costs discovery traffic and a gossip
  subscription)? Proposed: a soft limit of 8, configurable.
- Network Chat: one global unread badge or one per network?

## Phases

1. Config memberships and migration; join/leave as visitor without standby.
2. Per-membership runtime (discovery, token, peers, route plans).
3. Gateway and `/v1/models` with `slug:model`, key allowlist, audit field.
4. UI: Networks, Chat, models served; then the switcher, Dashboard, Network
   Chat, My budget, notifications; guides.

## Out of scope

- Multi-network membership as worker or admin.
- Cross-network pipelines.
- Federated identity across networks (each network still admits the node on
  its own).

## Related

`2026-11-07-member-gateway-planning-design.md`: when a visited network plans
through its members, the visitor needs only a route to an entry node of that
network, not route plans and connections to its workers, which shrinks the
per-membership runtime of decision 2.
