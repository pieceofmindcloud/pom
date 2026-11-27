# enterprise-plugin-fixture

Minimal `cdylib` used in local tests of the plugin contract. Exports
`pom_logs_plugin_v1` and `pom_base_plugin_v1`, plus the callbacks in the ABI
defined by `enterprise-protocol`, and `pom_probe_plugin_v1`, whose `query`
tries the reads, writes and signal a request names and answers what worked
(the sandbox tests of `crates/plugin-host`); it does not represent commercial plugins and
contains no audit. The only loader is `crates/plugin-host`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.