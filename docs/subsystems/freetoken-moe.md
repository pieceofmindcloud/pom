# Goal FreeToken e caminho MoE

Este documento acompanha o goal `feat/ftk`. Ele registra as medições, decisões
e limites do caminho MoE novo sem alterar o contrato do caminho denso, que
continua usando o `llama.cpp`.

## Fonte e escopo

A fonte primária do estudo é
`/Users/jairpereira/Sites/firstmate/data/rdma-freetoken-estudo/report.md`, lido
integralmente antes do trabalho. O FreeToken foi usado como fonte de desenho e
algoritmo, não de código. Nenhum peso trafega na rede por causa do fatiamento.

O goal será entregue em fases coesas:

1. medições locais e documentação do baseline;
2. orçamento de KV em bytes, rebuild de contexto sem reler pesos, cancelamento
   de prefill e stop strings;
3. detecção de MoE com bifurcação reversível;
4. motor MoE próprio com um formato de quantização por vez e cache LRU global;
5. co-execução CPU/GPU baseada em banda medida, ainda pendente de um executor
   GPU concreto.

### Fase 1: S9 entregue

O gateway agora aceita `stop` no formato OpenAI e `stop_sequences` no formato
Anthropic, além do campo equivalente em `/api/generate`. Todos passam por um
`StopMatcher` incremental comum. Ele só libera texto quando o próximo sufixo
não pode mais completar nenhuma parada; ao casar, descarta a parada e o texto
posterior, encerra a geração e propaga `finish_reason=stop` mais
`stop_sequence`. O protocolo entre peers não mudou.

Os testes unitários cobrem parada dividida entre eventos, prefixo pendente,
UTF-8, duplicatas e múltiplas paradas. A suíte nativa foi executada depois de
materializar localmente o `llama.cpp` exigido pelo build script; os números
consolidados ficam na seção de validação ao fim deste documento.

Modelos densos devem continuar no caminho existente. A seleção do caminho MoE
será uma decisão explícita baseada em metadados GGUF, com teste que proíbe um
modelo denso de entrar no motor novo.

### Fase 2: S1, cache de prefixo entre turnos

Mesmo sem o baseline de desempenho, conforme a decisão registrada pelo captain,
o cache de prefixo foi implementado por segurança de estado e reaproveitamento
de trabalho. `PrefixCache` é global ao gateway, com capacidade nominal de 32
entradas e LRU entre entradas ociosas. Leases ocupados não são despejados, então
o total pode exceder a capacidade até uma geração terminar. A política não
guarda pesos nem tensors: cada entrada guarda somente a chave do modelo e dos
hops, o texto do prompt, o
`session_id` e a quantidade de tokens confirmada pela cabeça. Um lease marca a
sessão como ocupada para impedir que duas gerações usem o mesmo KV.

Em um miss, uma entrada ociosa pode ser reciclada para que o cache não exija
um segundo slot de KV quando `max_sessions=1`; o prefill novo reseta essa
sequência antes de decodificar. Entradas ocupadas nunca são compartilhadas.

Ao terminar o prefill com sucesso, o gateway envia `/v1/session/rewind` a todos
os peers do plano. Cada peer remove do KV o sufixo gerado e confirma o
`n_past` resultante. Só depois de todas as confirmações o lease entra no cache;
qualquer falha descarta a entrada e libera a sessão. No turno seguinte, um hit
mantém a rota e a sessão, sinaliza `reuse_prefix` no primeiro chunk e a cabeça
tokeniza o novo prompt. Ela calcula o maior prefixo de tokens comum, recua o KV
para essa posição e propaga `start_pos`/`next_pos` nos chunks seguintes. Os
peers que não tokenizam recebem apenas o estado oculto, mas executam na posição
explícita, evitando buracos silenciosos entre estágios.

Os campos são metadados adicionais do frame de inferência existente; o envelope
binário e as APIs públicas não mudaram. O autoteste diferencial de
`prefix_cache_lab.rs` executa operações contra uma referência simples e exige
que um proxy que altera `cached_tokens` seja detectado. O teste puro do cache
também cobre maior prefixo, leases ocupados e eviction LRU. A suíte nativa foi
executada com o symlink local documentado em `build-execucao.md`. Nenhum ganho
de desempenho é afirmado; os números ficam para a medição posterior pelo
captain.

### Fase 3: detecção de MoE e bifurcação reversível

`gguf::read_info` agora lê `<arquitetura>.expert_count` e
`<arquitetura>.expert_used_count`. A classificação é positiva somente quando
`expert_count > 1`; nome de arquivo, falha de carga e nomes de arquitetura sem
essa evidência não selecionam MoE. Em `auto`, um cabeçalho denso ou ausente
segue explicitamente para `dense/llama.cpp`.

O override `POM_INFERENCE_MODE` aceita `auto`, `dense` ou `moe`. `dense` força o
caminho existente mesmo para um GGUF MoE. `moe` recusa um modelo sem
`expert_count > 1`, em vez de desviar silenciosamente. O caminho MoE desta
fase foi inicialmente `moe/llama.cpp-delegated`, uma delegação temporária ao
`LlmEngine`. A costura posterior da Fase 4 substituiu essa seleção operacional
por `moe/q4_0`; a referência à delegação permanece somente para explicar o
estado histórico e o teste de telemetria que garante que uma delegação nunca
seja rotulada como motor MoE. Os testes de classificação cobrem denso, MoE,
override e metadados ausentes.
Os testes isolados de GGUF e executor passaram em um harness `rustc` com 15
casos. A suíte `cargo test -p node` também passou com o symlink local do
`llama.cpp` materializado durante a validação.

### Fase 4: cache global, loader GGUF e primeiro bloco MoE

O desenho foi reimplementado em Rust a partir do FreeToken fixado em
`bd372b630a028e3faa51f4ab0ef6a98c2f2de501`. A instância `ExpertCache` é
compartilhada pelo servidor e indexa cada entrada por modelo, camada, expert e
formato. Um `ExpertLease` conta referências ativas; eviction só escolhe a
entrada ociosa com menor `last_used`, portanto uma requisição em andamento não
perde seu peso.

`admission::MemoryAccount` não usa uma constante de VRAM. Ele desconta do
baseline livre medido os pesos, buffers fixos e a reserva de ativações, e cruza
esse teto com uma nova leitura de memória livre antes de cada miss. A sonda
local usa `MemAvailable` no Linux, `vm_stat` no macOS e
`GlobalMemoryStatusEx` no Windows. Uma futura sonda de VRAM pode implementar a
mesma trait sem mudar a política.

O primeiro formato é Q4_0 packed em CPU. `format_q4_0.rs` valida a geometria
GGML, mmapa o GGUF, copia somente os dois bancos do expert solicitado para o
cache, decodifica os blocos de 32 elementos, executa gate-up, SiLU gated e
down. `router.rs` escolhe top-k de forma determinística e renormaliza os pesos;
`block.rs` conecta essa seleção ao `Q4_0Executor`, usando uma chave global por
modelo, camada, expert e formato. Nenhuma linha do FreeToken foi copiada.

O harness isolado passou com 28 testes, incluindo parser GGUF sintético,
router F32, forward do bloco, cache hit, eviction global, memória medida,
leases ativos e rejeições de geometria. Isso prova as fronteiras puras do
motor, não um modelo completo. O parser também aceita arrays GGUF grandes,
limitando-os pelo espaço restante do arquivo em vez de um teto arbitrário.
O runtime HTTP agora abre o banco Q4_0 local,
calcula os logits do router e aplica o bloco ao hidden state recebido ou ao
residual que será encaminhado. O frame continua em f16 e não leva pesos.
Ainda delegam ao `LlmEngine` a atenção, as operações que não pertencem ao
bloco costurado e a amostragem final do token; portanto esta entrega não
afirma ser um transformer MoE completo. O captain assumiu a prova ponta a
ponta com um modelo real e fará essa validação fora deste ambiente, sem
alterar o caminho denso.

Não há número de desempenho nesta fase. A validação nativa foi executada com o
symlink local de `llama.cpp` documentado em `build-execucao.md`; esse artefato
não entra no commit e não substitui a prova ponta a ponta com um modelo real.

### Ligação e telemetria do hop

`infer_server` mantém o caminho denso em `llama.cpp`. Quando a seleção GGUF é
MoE, `MoeRuntime` é aberto somente para esse caminho e o router local escolhe
os experts Q4_0. No prefill e no decode, um hidden state recebido é transformado
antes do decode local; quando o pedido entrou como tokens, o residual capturado
é transformado antes de seguir para o próximo hop. Os caminhos unitário e batch
passam pela mesma ponte. O wire permanece o estado oculto f16 já existente,
sem peso na rede. O limite atual é explícito: atenção, operações restantes e
sampling final continuam no `LlmEngine`.

Cada resposta de inferência inclui `node_timings.engine`, produzido pelo estado
do backend, e `/health` publica o mesmo `execution_engine`. O gateway agrega o
campo por fase de cada hop, sem deduzi-lo do nome do modelo, da arquitetura ou
de tempos. A interface mostra `llama.cpp`, `motor MoE` ou `desconhecido`; um
peer antigo, uma entrada sem campo ou chamadas divergentes no mesmo hop ficam
em `desconhecido`. Uma delegação explícita `moe/llama.cpp-delegated` é
classificada como `llama.cpp`.

O primeiro ponto esperado de quebra na prova real é o layout/nome do tensor de
router no GGUF da fatia. Depois vêm geometria Q4_0 incompatível ou camada fora
da faixa, e por fim a qualidade numérica da costura parcial, que ainda não
substitui todas as operações do transformer. Os testes puros do executor e da
telemetria estão escritos; a suíte nativa já foi executada. A prova física
ponta a ponta com um modelo MoE real continua sendo responsabilidade do
captain.

### Fase 5: co-execução CPU/GPU ainda não ativa

A investigação dos warnings mostrou que a política de banda e o double buffer
existiam apenas como scaffolding de testes: não havia perfil medido carregado
no runtime e não havia executor GPU para os pesos Q4_0 do motor próprio. O
`llama.cpp` tem seu próprio backend GPU para o transformer, mas não expõe uma
lane que execute os experts Q4_0 mmapados por `MoeRuntime`.

Esse scaffolding foi removido do binário para não sugerir que a co-execução
acontece. O caminho MoE que existe hoje é CPU para router e experts, com cache
LRU global; a fase 5 só poderá ser retomada quando houver um executor GPU
concreto e uma fonte de medições simultâneas CPU/transferência. Nenhum ganho de
desempenho ou sobreposição CPU/GPU é afirmado.

## Fase 0: baseline local

### Método reproduzível

Para uma conversa real, cada resposta deve guardar o objeto `trace` retornado
por `/api/generate` ou por `/v1/chat/completions` com `stream_options` de uso.
Para cada turno, registrar:

```text
prefill_fraction = prefill_ms / (prefill_ms + decode_wall_ms)
input_tokens, output_tokens, prefill_ms, total_ms
sum(hop.prefill.processing_ms), sum(hop.decode.processing_ms)
```

O tempo total é medido no cliente ao redor da requisição. O `RequestTrace` já
separa `prefill` e `decode` por salto e não deve ser reconstruído a partir de
um total agregado. A conversa deve ter pelo menos quatro turnos, mantendo o
contexto anterior em cada pedido.

Para throughput, usar o mesmo prompt e `max_new_tokens` em todos os ensaios,
executar 1, 2 e 4 requisições simultâneas, e repetir com `max_sessions` 1, 2 e
4. O A/B de `kv_unified` deve manter todos os outros parâmetros iguais. Para
cada célula registrar tokens de saída, duração de parede, tok/s por requisição,
tok/s agregado, erros e a configuração efetiva reportada por `/health`.

### Ambiente encontrado nesta execução

| Medição | Resultado local | Motivo da lacuna |
|---|---|---|
| Conversa real multi-turno | não executada, `0` requisições | não há modelo GGUF no worktree, `ollama list` não lista modelos e as portas locais 8181/8080 não têm servidor POM |
| Prefill/decode | `n/a` | depende da geração real e do `RequestTrace` produzido por um modelo carregado |
| Tok/s com 1, 2 e 4 requisições | `n/a`, `0` células | não há runtime local nem modelo para carregar |
| A/B `kv_unified=true` | não executado | o parâmetro ainda não está exposto no runtime e a única instância POM observada pertence a outro worktree e mantém conexões com o cluster remoto, que está fora do escopo |

Nenhum acesso ao cluster remoto, SSH ou deploy foi feito. Portanto, não há
número de desempenho para inventar. Por decisão do captain, a Fase 2 seguirá
mesmo sem esse baseline; a medição posterior deve usar o método acima.

As medições de geração continuam fora do alcance deste worktree sem um GGUF e
um servidor local. A infraestrutura nativa, porém, foi validada com o symlink
temporário documentado em `build-execucao.md`; os resultados estão abaixo.

### Validação executada

No worktree com `<worktree>/llama.cpp` apontando para
`/Users/jairpereira/Sites/rdma-p2p/llama.cpp`:

- `cargo build -p node`: código 0, 0 erros e nenhum warning.
- `cargo test -p node`: 761 testes passaram, 5 ignorados.
- `cargo fmt --check`: código 0.
- `cargo clippy -p node`: código 0, 0 erros, 45 warnings restantes, todos fora
  do motor MoE e do gateway refatorado neste goal; não foram silenciados.
- `apps/frontend`: `npm run build` passou, com o aviso do Vite sobre chunk
  maior que 500 kB; `npm test` passou com 44 arquivos e 278 testes.

Os warnings restantes foram preservados no registro de execução; não houve erro
do provider Hugging Face nesta validação e nenhum arquivo desse provider foi
alterado. O symlink foi removido ao fim e não é artefato versionado.

### Reexecução

Com um GGUF local e uma instância iniciada a partir deste worktree, repetir o
ensaio sem cluster remoto:

```bash
POM_HOME=/tmp/pom-ftk-local ./target/release/pom \
  --model /caminho/modelo.gguf --single-peer --gateway-port 8080 \
  --inference-port 8181 --n-ctx 8192 --max-new-tokens 128
```

Depois, enviar a conversa e as células de concorrência para o gateway local e
anexar as tabelas de números abaixo deste baseline. O executor deve registrar
commit, sistema, CPU/GPU, memória, feature de build, tipo de KV, `n_ctx`,
`max_sessions`, `n_batch`, `max_new_tokens`, modelo e temperatura.

## Invariantes de segurança

- Metadados ausentes não viram zero silenciosamente. Um orçamento de KV só pode
  ser calculado quando a geometria necessária estiver presente.
- Um rebuild acontece como nova geração: o servidor anterior para de aceitar
  conexões, drena requisições em voo e só então libera o contexto antigo.
  Reconfiguração compatível mantém os pesos no `ModelCache`; troca de fatia ou
  de carregamento limpa esse cache. Falha de rebuild não pode deixar a nova
  geração anunciada como pronta.
- Cancelamento é cooperativo e idempotente. O fechamento do stream interrompe
  a chamada do gateway em voo e impede o próximo chunk ou hop; um frame que já
  foi aceito pelo peer pode terminar. O cleanup libera a sessão em todos os
  peers alcançados.
- O motor novo nunca é selecionado por nome heurístico ou por falha do
  `llama.cpp`; a arquitetura GGUF precisa declarar MoE e a configuração pode
  forçar o caminho denso enquanto o caminho novo estiver em validação.
- O cache LRU de experts é local à máquina. Ele nunca atravessa o fatiamento e
  não transforma peso em tráfego de rede.

## Defeitos anotados

- As medições de throughput e de divisão prefill/decode ainda não são possíveis
  neste ambiente local sem obter um modelo e iniciar um runtime próprio.
