# Armazenamento, paralelismo e transferências seguras

**Estado:** aprovado em conversa, pendente de plano de implementação

## Contexto

O produto já fatia GGUF por tamanho, distribui fatias e oferece dois caminhos
de paralelismo na implantação: pipeline parallelism e tensor parallelism. Há
três lacunas relacionadas ao uso operacional desse fluxo:

1. A última fatia ainda aceita um alvo em GB, embora seja a única fatia que
   deve receber todas as camadas restantes. O arredondamento por camadas pode
   deixar uma parte do modelo sem cobertura.
2. A tela de pastas não impede que a pasta de fatias fique dentro da pasta de
   modelos inteiros, e não há limite de armazenamento nem visão do uso atual.
3. A tela de transferências aceita um caminho arbitrário digitado pelo usuário.
   O caminho completo atravessa a fronteira frontend/backend, e a resolução
   atual permite escolher qualquer arquivo legível que passe pela validação de
   GGUF.

## Decisões

### Fatiamento

A última entrada da lista de fatias é sempre `rest`, sem campo editável de GB.
O contrato do backend também considera a última entrada como resto, mesmo que
um cliente antigo envie `target_mb` nela. O atalho de divisão igual gera alvos
para as entradas anteriores e `target_mb: null` na última entrada. A camada
continua sendo a unidade indivisível, mas a última entrada fecha a cobertura
até `total_layers - 1`.

### Armazenamento

O limite de armazenamento é por nó e vale somente para a pasta de fatias. A
configuração persistida será:

```json
{
  "shard_max_gb": 100
}
```

O valor padrão para configurações antigas ou ausentes é 100 GB. O backend
recusa valores não finitos ou menores ou iguais a zero.

A pasta de fatias não pode ser a mesma pasta de qualquer modelo inteiro nem
ficar dentro dela. A comparação usa caminhos canônicos, portanto também cobre
aliases por symlink enquanto as pastas existirem. O backend continua sendo a
autoridade da validação.

`GET /api/ui/paths` passa a retornar o uso recursivo da pasta de fatias, o
limite em bytes e o percentual. A interface renomeia a tela para
**Armazenamento**, preserva `/config/pastas` como redirecionamento compatível e
mostra uma barra `usado / máximo`. As pastas de modelos inteiros continuam
sendo somente leitura para essa finalidade: a aplicação não apaga modelos
inteiros automaticamente.

Antes de iniciar um novo fatiamento, o nó faz uma retenção local das fatias
antigas, ordenadas pela data de criação, com fallback para modificação quando o
sistema de arquivos não fornece criação. Somente artefatos da pasta de fatias
que não estejam em uso e não sejam saídas do fatiamento atual são candidatos à
remoção. O modelo atualmente servido é protegido. Se os arquivos protegidos e
as novas saídas não puderem caber no limite, o fatiamento é recusado antes de
começar e o operador recebe a razão.

### Transferências

`POST /api/transfer/send` deixa de aceitar `path` e passa a aceitar somente:

```json
{
  "to_node_id": "...",
  "file_name": "qwen_0_15.gguf"
}
```

O frontend só oferece arquivos `.gguf` encontrados pelo catálogo local. O
combobox mostra o nome amigável e o caminho completo, mas envia somente o
basename. O botão de envio na lista de modelos e o combobox de fatias usam o
mesmo contrato.

O backend revarre o catálogo local e resolve o basename contra os arquivos
descobertos. Ele recusa nomes com separadores, `..`, extensão ausente ou
diferente de `.gguf`, arquivos fora do catálogo, conteúdo que não seja GGUF e
basenames ambíguos. A resolução é feita no servidor para que um pedido manual
ou adulterado não consiga escolher um caminho fora da lista.

O nome recebido para a gravação em `recebidos/` também é validado como basename
GGUF antes de qualquer criação de caminho, impedindo traversal em ofertas
malformadas.

### Documentação de paralelismo

Será criado `docs/subsystems/paralelismo.md` e o caminho de documentação
existente sobre o caminho de uma pergunta ganhará um aprofundamento que liga
Pipeline Parallelism e Tensor Parallelism.

O guia explicará que pipeline mantém camadas completas por máquina e troca
hidden states, sendo a escolha preferencial para máquinas distantes e
heterogêneas. TP divide pesos entre ranks e sincroniza reduces dentro do
forward pass, sendo indicado para máquinas próximas e interconexões de baixa
latência. Também documentará o custo de RTT, as limitações de arquitetura e a
possibilidade de combinar pipeline entre grupos com TP somente onde a rede
permite.

## Interfaces afetadas

- `Config.shard_max_gb` e seu default de 100 GB.
- `GET /api/ui/paths`, com armazenamento usado e limite.
- `POST /api/ui/paths`, com `shard_max_gb` e validação de sobreposição.
- `POST /api/transfer/send`, com `file_name` em vez de `path`.
- `SizePlan`, com a última faixa marcada como resto sem alvo editável.
- Rotas frontend `/config/armazenamento`, `/config/pastas` como alias,
  `/transferencias/enviar`, `/modelos` e `/fatias`.

## Invariantes

- Nenhum caminho enviado pelo frontend é usado diretamente para abrir um
  arquivo local.
- Nenhum arquivo que não termine em `.gguf` pode sair por uma transferência.
- A última faixa de um plano válido sempre termina na última camada ou fica
  explicitamente vazia porque as anteriores já cobriram o modelo.
- O limite da pasta de fatias é aplicado antes de iniciar uma nova escrita.
- Nenhuma pasta de modelos inteiros é apagada pela política de retenção.
- Nenhum código do fork `llama.cpp` será modificado.

## Fora de escopo

- Alterar o protocolo de inferência ou a implementação nativa de TP.
- Apagar automaticamente modelos inteiros.
- Permitir envio de blobs do Ollama sem extensão.
- Criar upload de arquivos que ainda não estejam no catálogo local.
