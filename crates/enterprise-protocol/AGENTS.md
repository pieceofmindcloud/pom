# enterprise-protocol

Crate compartilhado do contrato cliente de licenciamento de plugins. Define
DTOs JSON do licenciamento e a ABI C v1 usada exclusivamente pelo
`pom-plugin-host`; não carrega biblioteca dinâmica, faz rede ou persiste
licença. O nome do crate ficou como está por história de git - o contrato é
por `plugin_code`, não de um produto único: `ActivationPayload`,
`ActivationResponse` e `GrantBody` carregam `plugin_code`, e uma mesma chave
pode conceder vários. Os DTOs precisam continuar compatíveis com o servidor
em `license.pieceofmind.cloud`, inclusive com envelope, manifests/grants
assinados e estados de lease.
As respostas autenticadas também trazem plugins concedidos, candidatos com
release ativável, expiração e uso de ativações por `plugin_code`; o POM mantém
esses dados no status local sem expor tokens. `PluginManifest.preferences` é
opcional e tipado para preservar respostas antigas; define somente o schema da
interface de preferências locais e nunca é entitlement.

Também publica `MAX_IPC_FRAME_BYTES`, o limite compartilhado dos frames JSON
na comunicação local por stdin/stdout entre `pom` e `pom-plugin-host`.

O aviso de atualização do `pom_core` **não** vive aqui: ele saiu deste crate
com o pivô de plugins e é tipo interno de `crates/node/src/pom_core_update.rs`,
porque o binário `pom` não é um plugin e sua atualização não é parte do
contrato de licença. A referência de arquitetura e os limites restantes estão
em `docs/subsystems/licenciamento-enterprise.md`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
