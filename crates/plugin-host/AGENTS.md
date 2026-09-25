# plugin-host

Binário auxiliar `pom-plugin-host`, responsável por carregar a biblioteca
`cdylib` de UM plugin em processo separado e expor o IPC JSON enquadrado do
host. Valida caminho absoluto, manifesto, hash, versão ABI e callbacks antes
de criar o handle. O processo `pom` não deve importar `libloading` nem
carregar a biblioteca diretamente. Além de `ready`, `ingest` e `shutdown`, o
IPC expõe `query` com cópia segura do buffer retornado pela biblioteca e
limite compartilhado de 16 MiB definido por
`enterprise_protocol::MAX_IPC_FRAME_BYTES`.

Cada instância deste binário serve um plugin só; nada aqui é global ou
singleton (nem estado de processo, nem socket com nome fixo), então
`crate::plugins::PluginManager` (em `crates/node`) já pode manter uma
instância concorrente por `plugin_code` ativo, cada uma com seu próprio
`POM_PLUGIN_DB` e seus próprios pipes stdin/stdout. O segundo argumento do
processo é o `plugin_code` validado no manifesto; ele resolve o símbolo
`pom_<plugin_code>_plugin_v1`, sem acoplar o host a um produto específico.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
