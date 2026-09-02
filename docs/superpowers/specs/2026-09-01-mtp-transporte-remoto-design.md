# Especificação: rascunho MTP no transporte remoto

## Contexto

O transporte `inference/remote-token-stream-v1` transformou a geração em um
ciclo que roda na cabeça remota. A implementação anterior só enviava a
especulação inicial para esse ciclo quando o plano tinha um único hop. Em um
plano fatiado, a cauda continuava capaz de produzir `next_draft`, mas o
emissor do stream descartava esse campo. O gateway então recebia apenas os
tokens aceitos e concluía, incorretamente, que a especulação havia acabado.

O objetivo é fazer o MTP fatiado coexistir com o stream remoto sem criar uma
travessia adicional por token.

## Decisão

O frame de controle que já inicia o stream continua iniciando a sessão. Cada
resposta da cauda que contiver `next_draft` atualiza o estado do ciclo na
cabeça remota. O rascunho não volta ao gateway como uma chamada separada e não
há novo pedido de rede para transportá-lo.

O callback do stream passa a publicar metadados explícitos sobre a
especulação:

- `speculated`: a rodada atual verificou um rascunho;
- `speculation_active`: a rodada seguinte ainda será executada com MTP;
- `speculation_disabled_reason`: razão estável e legível quando o MTP foi
  desativado.

Os campos são opcionais para que um par antigo continue sendo entendido. A
ausência deles significa o comportamento anterior: um token ou lote remoto
sem indicação de especulação.

## Máquina de estados

1. A cabeça recebe o `Speculate` inicial quando o plano anuncia MTP fatiado.
2. A resposta da cauda pode conter `next_draft`; o produtor do stream grava o
   rascunho no próximo `step_meta["speculate"]["draft"]` antes de continuar o
   ciclo.
3. A mesma rodada é emitida com `speculated=true` quando a resposta confirmou
   uma verificação especulativa. Uma rodada de aquecimento pode ser
   `speculated=false` e ainda ter `speculation_active=true` se já tiver
   produzido o rascunho da próxima rodada.
4. Se não houver rascunho, o produtor remove a especulação da próxima chamada
   e emite `speculation_active=false` com uma razão nomeada.
5. O gateway usa `speculation_active` para manter suas estatísticas corretas,
   mas nunca reinjeta um rascunho quando a sessão está no stream remoto.

## Razões de degradação

As razões devem distinguir os casos observáveis:

- `tail_without_nextn`: a cauda não tem a cabeça `nextn` disponível;
- `model_without_mtp`: o modelo servido não está pronto para MTP;
- `hybrid_tail_no_rollback`: a cauda híbrida ou recorrente informa
  `kv_can_shift=false` e não pode produzir uma rodada especulativa segura;
- `peer_without_sliced_mtp_wire`: algum peer não anuncia a capacidade da wire
  nova.

Uma razão específica pode incluir o detalhe fornecido pelo motor, mas deve
  manter um desses identificadores para logs e testes.

## Compatibilidade

O gateway só envia a especulação inicial quando todos os hops anunciam
`speculate/v1-mtp-sliced` e a cauda anuncia `speculate/v1-mtp`. Com um peer
antigo, o plano continua usando o transporte remoto ou o caminho comum sem
MTP. A ausência dos novos campos nos callbacks não é erro de protocolo e
  resulta no comportamento de hoje.

## Latência e invariantes

O rascunho é anexado aos metadados do frame de resposta que já existe. Não há
`sleep`, espera de agrupamento, temporizador, chamada HTTP adicional ou limite
novo de velocidade. `FIT_MAX_UBATCH` permanece 512 e `kv_seq_budget` não é
alterado.

## Verificação obrigatória

Os testes devem cobrir:

1. plano com dois ou mais hops, stream remoto e MTP pronto na cauda, mantendo
   a especulação ativa depois do primeiro callback;
2. uma sessão stream cujo contador de chamadas prove que cada rodada usa uma
   única travessia de rede;
3. peer sem `speculate/v1-mtp-sliced`, com degradação e razão
   `peer_without_sliced_mtp_wire`;
4. cauda com `kv_can_shift=false`, sem `next_draft` e com razão
   `hybrid_tail_no_rollback`;
5. callbacks antigos sem os metadados novos, aceitos sem quebra.

