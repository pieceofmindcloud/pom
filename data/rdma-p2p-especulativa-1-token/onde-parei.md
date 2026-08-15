# Onde parei - rdma-p2p-especulativa-1-token

**Estado: a tarefa esta completa e comitada.** Parei por ordem de cota, nao por
bloqueio. Nada em stash, nada nao comitado.

- Branch: `fm/rdma-p2p-especulativa-1-token`
- Commit: `2dc89d0 fix(especulativa): a recusa de rascunhar e resposta, nao erro`
- Arvore limpa (`git status` vazio). Sem push, sem PR, como manda o contrato.

## Causa raiz (uma linha)

Quem pede a rodada especulativa nao sabia ler a resposta que o proprio protocolo
manda quando a cabeca recusa rascunhar (`kv_can_shift == false` ->
`should_draft` diz nao -> `draft` omitido -> a cauda responde no formato antigo
de um token), e tratava essa resposta legitima como erro fatal.

## O que ja fez

1. **Reproducao antes da correcao.** Bancada nova `crates/node/src/inference/spec_lab.rs`:
   `gateway::generate_stream` de verdade contra dois hops falsos que decidem com
   `should_draft`/`find_draft`/`longest_match` **de producao**, com `kv_can_shift`
   como botao. Com o botao em `false` ela reproduziu o log do captain linha por
   linha:

   ```
   INFO  Pipeline planejado: 2 hops [hop0->hop1] session=... max_tokens=5 resposta=direta
   INFO  prompt ingerido em 1 pedaco(s) de 512 tokens (6 tokens) [resposta direta]
   INFO  prompt de 6 tokens em 1 pedaco(s) - 0.0s ate a primeira palavra
   WARN  Erro na rodada especulativa (token 1): ultimo peer nao devolveu nenhum token aceito
   INFO  devolvendo 1 tokens parciais
   ```
   `tokens_generated: 1`, `partial: true`, `decode.calls == 0` nos dois hops.

2. **Refutacao da hipotese da resposta direta (07e2118), com evidencia.**
   `step_speculative` nunca recebeu o `DirectReply` - o gateway nem passa o
   argumento -, entao o caminho e o mesmo com e sem resposta direta. O teste
   `o_anel_borbulhado_se_comporta_igual_ao_modo_direto` guarda o contrafactual:
   o defeito aparecia identico no anel borbulhado.

3. **Correcao dirigida a causa raiz** em `prefill::speculative_step_from_meta`:
   passa a aceitar as **duas** formas legitimas de resposta. A forma antiga vira
   uma rodada de largura 1 (`accept_count == batch_width == 1`, nada a
   rebobinar - a justificativa do invariante esta no doc da funcao).

4. **A metade de desempenho**, porque insistir custava caro e em silencio:
   recusada a especulacao, `generate_stream` para de pedir pelo resto da
   requisicao (`especulando`) e passa a chamar `prefill::step`, que usa a
   resposta direta (N+1 travessias) em vez do anel que `step_speculative` e o
   unico a conhecer (2N).

5. **Testes de regressao** (a tarefa anterior entrou sem cobertura; esta nao):
   - `spec_lab.rs`: 5 testes de ponta a ponta pelo `generate_stream`.
   - `prefill.rs` `mod tests`: 4 testes de unidade das duas formas de resposta.
   - 370 -> 379 testes. Verificado que 3 dos 5 testes do lab **falham** quando a
     correcao e revertida pontualmente.

6. **AGENTS.md** (== CLAUDE.md): secao nova "Uma rodada especulativa tem duas
   respostas legitimas", e a nota estagnada sobre `step_speculative` corrigida.

## Verificacao

- `cargo test`: **379 passam**, 0 falham, 3 ignorados.
- `cargo clippy --all-targets`: **0 erros**, 66 avisos - exatamente a linha de
  base do branch (conferido com `git stash`). Nenhum aviso novo.
- Suite do frontend nao afetada: nenhum arquivo fora de `crates/node` e
  `AGENTS.md` foi tocado.

## Arquivos tocados

| arquivo | o que mudou |
|---|---|
| `crates/node/src/inference/prefill.rs` | a correcao: `speculative_step_from_meta` le as duas formas; `SpeculativeStep::speculated` e `::of_one_token`; 4 testes |
| `crates/node/src/backend/gateway.rs` | `especulando` no laco de decode; mensagens de log honestas quanto ao modo; helpers de teste `spec_config` / `with_self_reachable_gateway` |
| `crates/node/src/inference/spec_lab.rs` | **novo** - a bancada e os 5 testes |
| `crates/node/src/inference/infer_server.rs` | `should_draft`/`find_draft`/`longest_match` viraram `pub(crate)` para a bancada usar as funcoes de producao |
| `crates/node/src/inference/runtime.rs` | `seed_config_for_test` (so em teste) |
| `crates/node/src/main.rs` | registra `mod spec_lab` |
| `AGENTS.md` | a secao nova |

## O que falta

Nada para fechar a tarefa. O que fica em aberto, para quem pegar depois:

1. **Confirmar no cluster de verdade.** Nao consegui alcancar a ksia
   (`192.168.100.138:8181` e `:8090` sem resposta desta maquina), entao o motor
   llama.cpp nao entrou na prova. O passo exato:
   `curl -s http://<peer>:8181/health | jq .kv_can_shift` nos dois nos - a
   previsao desta analise e **`false`**. Se vier `true` nos dois, a explicacao
   aqui nao cobre o caso do captain e a investigacao precisa recomecar do trace.
   Depois: uma requisicao real a `/v1/chat/completions` com stream tem que dar
   texto completo, `partial: false` e `decode.calls > 0` nos dois hops.
2. **`step_speculative` continua so no anel borbulhado.** Estende-lo a resposta
   direta deixaria de valer a pena so para modelos que de fato especulam; com
   esta correcao um modelo que nao especula ja nao paga o anel. Fica como
   trabalho separado, nao como pendencia desta.
