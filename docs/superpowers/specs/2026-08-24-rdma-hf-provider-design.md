# Integração Hugging Face por ModelProvider

Data: 2026-08-24

## Contexto

O POM já descobre GGUF localmente, calcula quantização, calcula SHA-256 e
mantém manifestos das fatias. Ainda não existe uma fonte remota de modelos
atrás de uma abstração própria. A primeira fonte será o executável `hf`, mas o
restante do POM não deve depender da sintaxe do CLI, de texto de terminal ou de
credenciais do Hugging Face.

A referência operacional do CLI é a documentação oficial do Hugging Face:
`hf models ls`, `hf models info`, `hf models card --metadata --format json`,
`hf auth whoami` e `hf download` com revisão, cache, diretório local e filtros.

## Abordagens consideradas

1. Colocar chamadas `hf` nos handlers de `webui.rs`. É curto, mas vaza o CLI
   para a UI, duplica jobs e torna a troca por HTTP direta cara. Rejeitada.
2. Criar uma biblioteca do Hugging Face no módulo de catálogo. Evita processos
   no handler, mas mistura inventário local e remoto e cria uma dependência
   conceitual com a origem. Rejeitada.
3. Criar `ModelProvider`, com `HuggingFaceProvider` como implementação, um
   runner de processo injetável e um storage de manifesto independente.
   Recomendação adotada: preserva a fronteira para HTTP/POM Registry futuros,
   permite testes sem rede e reutiliza catálogo, hashing, jobs e tracing já
   existentes.

## Arquitetura

### Contrato agnóstico

`models/provider.rs` define:

- `ModelProvider`, com `search_models`, `get_model_info`, `list_files`,
  `download_file`, `download_model`, `get_license` e
  `check_authentication`;
- tipos de descoberta (`ModelSummary`, `ModelInfo`, `ModelFiles`), GGUF
  (`GgufFile`), licença (`ModelLicense`), estado do CLI (`HfCliStatus`),
  opções e resultado de download;
- `DownloadProgress` e um canal opcional de eventos;
- `ModelManifest` e `ModelStorage`, que não mencionam comandos Hugging Face;
- `HfError`, com variantes discrimináveis e mensagens sanitizadas.

`ModelProvider` é `async`, `Send` e `Sync`. Download aceita
`CancellationToken`, não bloqueia o executor Tokio e pode entregar progresso
incremental. O provider não recebe nem devolve token do Hugging Face.

### CLI isolado

`models/hf_cli.rs` define `CommandSpec`, `ProcessOutput`, `HfCommandRunner` e
o runner real. O runner usa `tokio::process::Command`, `stdin` nulo, stdout e
stderr em pipes separados e `.args(...)`. O parser converte JSON de consultas,
linhas de caminho de download, versões, autenticação e mensagens de erro. Toda
interpretação de texto fica nesse módulo.

O runner injeta um observador de chunks para que o provider possa emitir
progresso sem esperar o fim do processo. O cancelamento mata o filho e espera
seu encerramento. Um runner fake usado pelos testes registra `CommandSpec` e
devolve `ProcessOutput`, portanto não há rede nem shell nos testes.

### HuggingFaceProvider

`models/huggingface.rs` monta os comandos atuais do CLI:

- busca: `hf models ls --search QUERY --format json`;
- informação: `hf models info REPO --format json`;
- arquivos: `hf models ls REPO --revision REV --format json`;
- licença: `hf models card REPO --metadata --format json`, com os metadados
  de `info` como fallback;
- autenticação: `hf --version` e `hf auth whoami --format json`;
- download: `hf download REPO [FILENAMES] --revision REV --cache-dir DIR
  --local-dir DIR --include PATTERN --exclude PATTERN`.

Todos os valores vindos do chamador passam por validação antes de virar
argumento. `repo_id` precisa ter dois componentes seguros; `revision` aceita
`/` interno para branches, mas não componentes vazios, `.` ou `..`; arquivos
aceitam subdiretórios POSIX, mas não caminhos absolutos, barras invertidas,
componentes de escape, NUL ou volume Windows. Filtros e diretórios rejeitam
NUL. O provider nunca envia `--token`, `--trust-remote-code` ou equivalente.

O status do CLI ausente é um estado normal (`installed: false`) e não uma
exceção. Falhas de operação são mapeadas para `HfError` por padrões seguros da
saída sanitizada: autenticação ausente, repositório inexistente, acesso
negado, limite de requisições, resposta inválida e falha de download.

### GGUF, manifesto e storage

Arquivos terminados em `.gguf` recebem `GgufFile`; a quantização reutiliza
`catalog::parse_quant`, e o nome base e tamanho de parâmetros só são
preenchidos quando o nome permite inferi-los. Caso contrário ficam `None`.

Depois de um download, o caminho final retornado na última linha não é
executado. O provider só enumera arquivos, lê metadados e calcula SHA-256 por
`pom::hash_file` em worker bloqueante. O `ModelStorage` cria um
`ModelManifest` com `source`, `repo_id`, revisão e arquivos com nome, caminho,
tamanho e hash opcional. Nenhuma cópia adicional é feita: cache e
`--local-dir` permanecem sob controle do CLI.

### Telemetria e progresso

Downloads registram em `tracing` os campos `provider`, `repo_id`, `revision`,
`download_duration_ms`, `download_bytes`, `status` e `error_type`. A saída
bruta do processo nunca entra no evento. `DownloadProgress` expõe status,
arquivo, bytes, total, percentual e velocidade como opcionais, porque o CLI
nem sempre fornece todos os valores.

## Erros e segurança

O caminho de erro sempre usa o enum `HfError`; detalhes de stdout e stderr são
reduzidos e passam por redaction de tokens, `Authorization` e `HF_TOKEN`.
Repositório, revisão, nome de arquivo e caminho retornado são validados antes
de uso. Um conteúdo baixado só pode ser enumerado e hasheado. O POM não
instala `hf`, não aceita credencial própria e não executa scripts ou código
remoto.

## Testes

Os testes de `hf_cli.rs` cobrem montagem com argumentos separados, parsing JSON,
redaction e classificação de erros. Os testes de `provider.rs` cobrem
validação, quantização, manifesto e `ModelStorage`. Os testes de
`huggingface.rs` usam um runner fake e cobrem CLI ausente, autenticação,
repositório inexistente, GGUF, download, falha, cancelamento, path traversal e
ausência do token em erros e eventos. Nenhum teste acessa o Hugging Face real.

## Fora de escopo

- instalação automática ou login interativo do `hf`;
- rota HTTP ou tela de UI para esta primeira etapa;
- distribuição P2P do modelo;
- execução de conteúdo remoto;
- redistribuição do pacote `huggingface_hub` ou do binário `hf`.
