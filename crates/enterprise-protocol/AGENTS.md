# enterprise-protocol

Shared crate of the plugin licensing client contract. Defines the licensing
JSON DTOs and the C ABI v1 used exclusively by `pom-plugin-host`; it does not
load a dynamic library, do networking or persist a license. The crate name
stayed as it is for git history - the contract is per `plugin_code`, not tied
to a single product: `ActivationPayload`, `ActivationResponse` and `GrantBody`
carry `plugin_code`, and one key can grant several. The DTOs must stay
compatible with the server at `license.pieceofmind.cloud`, including the
envelope, signed manifests/grants and lease states.
Authenticated responses also carry granted plugins, candidates with an
activatable release, expiration and activation usage per `plugin_code`; POM
keeps this data in the local status without exposing tokens.
`PluginManifest.preferences` is optional and typed to preserve older responses;
it only defines the schema of the local preferences interface and is never an
entitlement.

It also publishes `MAX_IPC_FRAME_BYTES`, the shared limit for JSON frames in
the local stdin/stdout communication between `pom` and `pom-plugin-host`.

The `pom_core` update notice **does not** live here: it left this crate with
the plugin pivot and is an internal type of `crates/node/src/pom_core_update.rs`,
because the `pom` binary is not a plugin and its update is not part of the
license contract. The architecture reference and the remaining limits are in
`docs/subsystems/enterprise-licensing.md`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.