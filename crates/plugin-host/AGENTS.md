# plugin-host

Binário auxiliar `pom-plugin-host`, responsável por carregar uma biblioteca
Enterprise em processo separado e expor o IPC JSON enquadrado do host. Valida
caminho absoluto, manifesto, hash, versão ABI e callbacks antes de criar o
handle. O processo `pom` não deve importar `libloading` nem carregar a
biblioteca diretamente. Além de `ready`, `ingest` e `shutdown`, o IPC expõe
`query` com cópia segura do buffer retornado pela biblioteca e limite de 1 MiB.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
