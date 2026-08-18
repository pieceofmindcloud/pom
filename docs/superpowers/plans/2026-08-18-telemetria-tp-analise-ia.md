# Telemetria de TP, análise com IA e modelo inicial

Data: 2026-08-18

## Objetivo

Expor o custo das coletivas TP, explicar o gargalo de uma execução, enviar as
duas tabelas de telemetria ao modelo atual e mostrar o modelo da conversa antes
da primeira mensagem.

## Decisões

- O backend anexa `TpDecodeTelemetry` opcional a `node_timings.tp`.
- Os bytes e esperas TP permanecem separados dos frames de pipeline.
- O frontend mantém compatibilidade com traces antigos e usa uma análise pura,
  sem alterar roteamento.
- O botão envia uma mensagem visual curta e mantém as duas tabelas somente no
  conteúdo da requisição.
- O modelo inicial é uma marca visual, não um turno enviado ao modelo.

## Entrega

- [x] Instrumentar transporte direto/túnel, reduces, broadcasts e percentis TP.
- [x] Propagar TP por `NodeTimings` e pelo contrato `PhaseTrace`.
- [x] Criar diagnóstico para TP, pipeline, fila, backend e execução equilibrada.
- [x] Renderizar tabela TP sem remover a tabela de pipeline.
- [x] Adicionar o botão de análise com IA e serializar as duas tabelas.
- [x] Mostrar o modelo inicial antes das mensagens e preservar sessões antigas.
- [x] Adicionar traduções, testes, documentação e validação de build.

## Verificação

- Rust: testes focados de `tp` aprovados.
- Frontend: testes de análise, store e `ExecutionTelemetry` aprovados.
- Frontend: `npm run build` aprovado.
- A validação final deve confirmar `git diff --check`, formatação dos arquivos
  Rust tocados e os testes completos disponíveis no ambiente.
