# Plano: recarga apos descarregamento por ociosidade

**Spec:** briefing da tarefa sobre travamento total durante a recarga de um
modelo descarregado por ociosidade.
**Data:** 2026-09-14

## Objetivo

Garantir que o ciclo `Ready -> Idle -> recarga -> Ready` nao bloqueie o
executor Tokio nem deixe a geracao anterior competir com a nova. O teste deve
provar, com um dublê deterministico, que o HTTP continua atendendo durante a
liberacao nativa e que a inferencia volta a ser atendida dentro de um timeout.

## Estrategia

- [x] Reproduzir o ciclo com o servidor local e instrumentacao de teste para
  distinguir uma requisicao pendurada de um executor inteiro bloqueado. O
  teste HTTP em runtime single-thread ficou vermelho antes do fix: `llama_free`
  sincrono impedia ate `/health` de ser atendido.
- [x] Isolar a limpeza FFI sincrona fora de tarefas async e coordenar o fim da
  geracao anterior antes de iniciar a nova, preservando a serializacao por
  operacao.
- [x] Adicionar teste de regressao do ciclo idle-reload-inference, executar a
  cobertura de runtime/gateway e atualizar a documentacao dos contratos.
- [x] Rebasear na ponta atual de `origin/feat/implantar3` e validar. O commit e
  a publicação ficam para a etapa de entrega depois da revisão.

## Criterios de aceite

- `/health` responde durante a limpeza bloqueante simulada.
- Uma requisicao apos `Idle` recarrega o modelo e processa a inferencia dentro
  de um timeout razoavel.
- O encerramento de uma geracao nunca executa a liberacao nativa diretamente no
  executor async.
- Testes e build relevantes passam, e a arvore fica limpa no branch de entrega.
