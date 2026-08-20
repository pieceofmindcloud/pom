# Manifesto por modelo original e partes alternativas

## Contexto

O formato atual usa o hash da lista de partes como `pom_id`. Isso faz cada
execução de fatiamento criar uma nova identidade e uma nova pasta, mesmo quando
as partes pertencem ao mesmo GGUF original. Também impede que execuções
posteriores acrescentem alternativas ao mesmo inventário sem substituir o
manifesto anterior.

O modelo de uso desejado é um inventário estável por arquivo original. O mesmo
GGUF pode produzir várias partições por camadas e vários grupos de shards por
Tensor Parallelism. Todas essas partes devem poder coexistir no mesmo manifesto
sem que uma execução posterior remova o que já existe.

## Decisões

### Identidade do modelo e caminho no disco

O `pom_id` do formato novo será o SHA-256 completo do arquivo GGUF original.
Ele não será recalculado a partir dos bytes das partes. A pasta nova usará o
nome sanitizado do modelo e um prefixo curto desse hash:

```text
<shard_dir>/<modelo-sanitizado>-<hash-curto-do-original>/
```

O nome é apenas legibilidade. Para garantir que uma mudança de rótulo não crie
uma segunda pasta para os mesmos bytes, o código procurará primeiro um
manifesto v2 cujo `pom_id` seja o hash completo. Só criará uma pasta nova quando
esse hash ainda não existir.

O nome do `.pom` será determinístico dentro dessa pasta. Uma pasta v2 terá um
único manifesto lógico, atualizado por merge. Manifestos v1 antigos continuarão
legíveis no caminho antigo; não serão migrados automaticamente, porque não
carregam o hash do GGUF original e não permitem provar a equivalência.

### Formato do manifesto v2

O manifesto terá a seguinte forma conceitual:

```json
{
  "pom_version": 2,
  "pom_id": "<sha256-completo-do-gguf-original>",
  "model": {},
  "kind": "layers | tensor-parallel | combined",
  "parts": [],
  "tp_parts": []
}
```

`parts` contém somente fatias de camadas. `tp_parts` contém somente shards de
Tensor Parallelism e exige `rank` e `size` em cada entrada. O manifesto pode
conter ambos os arrays; nesse caso `kind` é `combined`.

Cada array é um inventário, não uma única cobertura de implantação:

- duas entradas com a mesma faixa são permitidas quando representam arquivos
  diferentes;
- entradas como `L0-7` e `L0-3` permanecem juntas;
- grupos TP de tamanhos diferentes permanecem juntos;
- uma entrada repetida pelo mesmo SHA-256 não é duplicada no array.

`complete` para camadas significará que existe pelo menos uma cadeia exata,
sem buracos, entre `L0` e a última camada. Não exigirá que todas as partes do
inventário sejam simultaneamente não sobrepostas. A completude TP será avaliada
por tamanho de grupo e exigirá os ranks `0..size-1` daquele tamanho.

### Merge local e remoto

Toda operação que sela ou recebe uma parte usará uma operação de merge atômica:

1. calcular ou validar o hash do GGUF original;
2. localizar o manifesto v2 correspondente pelo `pom_id` completo;
3. carregar o manifesto local, se existir;
4. unir `parts` e `tp_parts` sem apagar entradas locais;
5. garantir que a parte que está sendo selada ou recebida esteja descrita;
6. mover somente os arquivos novos, preservando arquivos já existentes;
7. gravar o manifesto agregado por arquivo temporário e rename atômico.

O manifesto enviado por um peer é uma descrição de conhecimento, não uma
ordem de substituição. Portanto, se o receptor já tem `L0-7` e o remetente
conhece somente `L0-3`, o resultado conserva as duas entradas. Se o remetente
conhece outras partes que ainda não estão fisicamente no receptor, elas podem
ser descritas pelo manifesto, mas só entram no catálogo quando o arquivo
correspondente existir no disco.

Merges concorrentes no mesmo modelo serão serializados por um lock local de
curta duração. A escrita final continuará atômica para que uma interrupção
deixe o manifesto anterior intacto.

### Catálogo e implantação

O índice do catálogo lerá `parts` e `tp_parts` separadamente. Somente `parts`
participará dos `ShardSet` de pipeline. Shards de TP terão metadados próprios e
não poderão ser confundidos com uma faixa de camadas.

Um único `ShardSet` representa o modelo original e expõe todas as faixas de
camadas disponíveis. O planejador continuará escolhendo uma cadeia concreta,
podendo selecionar uma alternativa em cada posição. A existência de uma
segunda faixa que começa em `L0` não será uma recusa por si só.

### Hash e custo

O hash do GGUF original será calculado em streaming antes da operação de
fatiamento. Para não reler o mesmo arquivo a cada parte, a operação calcula o
hash uma vez por execução; o cache existente por caminho, tamanho e mtime pode
ser reutilizado para chamadas posteriores.

### Transferência

As rotas de oferta, pull, manifesto e download considerarão os dois arrays.
Ao receber um manifesto, o peer chamará o merge local antes de iniciar ou
finalizar o download. O caminho de destino será o diretório resolvido pelo
manifesto local ou remoto com o mesmo `pom_id`, evitando que rótulos diferentes
criem pastas paralelas.

## Invariantes

- `pom_id` v2 é exatamente o SHA-256 completo do GGUF original.
- Um merge nunca remove uma parte já registrada no manifesto local.
- Um arquivo de parte só é catalogado quando existe e está descrito por um
  manifesto válido.
- `parts` nunca contém shard TP; `tp_parts` nunca é usado como faixa de
  pipeline.
- O mesmo SHA-256 não aparece duas vezes no mesmo array.
- Partes sobrepostas são armazenáveis; a implantação escolhe uma cobertura
  válida em vez de tratar o inventário inteiro como uma única cobertura.
- Manifestos v1 continuam legíveis e não são reescritos sem hash verificável do
  modelo original.
- `CHANGELOG.md`, arquivos gerados e `vendor/llama-cpp-sys-2/llama.cpp` ficam
  fora da mudança.

## Verificação

Os testes focados devem cobrir:

- identidade v2 pelo hash do arquivo original, independentemente das partes;
- merge idempotente de faixas alternativas;
- merge que preserva partes locais ausentes no manifesto remoto;
- merge de `parts` e `tp_parts` no mesmo manifesto;
- carregamento de manifesto v1;
- catálogo que mantém alternativas e exclui TP dos `ShardSet` de pipeline;
- transferência que mescla o manifesto remoto ao local;
- localização estável da pasta quando o nome legível do modelo varia.

Depois dos testes focados, o frontend não precisa de alteração funcional, mas o
build deve confirmar que os contratos JSON do catálogo continuam válidos. A
documentação de `fatiamento`, `implantacao`, `transferencia` e os
`AGENTS.md` das pastas modificadas devem refletir o formato v2.

## Fora de escopo

- Migrar automaticamente pastas v1 sem hash verificável do GGUF original.
- Apagar partes ou manifestos antigos.
- Alterar o algoritmo de corte GGUF ou o formato dos pesos.
- Misturar shards TP na seleção de uma pipeline por camadas.
- Criar uma política de retenção diferente para partes alternativas.
