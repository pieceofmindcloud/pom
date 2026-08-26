# Relatório de entrega: provider Hugging Face

Data: 2026-08-24
Branch: `fm/rdma-hf-warnings`
Especificação: `/Users/jairpereira/Sites/firstmate/data/rdma-hf-provider/spec.md`

## 1. Arquitetura implementada

O subsistema novo separa o contrato `ModelProvider` da implementação
`HuggingFaceProvider`. O contrato concentra descoberta, inspeção, licença,
autenticação, download, progresso, manifesto e storage, sem mencionar o CLI.

`hf_cli.rs` isola montagem de argumentos, execução assíncrona com stdout/stderr
separados, cancelamento do processo, parsing JSON, caminho final do download e
classificação de erros. `huggingface.rs` adapta esses resultados ao contrato,
valida entradas e calcula o manifesto local. `LocalModelStorage` reutiliza o
hash SHA-256 já usado por `models/pom.rs`.

O backend usa `check_authentication` em
`GET /api/ui/model-provider/status`, devolvendo o estado estruturado do CLI,
versão, autenticação e usuário sem expor token. O detalhe remoto chama
`get_license` antes de a UI oferecer o download, preserva o marcador `gated` e
mostra a licença detectada ou desconhecida.

O resultado é somente armazenamento, enumeração e hashing. Nenhum conteúdo
baixado é executado, nenhum token é aceito como argumento e nenhuma opção de
execução remota é enviada.

## 2. Arquivos criados

- `crates/node/src/models/provider.rs`
- `crates/node/src/models/hf_cli.rs`
- `crates/node/src/models/huggingface.rs`
- `docs/rdma-hf-provider-reuso.md`
- `docs/rdma-hf-provider-relatorio.md`
- `docs/superpowers/specs/2026-08-24-rdma-hf-provider-design.md`
- `docs/superpowers/plans/2026-08-24-rdma-hf-provider.md`

## 3. Arquivos alterados

- `crates/node/src/main.rs`: declaração dos três módulos novos.
- `crates/node/src/models/AGENTS.md`: responsabilidades e limites dos novos
  módulos.
- `crates/node/AGENTS.md`: mapa do subsistema `models` atualizado.
- `docs/AGENTS.md`: ponteiros para o levantamento e este relatório.

## 4. Decisões técnicas importantes

- O CLI é uma implementação substituível de `ModelProvider`, não uma
  dependência da UI.
- O runner aceita `CommandSpec` com `Vec<String>` e usa `.args(...)`; não há
  shell nem concatenação de entrada do usuário.
- O `hf` gerencia a credencial. O POM apenas consulta `hf auth whoami` e nunca
  chama `hf auth token`, registra token, envia token para telemetria ou inclui
  saída bruta em erro.
- `repo_id`, revisão, arquivo, filtros e caminhos são validados antes de
  formar comandos ou manifestos. Revisões aceitam barras internas para
  branches, mas recusam componentes de traversal.
- Consultas estruturadas preferem `--format json`. O download usa somente o
  caminho final impresso pelo CLI e interpreta progresso textual como dados
  opcionais.
- GGUF reutiliza `catalog::parse_quant`; quantização, tamanho e nome base
  ficam ausentes quando o nome não permite determinação segura. Licença
  desconhecida retorna `detected: false`.
- A telemetria usa `tracing` com provider, repositório, revisão, bytes,
  duração, status e tipo de erro, sem stdout, stderr ou credenciais.
- A rota HTTP e a tela consomem somente `ModelProvider`: status autenticado do
  CLI, busca, detalhes com licença e downloads estruturados. `with_runner` é
  restrito aos testes para injetar o executor fake; o processo real continua
  sendo criado somente pelo runner de produção.

## 5. Comandos para teste manual

Com o checkout completo do `llama.cpp` disponível no caminho esperado:

```bash
hf --version
hf auth whoami
hf models ls --search qwen --format json
hf models info owner/model --format json
hf models ls owner/model --revision main --format json
hf models card owner/model --metadata --format json
hf download owner/model model-Q4_K_M.gguf --revision main --cache-dir ./hf-cache
```

Os comandos de domínio do POM são:

```bash
rtk cargo test -p node hf_cli::tests -- --nocapture
rtk cargo test -p node huggingface::tests -- --nocapture
```

## 6. Testes adicionados

- Validação de `repo_id`, revisão e nome de arquivo contra traversal.
- Detecção de quantização GGUF e ausência de metadado inventado.
- Licença desconhecida com `detected: false`.
- Manifesto local com tamanho e SHA-256.
- Parsing de versão, autenticação, busca, arquivos, licença, caminho final e
  progresso do CLI.
- Classificação de autenticação ausente, repositório inexistente, limite de
  requisições e falha de download.
- Runner fake para CLI ausente, busca, GGUF, sucesso, falha e cancelamento.
- Status estruturado para CLI ausente, sessão sem login e usuário autenticado;
  falha do processo `card` preserva o fallback para `models info`.
- Verificação de argumentos separados e ausência de token em erros/debug.

Resultado verificável nesta execução:

- `rtk cargo build -p node`: passou sem os avisos do provider Hugging Face.
- `rtk cargo test -p node`: 752 passaram, 5 ignorados.
- `rtk cargo fmt --check`: passou.
- `rtk cargo clippy -p node`: código de saída zero; restam avisos preexistentes
  em outros subsistemas, sem avisos nos arquivos alterados.
- `rtk npm test`: 50 arquivos e 291 testes passaram.
- `rtk npm run build`: TypeScript e Vite passaram; o Vite mantém o aviso
  informativo de chunk acima de 500 kB.

## 7. Limitações conhecidas

- O progresso depende das linhas textuais fornecidas pelo CLI; bytes totais,
  arquivo atual e velocidade permanecem opcionais quando não puderem ser
  determinados com segurança.
- O chunk principal da UI ultrapassa 500 kB minificado; o build passa, mas uma
  futura divisão por rotas pode reduzir o carregamento inicial.

## 8. Próximos passos

1. Registrar o `ModelManifest` no catálogo local e conectá-lo ao fluxo de
   fatiamento/implantação, sem distribuir pesos automaticamente nesta fase.
2. Acrescentar fixtures para versões do CLI e testar também Windows/macOS em
   CI quando o crate `node` puder ser compilado nessas plataformas.
