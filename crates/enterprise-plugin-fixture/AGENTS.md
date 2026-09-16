# enterprise-plugin-fixture

`cdylib` mínima usada em testes locais do contrato Enterprise. Exporta
`pom_enterprise_plugin_v1` e callbacks no ABI definido por
`enterprise-protocol`; não representa o plugin comercial nem contém auditoria.
O único carregador é `crates/plugin-host`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
