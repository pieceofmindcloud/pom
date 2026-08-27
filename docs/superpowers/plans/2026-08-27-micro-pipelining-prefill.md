# Micro-pipelining do prefill Implementation Plan

**Goal:** transformar a janela de sobreposição do prefill num pipeline em ordem
de profundidade configurável, mantendo ordem de resultado e limite de memória,
e recuperar um pedaço perdido sem reenviar o prompt inteiro.

**Origem:** cartão MICRO-PIPELINING (https://trello.com/c/NhVsA3dR) e o
relatório de investigação `MICRO-PIPELINING.md` na raiz (não commitado). O
relatório concluiu que a janela antiga (`join_ordered`, teto 2) só sobrepunha
*um estágio de cálculo* atrás de *cálculo+rede*, o que num enlace limitado por
transmissão rende poucos por cento - o "overlap zero" observado pelo captain.

**Architecture:** em `crates/node/src/inference/prefill.rs`, `run_with_options`
(hub) e `run_direct` passam a rodar duas rotinas concorrentes na mesma task
(`tokio::try_join!`), ligadas por um `mpsc` limitado:

- **bomba da cabeça** - despacha os pedaços em série (cada um: uma perna de
  rede + o decode da cabeça), lendo `more_chunks`/`next_pos` do ACK de cada um;
  adquire um permit da janela antes de cada despacho e o repassa adiante.
- **bomba de downstream** - consome os pedaços de um `futures::stream::
  FuturesOrdered`, leva cada estado oculto pelos hops restantes e colhe os
  resultados na ordem de envio; o permit viaja dentro da future de downstream e
  só cai quando aquele pedaço termina.

`PrefillOverlapWindow` vira o limitador real: gate por contagem
(`MAX_INFLIGHT_CHUNKS = 8`, `DEFAULT_MAX_INFLIGHT_CHUNKS = 8`) mais um gate por
bytes de estado oculto em RAM no gateway (`INFLIGHT_BYTE_BUDGET = 512 MiB`,
segundo `Semaphore` com `acquire_many_owned`). `join_ordered` é removido.

Retomada: um pedaço `k > 0` que falha depois de a cadeia aceitar os anteriores
anexa um `PrefillResumable { from_chunk, start_pos, prompt_tokens,
cached_tokens }` ao erro (`anyhow` context). `run_with_options` ganha
`resume: Option<PrefillResumable>` e, com `Some`, o laço parte de `from_chunk`.
Os peers refazem o próprio KV pela reentrada de pedaço que `prefill_blocking`
já trata (`infer_server.rs:4009-4031`, `kv_remove` da cauda). O gateway
(`generate_stream_with_stops`) tenta: normal -> retomada-de-k -> retry frio
total. Sessão perdida (`is_unknown_session_error`) pula a retomada.

**Tech Stack:** Rust, Tokio mpsc/semaphore/select, `futures` 0.3
(`FuturesOrdered`), Axum, config persistida em `NodeConfig`/`RuntimeParams`;
frontend React/TS para a telemetria.

## Global Constraints

- Modo anel intocado: continua serializado pelo contrato aninhado, ignora a
  janela.
- Ordem do resultado preservada (`FuturesOrdered` entrega na ordem de push); o
  pedaço final só vira `PrefillOutcome` quando sai da frente da fila.
- Posição absoluta nunca inferida pelo índice do pedaço - sempre lida do ACK
  da cabeça.
- Nenhuma invariante nova de KV: a retomada exercita o caminho de remoção de
  cauda parcial que `prefill_blocking` já tenta e trata a recusa.
- Atualizar `crates/node/src/inference/AGENTS.md`,
  `crates/node/src/backend/AGENTS.md`, `apps/frontend/src/{routes,components}/
  AGENTS.md` e `docs/subsystems/inferencia.md` no mesmo commit que o código.
- Nunca editar `apps/frontend/src/docs` (gerado).

## Passos

- [x] 1. Teto/default/clamp da janela (8), CLI, UI `max: 8`, doc-comments;
  ajustar os testes de janela.
- [x] 2. Pipeline em ordem no modo hub (`run_with_options`): `head_pump` +
  `drain` com `mpsc` + `FuturesOrdered`; campo de trace
  `prefill_pipeline_depth_max`.
- [x] 3. Pipeline em ordem na resposta direta (`run_direct`), com `ticket` por
  pedaço e `await_result_with_ack`; remover `join_ordered` e seus testes.
- [x] 4. Gate por bytes na `PrefillOverlapWindow` (`reserve_bytes`, 512 MiB);
  o hub reserva o tamanho `f32` real do estado oculto, a resposta direta não
  retém tensor.
- [x] 5. Retomada-de-k: `PrefillResumable` + `run_with_options(resume)` +
  árvore de retry no gateway; sessão perdida vai direto ao retry frio.
- [x] 6. Telemetria + frontend: `prefill_pipeline_depth_max` no trace,
  `chatStream.ts`, `Chat.tsx`, `chat.telemetry.overlap` (en + pt-BR),
  `RuntimeParamsForm` `max: 8`.

## Testes

- `crates/node/src/inference/prefill.rs` -
  `orcamento_de_bytes_segura_um_pedaco_novo_ate_o_anterior_soltar` (gate por
  bytes, determinístico); `limite_de_pedacos_em_voo_e_respeitado` atualizado
  para o teto 8.
- `crates/node/src/inference/prefix_reuse_lab.rs` -
  `prefill_multi_pedaco_enche_o_pipeline`: com 4 hops (downstream ~3x a
  cabeça) e prompt frio de vários pedaços, `prefill_pipeline_depth_max >= 2`
  no hub e na resposta direta, e `<= 1` / sobreposição desligada no anel.
  `unknown_session_during_prefill_is_recovered` continua válido: a sessão
  perdida no pedaço 1 não vira retomada, vai ao retry frio como antes.

## Verificação

- `cargo check -p node --tests` (feito nesta máquina - o build completo do
  llama.cpp é pesado e a máquina do captain está em uso).
- `cargo test -p node inference::prefill` e
  `cargo test -p node prefix_reuse_lab` - **não rodados aqui**; rodar antes de
  confiar nos números.
- `cd apps/frontend && npm run build` - feito, passa (`tsc -b && vite build`).
- Manual em cluster: prompt grande (>= 10k tokens), 3 hops, comparar
  `trace.prefill_ms` e `trace.prefill_pipeline_depth_max` com
  `prefill_max_inflight_chunks` em 1, 2 e 8. Esperado: queda de `prefill_ms`
  aproximando `K · max(perna, cálculo)`; a estimativa de ordem 3x-5x está em
  `MICRO-PIPELINING.md` §A.4 e só fecha com essa medição.
