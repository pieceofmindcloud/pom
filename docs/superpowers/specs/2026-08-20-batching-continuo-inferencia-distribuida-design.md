# Batching contínuo de inferência distribuída

Data: 2026-08-20

## Objetivo

Permitir que várias requisições de inferência avancem simultaneamente no mesmo
modelo carregado, com uma fila limitada, lotes curtos e associação correta
entre cada sequência, seu KV cache, seus estados ocultos e sua resposta.

O resultado precisa funcionar nos dois pontos em que o POM distribui trabalho:

- no pipeline, onde estados ocultos passam por vários peers;
- no Tensor Parallelism, onde um lote idêntico precisa ser executado por todos
  os ranks e reduzido em conjunto.

O recurso será implementado dentro do processo Rust que já carrega o
`llama_context`. Não haverá um `llama-server` externo nem uma segunda pilha de
inferência.

## Contexto verificado

O servidor atual expõe `/v1/prefill` e `/v1/step` e chama diretamente
`llama_decode`. Os handlers usam `spawn_blocking` e protegem o
`InferState`/`LlmEngine` com mutex. Isso impede dois `llama_decode` simultâneos,
mas não é uma fila FIFO explícita e não forma um `llama_batch` com várias
sequências.

Hoje o motor configura `n_seq_max` a partir de `max_sessions`, mas cada chamada
de `decode_tokens` ou `decode_hidden` inicializa um lote para uma única
sequência. Quando todos os slots estão ocupados, uma nova sessão pode falhar
com `seq pool esgotado (max 1 sessões simultâneas)` em vez de aguardar.

O protocolo binário atual também carrega um único tensor por frame:

```text
[4 bytes BE: tamanho do JSON][JSON][payload f16 ou f32]
```

O Tensor Parallelism atual identifica um decode individual e espera barreiras
globais para cada redução. Portanto, somente aumentar `max_sessions` ou colocar
mais chamadas atrás do mutex não resolve o problema: o transporte, a captura
de residual e a coletiva também precisam distinguir os itens do lote.

## Decisões de arquitetura

### 1. Um scheduler é o dono do contexto

Cada servidor de inferência terá um `InferenceScheduler` associado a um modelo
carregado. O scheduler será o único dono de:

- `LlmEngine` e do `llama_context`;
- mapa de sessões e seus `seq_id` físicos;
- pool de sequências;
- fila de prefill, fila de decode e comandos de controle;
- estado de captura dos residuais e resultados de logits.

Os handlers HTTP apenas decodificam o frame, validam a requisição, registram
um comando em um canal limitado e aguardam uma resposta `oneshot`. Nenhum
handler manterá um mutex do motor durante uma requisição inteira.

O loop do scheduler rodará em um worker dedicado, porque as chamadas ao
llama.cpp e o callback de redução TP são síncronos e o contexto não é seguro
para uso concorrente. O worker pode processar apenas um lote por vez, mas cada
lote poderá conter várias sequências.

Essa separação torna explícita a espera que hoje aparece como `lock_wait_ms` e
evita que o número de conexões HTTP determine quantas operações tentam usar o
mesmo contexto.

### 2. Admissão limitada e justiça

O scheduler terá limites independentes para admissão e execução:

- `max_queue_requests`: máximo de requisições aguardando, com valor inicial
  recomendado de 128;
- `batch_window_ms`: janela curta para reunir chegadas próximas, com valor
  inicial recomendado de 4 ms;
- `max_batch_sequences`: no máximo `max_sessions` sequências ativas no
  contexto;
- `max_batch_tokens`: teto de tokens em um lote, limitado também por
  `n_batch`.

Os valores serão parte da configuração efetiva do nó e aparecerão na
telemetria. A implementação não criará um flag cosmético chamado
`--cont-batching`: o comportamento pertence ao scheduler que já controla o
contexto Rust.

Prefill e decode terão filas lógicas separadas:

- prefill agrupa prompts compatíveis até o teto de tokens e pode preencher
  várias sessões em uma chamada;
- decode escolhe no máximo uma rodada por sessão por ciclo, em ordem round
  robin, para que uma sessão que gera muitos tokens não impeça as demais;
- comandos de cancelamento, rollback e release têm prioridade sobre uma nova
  rodada de decode e liberam o slot da sessão de forma determinística.

Uma requisição que exceder a fila recebe erro HTTP de sobrecarga com
`Retry-After`, sem criar uma sessão parcial. Uma requisição cancelada antes de
entrar no lote é removida da fila; uma cancelada depois da admissão termina no
limite seguro do ciclo atual e libera o KV antes de aceitar trabalho novo.

### 3. Lote multi-sequência no llama.cpp

O `LlmEngine` ganhará uma operação de lote que constrói um único
`llama_batch` com entradas de várias sessões. Cada entrada carregará:

- token ou embedding;
- posição absoluta no KV daquela sessão;
- um `seq_id` físico exclusivo;
- indicação de quais posições devem produzir logits;
- referência lógica ao item que solicitou a saída.

O scheduler manterá a relação entre `session_id` lógico e `seq_id` físico. A
ordem das entradas no batch não será usada como identidade, porque prefill e
decode podem ser agrupados em ordens diferentes.

Depois de `llama_decode`, logits, residuais e métricas serão separados por
posição de saída e devolvidos ao `oneshot` correto. A captura de residual que
hoje tem um único slot global passará a registrar uma linha por item do lote.
Se um peer intermediário precisar exportar estado oculto, o tensor de saída
será particionado por item antes de ser enviado ao próximo hop.

O scheduler não misturará sequências de modelos, deployments ou configurações
de contexto incompatíveis no mesmo lote. Cada lote terá um `batch_id` interno,
uma lista estável de itens e uma política de falha: se o decode falhar, todos
os itens daquele lote recebem o mesmo erro e nenhum resultado parcial é
publicado como resposta válida.

### 4. Protocolo binário inicial para vários itens

O produto ainda não foi lançado. Portanto `frame/v1-batch` é o primeiro e
único contrato oficial, sem fallback, adapter ou compatibilidade com formatos
v1/v2 anteriores.

O novo frame terá um cabeçalho JSON e um payload concatenado. O cabeçalho
descreverá, para cada item:

- `item_id` e `session_id` lógico;
- fase `prefill` ou `step`;
- tokens, posição e flags de logits quando aplicável;
- shape, dtype, offset e comprimento do tensor no payload;
- `batch_id`, `chunk_index` e dados necessários para correlacionar a resposta.

O decoder validará todos os offsets, comprimentos, shapes, número de itens e
limites antes de alocar ou interpretar o payload. Um frame inválido será
recusado sem tocar no `InferState`. O limite atual de 16 MiB continua valendo
por frame; o scheduler limitará o número de itens para que o lote não seja uma
forma de contornar esse limite.

O anúncio de capacidades incluirá `inference/frame-v1-batch` somente quando o
servidor local estiver pronto. Uma requisição de um item usa o mesmo frame
oficial e não existe interpretação alternativa para um peer sem a capability:
ele não é elegível para inferência.

### 5. Pipeline distribuído

O gateway continuará sendo responsável por planejar a rota. No caminho de
batching, ele agrupará chamadas compatíveis apenas quando todas as requisições:

- usarem o mesmo modelo e deployment;
- tiverem a mesma configuração de amostragem e contexto relevante;
- puderem seguir o mesmo próximo hop;
- suportarem a mesma fase do protocolo.

Cada hop encaminhará um frame com os itens que realmente passam por ele. Um
item que não puder prosseguir não será removido silenciosamente: receberá uma
falha identificada pelo próprio `item_id`, enquanto os outros itens só
continuarão se a semântica do estágio permitir isolamento seguro. Se a
execução de um lote comprometer o contexto compartilhado, o lote inteiro
falhará e todas as sessões serão limpas.

No retorno, o relay carregará resultados e `hop_timings` por item. Streaming
continuará emitindo tokens na ordem de cada requisição, nunca na ordem
arbitrária de conclusão do lote. O scheduler poderá concluir uma sessão sem
esperar que outra gere todos os seus tokens.

### 6. Tensor Parallelism alinhado por lote

O primário criará um `batch_id` e transmitirá aos workers TP o mesmo conjunto
de itens, na mesma ordem lógica, com os mesmos tokens, posições e flags. Todos
os ranks deverão executar o mesmo lote antes de qualquer rank avançar para o
próximo.

As barreiras de redução deixarão de ser identificadas apenas por um decode ou
por uma sessão. A chave incluirá, no mínimo:

```text
(grupo TP, batch_id, camada, operação de redução, índice da redução)
```

O callback do llama.cpp continuará reduzindo o buffer completo fornecido pelo
grafo. A camada TP não poderá tratar uma redução de um item como se fosse a de
outro. O primário fará a amostragem e distribuirá os resultados por
`item_id`; workers não produzirão respostas independentes.

Se um rank perder uma barreira, enviar um lote com shape diferente ou falhar
na credencial, o batch TP inteiro falhará. Não será permitido devolver uma
resposta produzida com uma parcial de apenas alguns ranks. Release, rollback e
cancelamento serão replicados por item para que nenhum worker conserve um KV
que o primário já liberou.

O modo especulativo/MTP terá uma lane própria. Ele só poderá participar de um
lote quando todos os itens tiverem a mesma capacidade e o mesmo contrato de
rollback. Caso contrário, os itens serão executados na lane normal de decode;
não haverá desativação silenciosa de MTP no meio de uma sessão.

### 7. Capacidades, configuração e atualização gradual

O nó anunciará os limites que realmente suporta, além da capacidade do frame:

- batching contínuo habilitado;
- máximo de sequências por lote;
- máximo de tokens por lote;
- suporte a prefill em lote;
- suporte a decode em lote;
- suporte a TP alinhado em lote.

O planejador só escolherá batching distribuído quando a rota inteira tiver as
capacidades necessárias. Uma rota parcialmente atualizada continuará
funcionando com frames antigos de item único. A configuração local poderá
desligar batching para diagnóstico, mas não poderá forçar um peer incompatível
a receber o protocolo novo.

### 8. Telemetria e diagnóstico

Os tempos atuais continuarão disponíveis. Serão acrescentados:

- `scheduler_wait_ms` e `scheduler_queue_depth`;
- `batch_id`, `batch_size` e `batch_tokens`;
- `active_sequences` e `max_active_sequences`;
- tempo de montagem do lote;
- tempo de decode do lote;
- contagem de itens concluídos, cancelados e falhos.

No pipeline e no TP, os dados serão associados ao `item_id` e ao `batch_id`.
Isso permitirá distinguir espera de fila local, espera de rede e espera de
barreira TP, em vez de somar tudo em `lock_wait_ms`.

## Invariantes

1. Um `seq_id` físico pertence a uma única sessão enquanto estiver ativo.
2. Um tensor, logit, residual, token e erro nunca pode ser entregue a outro
   `item_id`.
3. Nenhum `llama_decode` concorrente usa o mesmo contexto.
4. Todos os ranks TP executam o mesmo lote, na mesma ordem e com os mesmos
   shapes antes de uma redução.
5. Uma redução TP incompleta nunca produz resposta válida.
6. Todo peer elegível para inferência anuncia e recebe `frame/v1-batch`.
7. O limite de fila é aplicado antes de alocar uma sessão ou um KV slot.
8. Release, rollback, timeout e cancelamento liberam todos os slots locais e
   remotos associados ao item.
9. Streaming mantém a ordem por requisição mesmo quando o lote conclui itens
   em momentos diferentes.
10. Nenhum peso GGUF é transferido para habilitar batching.

## Plano de validação

Antes de ativar o caminho distribuído, a implementação terá testes para:

- construção de `llama_batch` com várias sequências, posições e flags de
  logits;
- mapeamento de logits e residuais de volta aos itens corretos;
- codificação e decodificação `frame/v1-batch`, incluindo offsets inválidos,
  payload truncado, shapes incompatíveis e limite de tamanho;
- FIFO, janela de microbatch, round robin, cancelamento e backpressure;
- duas requisições em um pipeline de dois ou três hops, com streaming isolado;
- recusa de peer que não anuncia o frame inicial;
- dois ranks TP com batch alinhado, barreira fora de ordem e perda de um rank;
- release e rollback sem vazamento de sequência em todos os ranks;
- mistura proposital de prompts e sessões para provar que não há vazamento de
  token, KV, residual ou texto entre requisições.

Os testes distribuídos usarão o `gateway::testkit`, sem UDP real, e os testes
de wire não carregarão um modelo. O teste E2E principal deverá exercer a mesma
rota HTTP que uma chamada OpenAI usa, para validar fila, pipeline, streaming e
erro de sobrecarga juntos.

## Alternativas consideradas

### Usar `llama-server --cont-batching`

Não atende ao desenho atual. O POM precisa capturar residual no meio do
modelo, encaminhar hidden states entre peers e coordenar reduções TP. Um
servidor externo não teria acesso seguro a esses pontos e criaria uma segunda
implementação de sessão e autenticação.

### Apenas colocar requisições em uma fila FIFO

Resolve o erro de `seq pool esgotado`, mas continua executando um item por
`llama_decode`. É uma melhoria de backpressure, não continuous batching, e não
aproveita o throughput de GPU que motivou a mudança.

### Recomendação

Adotar scheduler local com batch multi-sequência, frame versionado e TP
alinhado. É a única alternativa que preserva o protocolo distribuído do POM e
entrega o comportamento solicitado sem introduzir um motor concorrente
paralelo ao runtime existente.

## Fora de escopo

- Transferência de pesos ou re-sharding do GGUF em runtime.
- Data parallelism entre modelos ou entre processos.
- Alteração do fork `vendor/llama-cpp-sys-2/llama.cpp` para criar um novo
  backend de rede; o batching usa as APIs de batch já disponíveis no contexto.
- Garantir batching em uma rota que não anuncie a capacidade inicial.
- Reordenar tokens entre requisições para formar um único contexto lógico.
