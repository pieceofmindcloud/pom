# plugin-host

Auxiliary binary `pom-plugin-host`, responsible for loading the `cdylib` of
ONE plugin in a separate process and exposing the host's framed JSON IPC. It
validates the absolute path, the manifest, the hash, the ABI version and the
callbacks before creating the handle. The `pom` process must not import
`libloading` or load the library directly. Besides `ready`, `ingest` and
`shutdown`, the IPC exposes `query` with a safe copy of the buffer returned by
the library and a shared 16 MiB limit defined by
`enterprise_protocol::MAX_IPC_FRAME_BYTES`.

Each instance of this binary serves one plugin only; nothing here is global or
a singleton (no process state, no fixed-name socket), so
`crate::plugins::PluginManager` (in `crates/node`) can already keep one
concurrent instance per active `plugin_code`, each with its own
`POM_PLUGIN_DB` and its own stdin/stdout pipes. The second argument of the
process is the `plugin_code` validated in the manifest; it resolves the symbol
`pom_<plugin_code>_plugin_v1`, without coupling the host to a specific product.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.