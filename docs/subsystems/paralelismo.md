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

## Tensor parallelism

No TP, todos os ranks carregam todas as camadas, mas alguns tensores de cada
camada são particionados. Projeções coluna-paralelas produzem larguras parciais;
as projeções linha-paralelas consomem essas larguras e precisam somar as
contribuições de todos os ranks. A soma é um all-reduce e cada rank recebe o
mesmo resultado antes de continuar a camada.

O POM carimba cada shard TP com tamanho e rank. A coletiva HTTP em
`POST /v1/tp/reduce` troca os parciais durante o forward pass e registra
bytes, espera e percentis na telemetria. Em um modelo de 32 camadas com dois
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

## Telemetria

Pipeline e TP aparecem em blocos separados. Pipeline mostra hops, fase,
backend, transporte, bytes e o modo de resposta. TP mostra reduces esperados e
observados, espera total, p50/p95, bytes de request e response e se o caminho
foi direto ou túnel. Uma espera TP próxima de `reduces * RTT` indica que a
topologia, e não apenas o kernel CUDA, está dominando a taxa de tokens.

Para uma análise operacional, compare:

1. `backend` e `espera do motor`, que mostram fila e computação local;
2. `transporte` e bytes, que mostram o custo de frames do pipeline;
3. `tp.wait_ms`, `tp.reduce_count` e os percentis, que mostram barreiras TP;
4. o modo `pipeline` ou `tp` registrado na execução, que confirma o caminho
   realmente usado.
