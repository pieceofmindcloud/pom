# Fatiamento multipart GGUF por propriedades observáveis

## Contexto

Um GGUF multipart é um modelo lógico distribuído entre arquivos físicos. O
primeiro arquivo pode conter apenas KV e os demais podem conter todos os
tensores. O loader do llama.cpp abre o primeiro arquivo, verifica `split.count`,
`split.no` e `split.tensors.count`, localiza os irmãos pelo padrão do nome e
monta uma tabela lógica de tensores sem remontar os pesos.

O POM fazia o contrário: `read_layout`, o splitter de pipeline e os dois
caminhos de tensor parallelism abriam somente o caminho recebido. Além disso,
o recorte de metadata por camada só funcionava quando o vetor tinha exatamente
`block_count` itens. O arquivo medido tem 43 blocos, quatro conjuntos de
sufixos de tensor alternados por índice e `attention.compress_ratios` com 46
valores. O llama.cpp lê esse vetor no índice das camadas declaradas e exige
somente que ele não seja menor que a contagem de blocos; os valores além dela
não são usados pelo modelo.

## Objetivo

Permitir que o POM leia e fatie qualquer conjunto GGUF multipart diretamente,
sem criar uma cópia GGUF única, preservando o comportamento de um GGUF comum.
Toda decisão deve usar somente o cabeçalho `split.*`, a tabela de tensores, os
sufixos de `blk.N.*` e a metadata declarada pelo arquivo ou pelo loader.

## Desenho

### Fonte GGUF unificada

Adicionar um leitor compartilhado que:

1. abre e valida o primeiro arquivo;
2. se `split.count` for maior que um, exige `split.no == 0`, deriva os irmãos
   pelo padrão `-00001-of-00004.gguf`, valida índice, contagem e quantidade
   total de tensores;
3. mantém um mmap por arquivo, sem copiar ou concatenar os dados;
4. calcula o tamanho de cada tensor pela diferença entre offsets dentro do
   arquivo que o contém;
5. devolve cada tensor com `part_index`, offset local, bytes, nome, dimensões e
   dtype;
6. conserva os bytes do KV e da tabela do primeiro cabeçalho para o splitter.

Para `split.count` ausente ou igual a um, a fonte tem uma única parte e o fluxo
é o mesmo com uma tabela de origem. Um caminho que aponta para `split.no != 0`
é recusado e instrui o operador a escolher a primeira parte.

O layout usa a soma dos bytes físicos como `source_file_bytes`. O hash da fonte
é o SHA-256 da concatenação ordenada dos arquivos multipart, calculado em
streaming, sem criar arquivo temporário. Isso muda a identidade somente para
conjuntos multipart; o hash de um GGUF único continua sendo o hash de seus
bytes.

### Metadata indexada por camada

Durante a leitura do KV, uma entrada é vetor por camada quando:

- está sob o namespace da arquitetura;
- não é namespace estrutural de rope;
- tem elementos escalares;
- tem comprimento pelo menos `original_block_count`.

O splitter escreve somente os elementos `start_layer..=end_layer`, mantendo o
tipo original e reindexando a posição inicial para zero na fatia. Comprimento
1 permanece broadcast. Um vetor arquitetural com comprimento entre 2 e
`original_block_count - 1`, ou um vetor aninhado com tamanho de camadas, é
recusado pelo nome e pelo tamanho, porque não há semântica observável para
adivinhar. Arrays fora do namespace arquitetural continuam sendo globais e
passam sem recorte.

O mesmo predicado é usado pelo cálculo de tamanho serializado em `gguf.rs`.
Os tensores continuam sendo selecionados pelo índice absoluto e renomeados para
`blk.0..` na saída. `layer_kinds` e `snap_end_layer` permanecem como proteção
para corridas arquiteturais em blocos; quando a mudança acontece a cada camada,
o metadata recortado é a proteção efetiva e não há tentativa de inventar um
passo de corte.

### Catálogo, API e interface

`DiscoveredModel` recebe a descrição do conjunto multipart: se é multipart, o
total físico em bytes, a contagem e a lista ordenada dos arquivos. A descoberta
continua publicando só a primeira parte como modelo lógico, mas a soma passa a
ser exibida e usada no planejamento. O layout e o hash usam a mesma fonte
unificada.

O plano de fatias inclui um bloqueio derivado da fonte quando o conjunto não
está completo, aponta para uma parte secundária ou a metadata/tabela não pode
ser unificada. A mensagem inclui a propriedade observada e a ação: manter
todas as partes juntas, selecionar a primeira parte e pedir ao POM para gerar
fatias. Não existe critério baseado em arquitetura pelo nome do modelo.

A tela de fatiar mostra um selo MULTIPARTE, o tamanho somado em GB e a lista de
partes com índice e bytes. Quando o plano está bloqueado, a ação de fatiar fica
desabilitada e o motivo concreto aparece no preview. O caso de arquivo único
mantém a apresentação e o fluxo atuais.

### Fronteira de transferência

Transferência de uma fatia POM continua transferindo um arquivo endereçado por
SHA-256, como exige o manifesto. A transferência arbitrária do modelo GGUF
inteiro multipart não será transformada nesta mudança em um protocolo de
conjunto: ela exige manifesto de arquivos, retomada coordenada e confirmação de
todos os irmãos. O catálogo, o planejamento e a identidade do fatiamento deixam
de mentir sobre o conjunto e o PR registra essa fronteira.

## Testes

- leitor multipart sintético com primeiro arquivo sem tensores, irmãos com
  tensores e offsets locais;
- validação de índices, contagem e recusa de parte secundária;
- vetor alternado por camada com cabeça e cauda especiais, verificando recorte
  real e reindexação;
- vetor maior que `block_count`, verificando que somente a faixa das camadas é
  escrita;
- vetor curto e vetor aninhado indexado por camada, verificando recusa;
- regressão do GGUF único e dos três caminhos do splitter;
- testes frontend do catálogo e do bloqueio multipart, além do build normal.

## Fora de escopo

- executar o fatiamento nos arquivos gigantes da ksia;
- tocar no processo POM ou no cluster em execução;
- corrigir os bindings ausentes do `llama-cpp-sys-2`;
- criar um protocolo novo de transferência coordenada de modelos multipart;
- decisões amarradas a nome de modelo, família ou arquivo.

