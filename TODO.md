# TODO - defects noted in the AGENTS.md files

Work list consolidated on 2026-09-07 from an audit of the `AGENTS.md` annotations,
at revision `985cfb51183c4fa1b509524e54d39aeda78b1cc8`.
The full evidence for each item is in the audit report that comes with this task.
The current list keeps the audit's numbering. Rechecked against the code on
2026-10-17: items 6, 12, 15, 20 and 22 were closed (fixed, or the wizard
deprecated), and the engine items moved to their own section. Items added
after the audit continue the numbering
from 27.

## Maintenance rule

When you fix a current numbered item, remove the annotation from the matching
`AGENTS.md` and the item from this file in the same commit. A fixed defect that
stays annotated is worse than an unannotated one. The notes under
`## Preserved history`, `## Refused` and `## Open documentation debt` are
context and must not be proposed again as new defects.

## NAT traversal (IPv4 with no public address)

Delivered on 2026-09-22, in six parts, under libp2p. On 2026-10-17 the
transport moved to iroh, which does N1, N2 and N5 on its own (relays, hole
punching and relay URLs as addresses); N0, N3 and N4 carried over. The current
design is in `docs/subsystems/p2p-network.md`, sections 3 and "NAT traversal";
this list stays as a record of what was done and has no open item.

- [x] **N1 - A swarm that can do traversal.** QUIC in the `SwarmBuilder` and in the listen
  addresses; `DEFAULT_RELAY_NODES` emptied, because the IPFS bootstrapper does not
  grant a Circuit Relay v2 reservation; `reserve_circuit` asks any peer that advertises
  `relay::HOP_PROTOCOL_NAME` for a reservation; `NatStatus::Public` calls `Swarm::add_external_address`.
- [x] **N2 - Publish and dial real multiaddresses.** `Announce`/`PeerInfo`
  carry `libp2p_addrs`, fed by `publish_swarm_addrs`;
  `libp2p_dialable_addrs` returns all of them; `tunnel_peer_multiaddrs` accepts
  a full multiaddress and the legacy `host:port`.
- [x] **N3 - A control path through the swarm.** `TunnelRequest.target` chooses
  between inference and gateway; `gateway_path_is_allowed` is the closed list
  checked on the receiving side, because what leaves the tunnel reaches the gateway over
  `127.0.0.1`. The admission `/handshake` probe and the policy gates fall into
  the tunnel once every direct address has failed.
- [x] **N4 - A preference the tunnel learns.** `PeerClients::prefer_tunnel`
  turns on when the tunnel delivers after the whole direct ladder has failed, and
  turns off at the first direct success; when it is on, the frame skips QUIC and HTTP
  on the first attempt.
- [x] **N5 - pkarr publishes the confirmed address.** `public_libp2p_addr`
  wins over the multiaddress derived at startup. Circuit addresses stay out: the
  record is republished every 30 min and a reservation does not reliably last that long.
- [x] **N0 - Observability.** `TraversalStatus` in `/api/ui/cluster` and in
  the "NAT traversal" panel on the Network screen.

## High

- [ ] **13 - Rollback is not passed on to TP**
  **Source:** `crates/node/src/inference/infer_server/server.rs` (`handle_rollback`,
  `rollback_session`; compare `open_tp_decode`); call site in
  `crates/node/src/backend/gateway/generation.rs` (`/v1/session/rollback`).
  **In practice:** `/v1/session/rollback` handles the local rank but does not replicate
  the discard of the rejected suffix to the workers; with TP speculation on, the KV
  caches can drift apart.
  **Who it affects:** tensor-parallel deployments that enable speculative decoding.
  **Effort:** medium, including headers, the rank 0 fan-out and tests with two
  ranks.

## Medium

- [ ] **5 - The open-core planner is not wired into the node**
  **Source:** `crates/routing-base/src/lib.rs:12-62`;
  `crates/node/src/routing/pipeline.rs:262-283,379-410`.
  **In practice:** `routing-base::plan` is complete and tested, but the node uses a
  different greedy planner with load scoring; the two can drift apart with no equivalence
  test.
  **Who it affects:** consumers of the open provider and maintainers of both
  planners.
  **Effort:** medium for equivalence; high to swap the production path over
  with compatibility and telemetry.

- [ ] **16 - The relay's failure modes have no E2E test**
  **Source:** `crates/node/src/inference/direct.rs`;
  `crates/node/src/inference/direct_lab.rs` (covers credentials, entry cost and
  positions, but no dead hop, dead tail or timeout scenario).
  **In practice:** a dead middle hop, a dead tail, a lost direct response and a timeout all
  have code and unit tests, but no four-peer scenario that reproduces the full topology.
  **Who it affects:** the reliability of the distributed pipeline.
  **Effort:** high, with a deterministic multi-peer harness, injectable failures and
  isolation from the real network.

- [ ] **24 - The contract still uses an inclusive end**
  **Source:** `crates/routing-contract/src/lib.rs:16-24,65-73,81-91,131-167`.
  **In practice:** the v1 wire validates and walks `end_layer` as inclusive; the future
  version with an exclusive end only appears as an intention, with no conversion or implementation.
  **Who it affects:** protocol evolution and future interoperability.
  **Effort:** medium when it is needed, including versioning, validation,
  serialization and compatibility tests.

- [ ] **26 - A manual build can lose the binary's commit**
  **Source:** `scripts/build-docker.sh`; `Dockerfile` (runtime `ARG POM_BUILD_COMMIT`).
  **In practice:** a manual `docker compose build` without `POM_BUILD_COMMIT`
  stamps an empty commit (the `Dockerfile` default is `""`); `build-docker.sh`
  itself falls back to `desconhecido` outside a git checkout.
  **Who it affects:** manual builds and pipelines that do not export the variable;
  the binary's diagnostics get degraded.
  **Effort:** low, with validation in the Dockerfile or an explicit requirement for the
  variable, keeping local builds without Docker working.

## Low

- [ ] **27 - The toolchain scenario of the Docker cache test is wrong**
  **Source:** `scripts/test-docker-build-cache.sh`; `scripts/AGENTS.md`.
  **In practice:** the "Rust toolchain changed" scenario fails with no change in
  the Dockerfile: the fork release download comes after rustup, falls in the
  `base` group, and the pinned test toolchain is cached from the second run on.
  **Who it affects:** whoever relies on the script to validate the Docker cache.
  **Effort:** low.

- [ ] **23 - Partial fixtures can break tests**
  **Source:** `apps/frontend/src/test/fixtures.ts:1-8`;
  `apps/frontend/src/components/PeerDetailCard.tsx` (`n?.status.state`).
  **In practice:** the shared fixtures already build the complete shape by
  default; what remains is the read `n?.status.state`, which guards `n` but
  not `status`, so a hand-written partial fixture still breaks it.
  **Who it affects:** the maintenance of new tests and false diagnostics during the
  development.
  **Effort:** low, with mandatory typed constructors or fixture validation.

- [ ] **25 - The public types crate is still a placeholder**
  **Source:** `crates/types/src/lib.rs:1-12`.
  **In practice:** `types` only exposes `WorkspaceMarker`; `PeerInfo` and `NodeConfig`
  stay inside `node`.
  **Who it affects:** the open-core boundary and consumers that expect public types.
  **Effort:** medium, extracting types without creating cycles and updating
  consumers.

## Belongs to the engine repository

`crates/llama-engine` left this repository for the private
`pom-plugin-llama.cpp`; the sources below point there and cannot be verified
from this checkout. Move them to that repository's backlog.

- [ ] **10 - The build depends on a full extraction of the fork**
  **Source:** `crates/llama-engine/build/mtp_arch.rs:25-47,104-114,178-186`.
  **In practice:** the build fails when the fork is missing or the extraction of
  MTP architectures comes out empty, ambiguous, or below the safety floor.
  **Who it affects:** onboarding, CI and releases without the right checkout.
  **Effort:** medium to make the preparation reproducible; high to remove
  the local dependency.

- [ ] **14 - The context pool is not integrated**
  **Source:** `crates/llama-engine/src/llm.rs:3200-3214`.
  **In practice:** `LlmEngine::pool` exists and has budget tests, but the
  current server loads one context per `load` and does not use the API.
  **Who it affects:** anyone expecting concurrent decodes on the same model.
  **Effort:** medium/high, with a slot policy, memory, isolation and
  telemetry.

- [ ] **18 - MoE diagnostics are ignored by default**
  **Source:** `crates/llama-engine/src/llm.rs:958-960`.
  **In practice:** the test only runs when `POM_MOE_ROUTE_DIAGNOSTICS=1`; a
  regression can slip through the normal suite.
  **Who it affects:** diagnosis and maintenance of the MoE path.
  **Effort:** low/medium, separating the deterministic assertion from the logging
  part and documenting the cost.

- [ ] **17 - The CUDA differential proof for MoE has no execution environment**
  **Source:** `crates/node/src/inference/AGENTS.md:359-375`;
  `crates/llama-engine/src/llm.rs:6268-6273`.
  **In practice:** the proof needs a real GGUF, `--features cuda`, a CUDA GPU and the
  external executor; none of those were available during the audit.
  **Who it affects:** the numerical validation of the CUDA MoE path.
  **Effort:** high, on a machine with a compatible GPU and the real GGUF.


## Preserved history

The previous TODO came from the 2026-08-15 survey, was last cleaned on
2026-08-17 and recorded 29 items triaged by the operator, of which 23 had
been closed, 4 were deferred and 2 refused. That count is historical and is not
replaced by the classification from the 2026-09-07 audit.

The old bitswap item stays recorded as resolved: the file and the behaviour left
the swarm, and content transfer now uses the `.pom` and `GET /api/transfer/*?sha=`
over HTTP Range. In the new audit it is item 21, an obsolete annotation.

The old notes about switching between observed addresses and about the 75 s
`connect` timeout are also preserved as history outside the new audit. The
current revision already describes the preference for the local address and the
peer client caps the TCP open at 2 s; they are not counted as current items of
this list.

## Open documentation debt

- The workspace graph is documented correctly in `crates/AGENTS.md` (old item 6,
  closed on 2026-10-17); the larger decision about extracting the crates stays
  recorded here so it is not lost.
- The contract uses an inclusive `end_layer`. The future move to an exclusive end
  is consolidated in item 24 and requires a new wire version.

## Items from the earlier triage, incorporated

The deferred `routing-base::plan`, `undeploy_authority` and `crates/types` became
items 5, 12 (closed) and 25 respectively, with the evidence updated. None
of that context was discarded.