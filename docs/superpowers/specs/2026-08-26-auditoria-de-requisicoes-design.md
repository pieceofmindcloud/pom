# Auditoria persistente de requisições

## Contexto

O gateway já associa cada requisição de `/v1` a um `Caller`, mas o registro
atual só existe em memória e só é criado depois da geração. Isso impede
auditar pedidos que falham, travam, excedem o orçamento ou perdem o cliente.
Também deixa prompts e detalhes da requisição fora do histórico após um
reinício.

## Decisões

- O registro será criado no início de cada geração, antes do `tokio::spawn` do
  pipeline, e receberá um `request_id` estável. A conclusão fará `UPDATE` do
  mesmo id.
- O armazenamento será SQLite em `<project_root>/audit.sqlite3`, inicializado
  com DDL idempotente no arranque. `rusqlite` com o recurso `bundled` evita
  depender de uma instalação local de SQLite.
- Uma thread dedicada serializa escrita. O gateway usa `try_send` numa fila
  limitada e nunca aguarda I/O de disco; fila cheia, banco indisponível e erro
  de atualização são registrados em tracing e não alteram a resposta da
  inferência.
- A tabela mantém o prompt final exato enviado ao pipeline, o JSON semântico
  do corpo da requisição, o resultado, telemetria, rota, modelo, identidade da
  chave, nó de entrada, hops, horários, tokens, duração, origem e erro.
- O segredo da API key e qualquer trecho derivado dele não serão gravados na
  auditoria. Cada chave terá um `id` opaco persistido na configuração; chaves
  antigas sem id usarão uma impressão estável dos metadados não sensíveis como
  fallback. A UI exibirá e filtrará por id, rótulo e nó.
- A busca por prompt usará uma tabela FTS5 indexada. A busca por identidade,
  nó, estado e horário usará índices B-tree. A resposta será limitada e
  ordenada por `received_at DESC`, com `offset` para evitar carregar o
  histórico inteiro no navegador.
- A rota `GET /api/ui/requests` exigirá `ConnectionAuth`, seguindo o controle
  administrativo existente. Prompts, corpos e telemetria não serão expostos
  sem autenticação.
- Estados: `in_progress` no recebimento; `ok`, `partial`, `error` ou
  `abandoned` na conclusão; `incomplete` quando um processo reinicia com
  pedidos antigos em andamento ou quando o watchdog de consulta encontra um
  pedido sem conclusão por 15 minutos. O erro explica a transição.

## Interface HTTP e UI

`GET /api/ui/requests` aceitará `api_key`, `prompt`, `node_id`, `from`, `to`,
`limit` e `offset`. A resposta manterá os totais e agrupamentos usados pela
tela, além dos campos completos de auditoria. `/logs` terá filtros de chave,
texto e período; cada linha poderá abrir prompt, corpo da requisição, estado,
resultado e telemetria em blocos legíveis. O estado incompleto terá rótulo e
explicação visíveis.

## Testes

O módulo de persistência terá testes com SQLite em memória para criação em
duas etapas, distinção de estados e consultas por identidade e texto. A UI
terá testes de renderização para filtros, prompt expandido e estado
incompleto. O build Rust será comparado com a baseline capturada antes da
edição, pois o worktree tem o erro preexistente do vendor de `llama.cpp`.

## Fora de escopo

- Expurgo, retenção por período, criptografia em repouso e exportação para um
  SIEM.
- Replicação dos registros entre peers. Cada nó audita as requisições que
  recebe.

needs-decision: definir política de retenção, expurgo e eventual exportação
externa antes de operar o banco por prazo indefinido em produção.
