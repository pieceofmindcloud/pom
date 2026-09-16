# enterprise-plugin

Biblioteca privada `cdylib` carregada somente pelo `pom-plugin-host`. Recebe
eventos JSON pela ABI C v1, mantém o domínio de auditoria fora do `pom.db` e
persiste em `enterprise.db`. A API do plugin não recebe chaves, tokens,
`GatewayState` ou ponteiros Rust do processo `pom`.

A listagem e as estatísticas devem conservar o contrato de resposta usado por
`crates/node/src/backend/reqlog.rs`. Mudanças no schema ou nos eventos exigem
teste de persistência e atualização de
`docs/subsystems/licenciamento-enterprise.md`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file, command, or doc instead.
Prefer rewriting and pruning over appending.
