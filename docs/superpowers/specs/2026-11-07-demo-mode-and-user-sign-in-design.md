# Demo mode and per-user sign-in (OAuth through a plugin)

Status: approved design, implementation in progress (`feat/demo-oauth` here,
`feat/oauth-users` in `pom-plugin-logs`).

## Goals

1. A public demonstration node: a worker on a VPS, started with `DEMO_MODE=1`,
   that anyone can open to watch the dashboard, see which machines serve the
   model and run inference - without leaving the network, changing anything or
   learning privileged facts (addresses of the machines, relays, keys).
2. People, not only nodes: a person signs in through an OAuth/OIDC provider
   (any number of providers, Google is only a preset), gets a user identity on
   the node, a quota, and sees only their own requests, statistics and
   telemetry in the logs plugin.
3. The same mechanism sells as the Business pack: an organization's members
   sign in with their own identity provider and mint their own API keys
   (refused in demo mode).
4. The POM core changes little. It knows no provider, no OAuth protocol and no
   user table: the logs plugin owns all of it.

## Non-goals

- The site's own token (the license server acting as issuer) is a later
  provider preset, not part of this delivery.
- No change to the trust chain, membership tokens or node-to-node
  authorization: a user is a person on one node, never a network member.

## Identities on a node

| Session | How it starts | What it reaches |
| --- | --- | --- |
| Operator | The node password, posted to the operator path | Everything it reached before; passes the demo filter |
| User | A provider sign-in completed by a plugin | The user surface only (below) |
| (loopback, local mode) | Unchanged | Unchanged |

`AdminSessionStore` keeps the session kind. The UI gate puts a
`SessionPrincipal` (`Operator` or `User { user_id, label, plugin }`) on the
request; handlers that care read it.

### Operator path (the password is no longer on the login screen)

- When at least one plugin offers a sign-in provider, the login screen shows
  only the providers. The password form is served only at the operator path:
  `POM_OPERATOR_PATH` (a path such as `/_k7q-console`), or a random path made
  on first start and kept in the config, printed once in the start-up log.
- The password is accepted only by `POST <operator_path>/login`; the
  `x-pom-access-token` header stops opening sessions on any other path while
  sign-in providers are offered. Anything else under the operator path, and the
  path when wrong, answers like an unknown route.
- Failed attempts are throttled per client address and globally (exponential
  lock-out). Operator sessions last 30 minutes of idleness instead of 8 hours.
- Without providers nothing changes: the login screen keeps the password.

## Demo mode (`DEMO_MODE=1`)

Read once at start (`crate::demo::enabled()`). It changes the user surface
only; an operator session is unaffected.

- **User surface**: one allowlist table (`webui/user_surface.rs`) of method and
  path prefixes a user session may call - the dashboard's read routes, the
  model list, the chat key, the plugin UI assets and RPCs, request logs and
  statistics, `/api/ui/me`, own API keys. Every other path answers 404 to a
  user session. The allowlist is the same with and without demo mode, except
  that demo removes own API keys.
- **Redaction**: in demo mode the JSON a user session reads from the
  dashboard routes passes through `demo::redact`, which removes every field that
  names an address (`addr`, `ip`, `host`, `endpoint`, `relay`, `url`, `pkarr`,
  socket-address-shaped strings) and shortens node ids to a stable alias.
- **No anonymous access**: without a session the node serves only the login
  screen, as today on a network-exposed node.
- The interface reads `GET /api/access` (`demo_mode`, `auth_providers`) and
  `GET /api/ui/me` (`kind`, `user`) to hide what the user cannot use. Hiding is
  cosmetic; the gate is the guarantee.

## Sign-in through a plugin (generic)

### Core side

- `GET /api/access` (public) gains `auth_providers: [{plugin, id, label, icon,
  start_url}]`, gathered from every active plugin that answers the query
  `auth.providers` (cached 10 s, a plugin that fails is skipped).
- `/auth/<code>/*` is public (no session) and forwarded to plugin `<code>` as
  the query `auth.http` with `{method, path, query, form, redirect_base}`.
  `redirect_base` is `POM_OAUTH_REDIRECT_BASE` or the request's own origin.
- The plugin answers `{status?, redirect?, html?, json?, session?}`. When
  `session: {user_id, label, email?, provider}` is present the core opens a
  user session, sets the cookie and redirects to `redirect` (default `/`). The
  plugin never sees or sets a cookie.
- A user session gets its inference key from `POST /api/ui/chat-key` as an
  in-memory token bound to the session (dies with it), never the node key.
  `Caller` gains `user_id`, so the request log, quota and audit carry it.
- Plugin RPCs and the request log queries from a user session carry
  `profile: "user"` and `user_id`; plugins must filter by it.
- Own API keys (outside demo): `GET/POST /api/ui/my-keys`,
  `DELETE /api/ui/my-keys/:id`, stored in `api_keys` with a `user_id` column.

### Plugin side (`pom-plugin-logs`)

- Providers are records: `id, label, icon, preset, issuer, authorize_url,
  token_url, userinfo_url, client_id, client_secret, scopes, claim_sub,
  claim_email, claim_name, allowed_domains, default_plan, enabled`. Presets
  (`google`, `microsoft`, `github`, `gitlab`, `oidc`) only fill fields.
- From the environment, numbered: `POM_OAUTH_<n>_ID`, `_PRESET`, `_LABEL`,
  `_ISSUER`, `_CLIENT_ID`, `_CLIENT_SECRET`, `_SCOPES`, `_AUTHORIZE_URL`,
  `_TOKEN_URL`, `_USERINFO_URL`, `_ALLOWED_DOMAINS`, `_DEFAULT_PLAN`. Env
  providers win over stored ones with the same id and are read-only on screen.
- From the Authentication screen (admin or operator only): create, edit,
  disable providers; the secret is write-only.
- Flow: authorization code with PKCE (S256), `state` and `nonce` kept in
  `oauth_pending` (single use, 10 minutes). The code is exchanged at the token
  endpoint over TLS; per OIDC Core 3.1.3.7 the id_token received directly from
  the token endpoint is trusted by TLS, and `iss`, `aud`, `exp`, `nonce` are
  checked. Providers without OIDC (GitHub) read the userinfo endpoint.
- A user is `(provider_id, sub)`, never the e-mail. Any account may sign in
  unless the provider sets `allowed_domains`. New users receive the provider's
  `default_plan` quota assignment.
- `requests.caller_json.user_id` scopes every read for `profile: "user"`:
  list, stats, insights, request detail and telemetry. Quota gains the `user`
  scope and `peer.quota.check` binds on it.

## Configuration of the demonstration VPS (Coolify)

```
DEMO_MODE=1
POM_OPERATOR_PATH=/_some-unguessable-path
POM_OAUTH_REDIRECT_BASE=https://demo.example
POM_OAUTH_1_ID=google
POM_OAUTH_1_PRESET=google
POM_OAUTH_1_CLIENT_ID=...
POM_OAUTH_1_CLIENT_SECRET=...
POM_OAUTH_1_DEFAULT_PLAN=demo
```

Callback to register at the provider: `<redirect_base>/auth/logs/oauth/<id>/callback`.

## Known limits

- The session chat token can be copied from the browser and used for the life
  of the session (8 h). It carries the user's id and quota, so it is no more
  than the session itself.
- Redaction is a deny-by-shape pass; a new dashboard field that names an
  address in an unexpected key must be added to `demo::redact`'s tests.
