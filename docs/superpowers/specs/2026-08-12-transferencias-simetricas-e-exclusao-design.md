# Transferências simétricas e exclusão de modelos

**Data:** 2026-08-12
**Estado:** aprovado, pendente de plano

## Problema

Uma transferência só é visível para quem **recebe**. O `TransferJob` nasce em
`spawn_download` (`crates/node/src/backend/transfer.rs`), no receptor; a máquina
que envia cria uma concessão, dispara a oferta e não guarda registro nenhum. Na
tela dela não há barra, não há velocidade, não há como parar.

Cancelar não existe em lado nenhum: começou, vai até o fim, falhar ou o operador
matar o processo.

E não há como apagar um modelo do disco pela interface. Fatiar, receber e fundir
enchem o disco; esvaziá-lo é trabalho de terminal.

## O que muda

1. Os dois lados veem a transferência, com barra, velocidade e ETA.
2. Os dois lados podem cancelar a qualquer momento, e o outro lado sabe que foi
   cancelamento - não uma queda de rede.
3. `/modelos` e `/fatias` ganham exclusão do arquivo em disco.
4. Transferir e excluir passam a aceitar **só arquivos GGUF**.

## Decisões e por quê

### Um `job_id` só, cunhado pelo receptor

O receptor continua cunhando o id em `spawn_download`, mas passa a carregá-lo no
pedido: `?job=<id>&peer=<node_id>` em `manifest` e `download`. O remetente
registra **o mesmo id** como job de direção `sending`.

Um id e dois lados é o que permite "ver" e "cancelar" simetricamente sem inventar
um segundo vocabulário para a mesma transferência.

`peer=<node_id>` é resolvido para URL pela tabela de peers que o nó já mantém,
como `send_to_peer` faz. **Nenhuma URL vem do outro lado da rede** - o remetente
nunca abre conexão para um endereço que o pedido escolheu.

### Progresso do remetente é contado, não relatado

O remetente é quem transmite os bytes, então ele já sabe. O handler `download`
envolve o `ReaderStream` num contador que atualiza `bytes_done`,
`bytes_per_sec` e `eta_secs` a cada 500 ms - o mesmo ritmo do loop do receptor.

Descartada a alternativa de o receptor reportar o progresso de volta: adiciona
tráfego, um caminho que falha em silêncio, e some justamente quando o receptor
morre no meio, que é quando o operador mais quer olhar.

Ganho colateral: um "puxar" iniciado pelo peer passa a aparecer na máquina que
serve. Hoje ela não faz ideia de que está enviando gigabytes.

### Cancelar apaga o parcial

Cancelar é desistir. O `.part` é removido e o disco volta ao que era, coerente
com a regra que já existe de nunca deixar arquivo pela metade parecendo fatia
usável. O custo aceito: recomeçar refaz a transferência inteira.

Guardar o parcial para retomar foi recusado - deixaria gigabytes ocupados que a
tela não mostra, e exigiria uma noção de "pausado" mais uma forma de limpar.

### O outro lado é avisado

Cortar um stream é indistinguível de queda de rede. Sem aviso, o operador do
outro lado vê "falhou" e tenta de novo achando que foi soluço do enlace.

Quem cancela chama a **mesma rota** no peer, então há um verbo só:
`POST /api/transfer/jobs/:id/cancel` com `{"by": "<hostname>"}`.

Se o aviso não chegar (peer fora do ar), o caminho que já existe assume: a
conexão morre e vira `failed` com o motivo real. **A tela nunca chama de
cancelamento o que foi queda.**

### Só GGUF, pelo nome e pelo conteúdo

Regra em conjunção: os 4 primeiros bytes têm que ser a assinatura `GGUF`, **e**,
se o arquivo tiver extensão, ela tem que ser `.gguf` (sem diferenciar
maiúsculas).

Assim um blob do Ollama (`blobs/sha256-<hash>`, sem extensão) passa, um `.gguf`
falso não passa, e um `.txt` com cabeçalho forjado também não. Uma regra só de
extensão tiraria todo o Ollama do recurso, e é dali que vem boa parte do que
`/modelos` lista.

Isto **estreita** o comportamento atual: `local_send_path` aceita hoje qualquer
arquivo legível. Depois desta mudança, mandar um `.txt` para o peer deixa de
funcionar. É intencional.

### Exclusão avisa, mas não impede

O operador decide. Sem `force`, a chamada responde 409 com os avisos; com
`force`, apaga.

## Arquitetura

### `crates/node/src/backend/transfer.rs`

`TransferJob` ganha:

| campo | porquê |
|---|---|
| `direction: "sending" \| "receiving"` | a tela precisa dizer de que lado está |
| `peer_host` (renomeia `from_host`) | `from_host` mente na direção de envio |
| `path` | a trava de exclusão precisa saber que arquivo está em trânsito |

`TransferState` ganha `cancelled: RwLock<HashSet<String>>`.

Estado novo `cancelled`, ao lado de `running` / `verifying` / `done` / `failed`.

Rotas:

- `POST /api/transfer/jobs/:id/cancel` — corpo `{"by": "<hostname>"}`. Marca o
  job local como cancelado e, quando o cancelamento nasceu aqui, chama a mesma
  rota no peer. Idempotente: cancelar duas vezes é cancelar uma.
- `manifest` e `download` aceitam `?job=` e `?peer=` (ambos opcionais, para não
  quebrar um peer que ainda não foi atualizado - sem eles a transferência
  funciona como hoje, só sem visão do lado remetente).

Pontos de checagem do cancelamento:

- **Receptor** — `run_download` já itera por chunk de 1 MB; checa o flag a cada
  volta, apaga o `.part`, marca `cancelled`.
- **Remetente** — o stream envolvido termina cedo quando o flag aparece.

`ensure_gguf(path) -> Result<(), String>` aplicada em três portas:
`local_send_path` (enviar), `servable_path` (servir - fecha também o caminho do
`grant`, que hoje aceita qualquer arquivo legível) e a exclusão.

### Exclusão

`POST /api/ui/models/delete`, corpo `{path, force}`. Sem `force` e havendo
motivo, 409 com `warnings` estruturados:

- `serving` — este nó está servindo essa fatia (camadas N-M). Apagar tira o
  modelo do ar sem que nada na tela diga; o nó segue anunciando cobertura e só
  quebra na próxima requisição.
- `transfer` — há transferência em curso com esse arquivo, com o id do job.
- `ollama_store` — o arquivo está dentro do store do Ollama, cujo manifesto vai
  ficar órfão. O certo é `ollama rm`.

### Frontend

- `Transfers.tsx` — jobs dos dois sentidos, separados por direção, cada linha
  com botão de cancelar enquanto ativa.
- `WholeModels.tsx` e `Shards.tsx` — botão de excluir, com diálogo mostrando o
  caminho real, o tamanho e os avisos antes de confirmar.

## Erros

| situação | comportamento |
|---|---|
| aviso de cancelamento não chega ao peer | o cancelamento local vale; o peer cai no caminho de conexão morta e reporta `failed` com o motivo real |
| `job`/`peer` ausentes (peer desatualizado) | transferência funciona como hoje, sem visão do lado remetente |
| `peer` desconhecido na tabela | cancela local e registra que não deu para avisar |
| cancelar job já terminado | sem efeito, resposta de sucesso |
| excluir arquivo que sumiu no meio | 404 com o caminho |
| excluir sem ser GGUF | 400, nomeando qual das duas checagens falhou |

## Testes

- `ensure_gguf`: aceita `.gguf` com assinatura e blob sem extensão com
  assinatura; recusa `.txt` com assinatura, `.gguf` sem assinatura, e diretório.
- Cancelar marca `cancelled` e remove o `.part`.
- Cancelar job inexistente ou já terminado não quebra.
- Exclusão recusa não-GGUF; devolve `warnings` quando em uso; apaga com `force`.
- Os testes de caminho que já existem (`safe_model_path`,
  `the_operator_may_send_a_file_from_anywhere`) continuam valendo, ajustados
  para arquivos com assinatura GGUF real.

## Fora de escopo

- Retomar transferência cancelada (a decisão foi apagar o parcial).
- Excluir arquivo em máquina remota pela tela local.
- Limpeza automática de `recebidos/`.
