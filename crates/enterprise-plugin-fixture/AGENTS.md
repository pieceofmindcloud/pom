# enterprise-plugin-fixture

`cdylib` mínima usada em testes locais do contrato de plugins. Exporta
`pom_logs_plugin_v1` e `pom_base_plugin_v1`, além dos callbacks no ABI
definido por `enterprise-protocol`; não representa plugins comerciais nem
contém auditoria. O único carregador é `crates/plugin-host`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
