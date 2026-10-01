# AGENTS.md - .github

Os workflows em `workflows/` publicam os pacotes de Linux, macOS e Windows.
O build nativo passa pela action composta `actions/llama-cache`, que resolve o
SHA atual da branch `pom-tensor-parallel`, restaura o checkout por SHA e valida
o clone antes de o Cargo usá-lo. A estratégia completa, incluindo `sccache`,
vive em `docs/subsystems/build-and-run.md`, seção "Cache in GitHub Actions".

Ao alterar a estratégia de cache, mantenha a invalidação separada por sistema,
arquitetura e SHA do fork. Alterações no lockfile devem continuar compondo a
chave do cache Cargo no workflow Windows. Não dispare uma release para testar
alterações de conveniência sem autorização explícita do operador.

## Maintaining this file

Mantenha este arquivo como um mapa curto da automação de CI. O YAML e a
documentação de build são a fonte de verdade; não replique aqui comandos ou
detalhes que já estão nesses arquivos.
