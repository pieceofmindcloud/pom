# Plano de recuperação do QUIC após reinício do peer

> **Para agentes:** use `superpowers:subagent-driven-development` ou
> `superpowers:executing-plans` para implementar este plano por tarefa. As
> etapas usam caixas de seleção (`- [ ]`) para acompanhamento.

**Objetivo:** recuperar imediatamente o transporte de inferência entre hops
quando uma conexão QUIC cacheada morre porque o peer remoto reinicia, mantendo
o fallback limitado para endpoints realmente indisponíveis.

**Arquitetura:** manter um endpoint QUIC por processo, mas tratar a falha de
uma requisição numa `QuicSession` cacheada como evidência de que somente aquela
sessão está velha. Fechar e remover a sessão, repetir um handshake novo na
mesma requisição e quarentenar somente a falha de um endpoint novo. Manter a
escada existente de fallback TCP e túnel, com testes que comprovem que ela
continua utilizável durante a recuperação após reinício.

**Stack:** Rust, Tokio, Quinn, h3, Axum, reqwest e testes do Cargo.

**Especificação:** brief de lançamento do firstmate para
`rdma-quic-reset-apos-restart-peer`.

## Restrições globais

- Preservar o orçamento agregado de descoberta QUIC de 300 ms e a quarentena de
  5 s para endpoints cujo handshake novo falhar.
- Nunca reutilizar uma `QuicSession` depois de um erro de transporte da
  requisição.
- Manter o caminho TCP disponível sem intervenção do operador.
- Atualizar as instruções de rede e a documentação do subsistema P2P em pt-BR
  quando o comportamento mudar.

### Tarefa 1: reproduzir a falha da sessão cacheada

**Arquivos:**
- Modificar: módulo de testes de `crates/node/src/network/quic.rs`
- Testar: `crates/node/src/network/quic.rs`

- [x] **Etapa 1: adicionar um teste de regressão de reinício** que inicia um
  servidor HTTP/3, completa uma requisição, para e reinicia o servidor na mesma
  porta UDP e exige que o mesmo `ClientPool` complete a próxima requisição.
- [x] **Etapa 2: executar o teste focado** e confirmar a falha porque a sessão
  cacheada é removida/quarentenada sem um handshake novo nessa requisição.

### Tarefa 2: repetir uma vez a sessão cacheada velha

**Arquivos:**
- Modificar: `crates/node/src/network/quic.rs`

- [x] **Etapa 1: adicionar um helper que fecha e remove uma sessão** pela chave
  exata do cache, mantendo o mapa de quarentena para falhas de endpoints novos.
- [x] **Etapa 2: fazer `request` distinguir falha de sessão cacheada de falha
  de handshake novo e repetir uma vez a sessão cacheada com handshake novo.
- [x] **Etapa 3: preservar a quarentena existente e o deadline agregado** quando
  o handshake novo também falhar.
- [x] **Etapa 4: executar a regressão de reinício e os testes QUIC existentes.**

### Tarefa 3: verificar o fallback e documentar o ciclo de vida

**Arquivos:**
- Modificar: testes de `crates/node/src/network/quic.rs`, se necessário
- Modificar: `crates/node/src/network/AGENTS.md`
- Modificar: `docs/subsystems/rede-p2p.md`

- [x] **Etapa 1: adicionar um teste de regressão** provando que uma sessão QUIC
  morta chega ao endpoint de inferência TCP direto.
- [x] **Etapa 2: atualizar as instruções de rede e a documentação P2P** com o
  comportamento de substituição da sessão e da quarentena.
- [x] **Etapa 3: executar formatação, testes focados e a suíte relevante do nó
  antes da entrega.**

- [x] **Etapa 4: reler o diff e preparar o commit da branch validada.**
