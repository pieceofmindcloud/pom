# Levantamento de reuso para o provider Hugging Face

Data: 2026-08-24

Este levantamento foi feito antes da implementação da integração com o CLI
`hf`, conforme a especificação do captain. A regra de desenho é manter o
Hugging Face atrás de uma abstração de provider: a aplicação conhece
`ModelProvider`, e não o comando `hf`.

## Módulos e serviços reutilizados

- `crates/node/src/models/` será a fronteira do novo subsistema. O catálogo em
  `catalog.rs` já descobre GGUF localmente e expõe `parse_quant`, que será
  reutilizado para quantização sem duplicar o parser. `model.rs` continua
  responsável pelos perfis de inferência; não será acoplado ao provider.
- `models/pom.rs` já define manifesto, hashing SHA-256 (`hash_file`) e a
  identidade de arquivos por conteúdo. O manifesto do download remoto será
  específico de `ModelProvider`, mas usará o mesmo padrão de arquivo completo,
  hash opcional e caminho local verificável.
- `models/storage.rs` já concentra operações de armazenamento e retenção de
  modelos fatiados. O provider não vai duplicar essa política nem misturar o
  cache do Hugging Face com a retenção de shards do POM; uma abstração pequena
  `ModelStorage` ficará preparada para uma política de cache do POM no futuro.

## Execução de processos externos

- `utils/sysmon.rs` usa `spawn_blocking` com `std::process::Command` para
  utilitários curtos e evita instalar o reaper global de processos do Tokio.
- `inference/runtime.rs` usa `tokio::process::Command`, pipes separados e
  cancelamento para processos longos supervisionados.
- `backend/devupdate.rs` usa argumentos separados, `stdin` nulo e captura
  separada de saída para comandos administrativos.

O provider seguirá o padrão de `Command::new(...).args(...)`, nunca shell ou
string concatenada. Para downloads longos usará o padrão assíncrono de
`inference/runtime.rs`, com stdout e stderr separados, observação incremental
de progresso, processo cancelável e runner injetável nos testes.

## Eventos, jobs e progresso

`backend/webui.rs` mantém jobs em `RwLock<HashMap<...>>`, atualiza progresso
monotonicamente em `set_job_progress` e executa trabalho demorado em tarefas
Tokio. O provider não duplicará esse armazenamento global porque a API
solicitada é interna: ele expõe `DownloadProgress` por um canal opcional, para
que a camada superior conecte os eventos ao job existente quando a UI for
integrada. O callback não bloqueia a thread principal.

## Logging e telemetria

O runtime usa `tracing` diretamente (`info!`, `warn!`, `error!`) e não há uma
fachada de métricas ou um exporter separado para downloads. O provider emitirá
eventos `tracing` com `provider`, `repo_id`, `revision`, duração, bytes, status
e tipo de erro. Nenhum stdout, stderr, token, header ou conteúdo privado será
registrado. Mensagens de erro passarão por redaction de padrões de credencial.

## Configuração e credenciais

`utils/config.rs` é a fonte da configuração persistente do nó e `brand.rs`
deriva nomes de produto e variáveis. Esta primeira versão não adicionará token
do Hugging Face ao `pom.json`: o próprio `hf auth login` gerencia a credencial.
O provider apenas consulta `hf auth whoami` e retorna estado estruturado.

## Tratamento de erros

O código atual usa `anyhow::Result` para operações internas, mas validações de
contrato e a fronteira do provider precisam ser discrimináveis pela UI. O
novo módulo terá um `HfError` tipado para CLI ausente, autenticação ausente,
repositório inexistente, acesso negado, limite de requisições, resposta
inválida, falha de download, cancelamento e entrada inválida. A mensagem
armazenada em cada variante será sanitizada e nunca carregará saída bruta do
processo.

## Testabilidade e compatibilidade

Não existe provider equivalente no projeto, então não haverá adaptação de
`OllamaClient`: Ollama continua sendo a integração HTTP usada na resolução de
perfil e saúde. O runner do processo externo será um trait injetável, e todos
os testes usarão um mock em memória. A implementação usará APIs portáveis de
filesystem e processos, sem assumir caminhos Unix, e não executará nenhum
arquivo baixado.

## Referência externa conferida

A documentação atual do Hugging Face confirma os comandos `hf models ls`,
`hf models info`, `hf models card --metadata --format json`, `hf auth whoami` e
`hf download` com `--revision`, `--cache-dir`, `--local-dir`, `--include` e
`--exclude`: [CLI do Hugging Face](https://huggingface.co/docs/huggingface_hub/en/guides/cli).
O parser ficará isolado porque a saída de progresso continua textual, enquanto
as operações de descoberta preferirão JSON.

## Limites deliberados

- Não haverá rota HTTP nem tela nova nesta etapa, para não reescrever
  `backend/webui.rs` durante trabalho paralelo. A camada superior poderá
  injetar o provider e consumir sua API interna sem executar `hf` diretamente.
- Não haverá distribuição P2P do modelo. O resultado produz um manifesto que
  poderá alimentar o catálogo e a implantação futura.
- Não haverá instalação automática do CLI e nenhuma opção equivalente a
  `trust_remote_code` será enviada.
