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

Everything a plugin starts ends with its host (`src/tree.rs`). On Linux the
host is a child subreaper, so an orphan anywhere below it (a daemon that
called `setsid` and double-forked, an agent pane, a headless browser) is
reparented to the host instead of init. When the host leaves, after the
plugin's `shutdown` (or a failed load), it sends SIGTERM to every live
descendant, waits 2 s, SIGKILLs the rest and reaps them; macOS runs the same
walk without a subreaper, so a process that detached there escapes. POM waits
10 s for that exit (`SHUTDOWN_TIMEOUT` in `crates/node/src/plugin_host.rs`).
While the host runs, an orphan that exits stays a zombie under it until the
host leaves: a reaper during the run would steal the exit status of the
plugin's own children. Windows uses POM's kill-on-close Job Object instead.
Only the plugin mode does any of this.

`pom-plugin-host --apply-update <plan.json>` is a second, unrelated mode
(`src/update.rs`): the node's POM update (`crates/node/src/pom_core_update.rs`)
stages a release, writes the plan (the node's pid, each staged entry and what
it replaces, a backup folder, the program and arguments to start, the log) and
starts a copy of this binary from the staging folder, detached. The host waits
for the node to exit, swaps every entry with a backup, puts everything back on
the first failed move, and starts POM again either way. It loads no plugin.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.