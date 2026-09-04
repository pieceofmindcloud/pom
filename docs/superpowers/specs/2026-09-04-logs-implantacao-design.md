# Logs da implantação - desenho

## Objetivo

Expor ao administrador uma consola do processo inteiro do cluster, mantendo as
linhas na ordem em que chegam ao nó principal e associando cada linha ao
`deploy_id` conhecido no nó que a emitiu.

## Arquitetura

Cada processo instala uma camada de `tracing` que só faz captura quando a
política local está ligada. A camada lê um contexto atômico do nó e da
implantação, serializa a linha e usa `try_send` numa fila limitada. Fila cheia
descarta sem bloquear o emissor e incrementa um contador de perdas.

O envio sai em lotes por um protocolo request/response libp2p exclusivo,
separado do túnel de inferência. O nó de origem procura apenas um peer que se
anunciou como `Principal`; não há retransmissão entre peers. Uma task de fundo
faz o envio best-effort e ignora falhas de transporte. O principal ingere os
lotes numa fila própria e um escritor dedicado persiste no SQLite. Cada linha
recebe `received_seq` pela inserção no principal; timestamp remoto é mantido
apenas como informação.

O campo `cluster_logs_enabled` no anúncio é somente gatilho. Quando divergir do
valor local, o nó consulta o principal por `/api/cluster/logs` usando a chave de
conexão e só adota uma resposta autenticada. Falha mantém o valor anterior.
O principal muda a política por `/api/ui/network/logs`, em tempo real, e também
altera a captura local.

## API e interface

- `GET /api/ui/cluster-logs?deploy_id=...` devolve uma janela em ordem de
  chegada e a contagem de perdas.
- A tela `/implantacoes/logs` mostra essa janela em terminal preto, com nome do
  nó antes de cada linha e filtro por implantação. Atualiza em cadência curta;
  o backend continua sendo a fonte de verdade.
- A tabela de Histórico ganha apenas um link filtrado para a consola da
  implantação, sem mover a lógica de Histórico.
- Preferências mostra o interruptor "Ligar logs"; o backend recusa alteração
  em qualquer nó que não seja o principal.

## Não prejudicar a inferência

A camada de captura não aguarda locks assíncronos, SQLite ou rede. As filas de
captura, transporte e persistência são limitadas. O transporte dedicado tem
timeout curto e não reutiliza o túnel de inferência. O benchmark de tok/s será
registrado no commit com a mesma máquina, modelo e prompt nos dois estados.

## Testes

Os testes cobrem a ordem `received_seq`, descarte com contador, adoção da
política e preservação após erro de busca, associação ao `deploy_id`, controle
de autoridade e serialização do protocolo. A UI cobre o rótulo distinto,
ordem visual, nome do nó e filtro.
