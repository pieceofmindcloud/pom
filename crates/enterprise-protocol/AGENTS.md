# enterprise-protocol

Crate compartilhado do contrato cliente Enterprise. Define DTOs JSON do
licenciamento e a ABI C v1 usada exclusivamente pelo `pom-plugin-host`; não
carrega biblioteca dinâmica, faz rede ou persiste licença. Os DTOs precisam
continuar compatíveis com o servidor em `license.pieceofmind.cloud`, inclusive
com envelope, manifests/grants assinados, aviso de atualização do `pom-core`
e estados de lease. A referência de
arquitetura e os limites restantes estão em
`docs/subsystems/licenciamento-enterprise.md`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
