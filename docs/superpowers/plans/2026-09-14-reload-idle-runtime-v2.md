# Plano: recarga coordenada após descarregamento por ociosidade

**Spec:** briefing do travamento recorrente na primeira inferência depois de
um idle-unload.
**Data:** 2026-09-14

## Diagnóstico confirmado

Não existe uma barreira distribuída de reload: cada peer recebe o comando e
recarrega sua própria fatia. O bloqueio local estava no `NodeRuntime` esperando
o supervisor anterior. O servidor pai notificava a task QUIC com
`Notify::notify_waiters()`, que não guarda o sinal. Se o supervisor recebesse
o novo generation antes do primeiro poll da task QUIC, o endpoint ficava em
`endpoint.accept()` para sempre e o `JoinHandle` nunca terminava. O log de
produção parava exatamente depois do log de reconfiguração, enquanto o
gateway continuava respondendo `/health`, compatível com essa espera isolada.

## Implementação

- [x] Trocar o sinal de parada QUIC por one-shot, preservando pedidos emitidos
  antes do primeiro poll.
- [x] Limitar a espera do endpoint QUIC a 2 s e abortar a task presa.
- [x] Limitar a espera do supervisor a 5 s, preservando o handle e recusando
  sobrepor uma geração que ainda pode estar liberando recursos nativos.
- [x] Limitar o anúncio pós-reconfiguração a 1 s como operação best-effort.
- [x] Cobrir dois peers ociosos encerrando QUIC simultaneamente e supervisor
  pendente com testes locais.

## Critérios de aceite

- `/health` continua independente da espera de reload.
- Dois peers conseguem encerrar a geração anterior e avançar juntos.
- Nenhum await de encerramento ou anúncio fica sem prazo.
- Testes focados e formatação dos arquivos alterados passam.
