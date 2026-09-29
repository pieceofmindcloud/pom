# Paralelismo de pipeline e tensor

O POM distribui inferência de duas formas diferentes. A escolha deve ser feita
de acordo com a topologia da rede e com o motivo pelo qual um único nó não
consegue carregar o modelo.

## Paralelismo de pipeline

No pipeline, cada nó mantém uma faixa completa de camadas do transformer. O
planejador ordena as faixas pela camada inicial e só aceita um plano que cubra
as camadas necessárias sem sobreposição. O nó que serve a camada 0 recebe o
prompt inteiro, processa a própria faixa e entrega um estado oculto f16 para o
próximo nó. O processo se repete até a cabeça de saída, que amostra o próximo
token.

Os pesos nunca são enviados durante a inferência. Cada máquina precisa ter a
própria cópia ou as próprias fatias do GGUF. O que trafega é o estado oculto,
com aproximadamente `hidden_dim * 2` bytes por token por salto, mais os headers
do frame. No decode, a resposta direta entrega o token da cauda diretamente ao
gateway; no modo de anel antigo, o resultado borbulha de volta pela cadeia.

O pipeline é a opção recomendada entre máquinas na internet. Ele faz uma
travessia por fronteira de camada para cada token e não precisa de uma barreira
global a cada operação interna. A ordem dos nós importa, e a máquina da camada
0 deve ter memória suficiente para o prompt e o KV cache.

### Escolha da pipeline por tempo de entrega

Com réplicas, a pipeline escolhida é a que **entrega a requisição mais cedo**
(`routing/fastest.rs`, `plan_fastest`), e não a de menor latência de rede. Por
requisição o gateway estima, para cada caminho que cobre `0..total_layers`:

- **prefill:** tokens novos × ms/token de cada estágio × (1 + fila);
- **decode:** tokens esperados (o `max_tokens` pedido, entre 16 e 512) ×
  ms/passo de cada estágio × (1 + fila);
- **rede:** (1 + tokens esperados) × RTT de cada aresta, inclusive quem pede →
  cabeça e cauda → quem pede.

Os termos são somas por estágio e por aresta, então o melhor caminho sai de uma
programação dinâmica sobre as faixas. Uma cabeça distante mas rápida vence a
próxima e lenta quando termina antes; com pouca saída pedida, a rede volta a
pesar mais.

- **Velocidade** de cada peer vem do `/health` (`perf`): um teste local da
  fatia logo depois de o modelo carregar (prefill de 64 tokens e 8 passos de
  decode, entrada sintética, sem rede - `infer_server/perf.rs`) e a média
  móvel do tráfego real. Peer sem medição usa o custo médio por camada dos
  medidos.
- **Rede** vem das rotas fixadas (`route_plan`): quem pede usa as próprias; as
  arestas entre dois outros peers vêm do `/health` do peer de origem
  (`route_rtt_ms`, `gateway_rtt_ms`). Aresta sem medição conta 100 ms.
- **Balanceamento:** a fila do estágio é o maior entre `active_generations` do
  peer (gerações em curso vindas de qualquer gateway) e a carga em voo deste
  gateway. Ela multiplica o custo do estágio: o peer mais rápido com fila fica
  mais lento que um médio ocioso, e a carga se espalha pela rede sem rodízio.
- **Prefixo:** cada caminho que já tem o prefixo deste prompt retido no cache
  do gateway entra como candidato, estimado só com os tokens novos. A
  reutilização vence quando compensa e perde quando o peer do cache está
  saturado.
- A cabeça escolhida na interface (`inference_head`) continua sendo a única
  aceita. Sem caminho viável, o planejador guloso antigo decide.

O log do gateway diz a escolha: `pipeline por tempo de entrega: A→B ~N ms`.

### Réplicas e scheduler

Duas máquinas que anunciam a mesma faixa são réplicas de um estágio, não dois
saltos consecutivos da mesma requisição. Cada peer mantém seu próprio scheduler
local, contexto `llama.cpp` e KV cache. O gateway escolhe uma réplica para cada
sessão e preserva essa afinidade nas rodadas seguintes; as duas réplicas não
compartilham fila nem scheduler global. Requisições independentes que chegam à
mesma réplica podem formar um microbatch local.

Esse batching local não transforma réplicas em um grupo TP. Para batching
distribuído de pipeline, os peers ainda precisam negociar um frame multi-item e
correlacionar cada estado oculto por sessão. No estado atual, a negociação e o
fallback de frame continuam individuais entre hops. `RouteAffinity` registra
os node IDs escolhidos por estágio, e `BatchRoute::group` só permite agrupar
itens que compartilham o mesmo próximo hop e contrato de execução.

## Tensor parallelism

No TP, todos os ranks carregam todas as camadas, mas alguns tensores de cada
camada são particionados. Projeções coluna-paralelas produzem larguras parciais;
as projeções linha-paralelas consomem essas larguras e precisam somar as
contribuições de todos os ranks. A soma é um all-reduce e cada rank recebe o
mesmo resultado antes de continuar a camada.

O POM carimba cada shard TP com tamanho e rank. A coletiva HTTP em
`POST /v1/tp/reduce` troca os parciais dos workers durante o forward pass; o
primary contribui na barreira local. O sistema registra bytes, espera e
percentis na telemetria. Em um modelo de 32 camadas com dois
pontos row-parallel por camada, um decode faz 64 reduces por token. Esses
reduces não podem ser adiados para o fim: a próxima operação da camada depende
do resultado somado. Fazer um único reduce final mudaria a matemática do
transformer.

A cabeça de saída permanece replicada na implementação atual. Reduzi-la exigiria
uma partição por vocabulário e all-gather ou merge de logits, além de tratar o
tokenizer. O output norm entrega largura cheia, portanto uma partição por
entrada também seria incorreta.

## Como escolher

| Situação | Recomendação | Motivo |
|---|---|---|
| Duas máquinas em regiões ou ISPs diferentes | Pipeline | Menos barreiras síncronas e menos sensibilidade ao RTT |
| GPUs na mesma máquina ou na mesma LAN de baixa latência | TP | As coletivas podem aproveitar links rápidos e o modelo fica replicado por camada |
| Um modelo não cabe em uma máquina, mas cada faixa cabe em um nó | Pipeline | Distribui pesos por camadas e mantém o tráfego em hidden states |
| Uma camada individual não cabe em nenhum nó | TP ou redução de quantização | O pipeline não divide uma camada; TP reduz largura dos tensores |
| WAN com TP obrigatório | Evitar se possível | O custo é aproximadamente `reduces * RTT` por token, além da transferência dos parciais |

TP pode reduzir a memória de pesos por rank, mas não transforma a rede em um
tráfego de estado oculto único. O próprio parcial das projeções row-parallel
precisa atravessar a rede antes que a camada seguinte possa rodar. Compressão
de frame do pipeline não elimina essa dependência; são problemas diferentes.

O scheduler de um grupo TP também não é uma fila compartilhada entre processos.
O rank primário deve coordenar a mesma lista de itens, posições e shapes em
todos os ranks antes de uma coletiva. Enquanto essa chave de batch não estiver
ligada à barreira, o runtime mantém TP no caminho de uma sequência por decode.

## Telemetria

Pipeline e TP aparecem em blocos separados. Pipeline mostra hops, fase,
backend, transporte, bytes e o modo de resposta. TP mostra reduces esperados e
observados, espera total, p50/p95, bytes de request e response e se o caminho
foi local, direto ou túnel. Uma espera TP próxima de `reduces * RTT` indica que a
topologia, e não apenas o kernel CUDA, está dominando a taxa de tokens.

Para uma análise operacional, compare:

1. `backend` e `espera do motor`, que mostram fila e computação local;
2. `transporte` e bytes, que mostram o custo de frames do pipeline;
3. `tp.wait_ms`, `tp.reduce_count` e os percentis, que mostram barreiras TP;
4. o modo `pipeline` ou `tp` registrado na execução, que confirma o caminho
   realmente usado.
