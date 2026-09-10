# Fase 2 - Causa da lentidao do pipeline distribuido

> **Para workers agenticos:** executar os passos em ordem e marcar cada caixa somente apos a verificacao correspondente.

**Objetivo:** medir, no modo de profiling, cada etapa por token do pipeline distribuido para separar compute, transporte, callback, espera e gaps de serializacao. Com a autorizacao do captain, a fase seguinte tambem implementa o transporte persistente e o desacoplamento limitado por fila.

**Arquitetura:** O caminho normal continua usando apenas agregados. O modo `POM_PROFILE_SYNC=1` propaga `trace_id` e `token_index` e registra eventos monotonicos locais no gateway, na cabeca remota, em cada hop e no callback. Eventos de peers diferentes nao sao subtraidos diretamente; a analise correlaciona-os por token e por identificador de frame.

**Stack:** Rust/Tokio, `serde_json`, `tracing`, `cargo test`, testes de bancada existentes de `relay`, `link`, `direct` e `prefill`.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-telemetria-pipeline-distribuido/fase2-analise-lentidao.md` e `/Users/jairpereira/Sites/firstmate/data/rdma-telemetria-pipeline-distribuido/spec-captain.md`.

## Restricoes globais

- Nao corrigir desempenho por suposicao: instrumentar, coletar e comprovar primeiro.
- A alteracao de transporte, batching e pipeline foi autorizada pelo captain depois da instrumentacao e da evidencia da fase 2.
- Nao tocar no `llama.cpp` vendorizado.
- Ignorar o executor MoE nesta inferencia e nesta analise.
- Detalhamento por token somente sob `POM_PROFILE_SYNC=1`; modo normal deve manter contadores e histogramas agregados sem sincronizacao extra no hot path.
- Relatorio deve distinguir piso fisico de RTT, espera inevitavel, custo evitavel e limitacoes da coleta.

### Tarefa 1: Mapear o caminho e fixar o contrato da evidencia

**Arquivos:**
- Ler: `crates/node/src/backend/gateway/generation.rs`
- Ler: `crates/node/src/inference/prefill/step.rs`
- Ler: `crates/node/src/inference/infer_server/step.rs`
- Ler: `crates/node/src/inference/infer_server/step_blocking.rs`
- Ler: `crates/node/src/inference/relay.rs`
- Ler: `crates/node/src/inference/link.rs`
- Ler: `crates/node/src/inference/prefill/support.rs`
- Atualizar: `data/rdma-telemetria-pipeline-distribuido/report.md`

- [x] Confirmar que o remote stream abre uma chamada de controle, que a cabeca roda `handle_step` por rodada, que o hop seguinte responde dentro de `send_frame_or_tunnel` e que cada callback volta por `REMOTE_STREAM_RESULT_PATH`.
- [x] Confirmar que `https-link` e uma conexao persistente, mas cada quadro de decode ainda e uma unidade request/reply correlacionada por `batch_id`.
- [x] Registrar no relatorio o que a instrumentacao antiga mede e o que ainda nao consegue separar.

### Tarefa 2: Adicionar teste vermelho para eventos criticos por token

**Arquivos:**
- Teste: `crates/node/src/inference/prefill/tests.rs`
- Implementacao: `crates/node/src/inference/prefill/support.rs`

**Interface:** `TelemetrySpan` ganha um nome de etapa estavel. Um helper de profiling deve aceitar `trace_id`, `request_id`, `pipeline_id`, `peer_id`, `token_index`, etapa, dominio de relogio e dois instantes locais, rejeitando intervalo invertido. O teste deve falhar antes do campo/helper existir e cobrir o contrato serializado.

- [x] Escrever e validar o contrato serializado de spans nomeados e eventos por token.
- [x] Executar testes direcionados de spans e correlacao.

### Tarefa 3: Instrumentar as fronteiras sem mudar o comportamento

**Arquivos:**
- Modificar: `crates/node/src/inference/prefill/support.rs`
- Modificar: `crates/node/src/inference/prefill/step.rs`
- Modificar: `crates/node/src/inference/infer_server/step.rs`
- Modificar: `crates/node/src/inference/infer_server/step_blocking.rs`
- Modificar: `crates/node/src/inference/relay.rs`
- Modificar: `crates/node/src/inference/link.rs`
- Modificar: `crates/node/src/backend/gateway/generation.rs`
- Modificar: `apps/frontend/src/lib/chatStream.ts`

**Interface:** Em profiling, cada rodada carrega `telemetry_token_index` e um identificador de frame. Cada fronteira acrescenta eventos locais: entrada/saida do decode, envio/recepcao do frame, inicio/fim do compute, envio/recepcao do callback, sampling e emissao para o cliente. Em modo normal nenhum vetor por token e criado.

- [x] Fazer o teste ficar verde com o menor modelo de evento.
- [x] Marcar o token no gateway antes de abrir cada rodada e no callback antes de entregar o lote.
- [x] Medir separadamente enfileiramento do callback e tempo aguardando a resposta HTTP de `deliver_stream_batch`.
- [x] Medir `send_batch_on_channel` e `send_frame_via_tunnel_with_auth` ao redor do envio e da resposta, mantendo `https-link` e `libp2p-tunnel` como transportes distintos.
- [x] Medir no servidor a entrada do request, inicio/fim de `handle_step`, inicio/fim do decode e ponto imediatamente anterior a serializacao da resposta.
- [x] Propagar eventos de downstream no `hop_timings` sem fingir sincronizacao entre relogios de peers.
- [x] Implementar callback full-duplex persistente em `/v1/link`, fila limitada e fallback HTTPS com motivo explícito.

### Tarefa 4: Verificar com testes de bancada e executar a coleta

**Arquivos:**
- Testes: `crates/node/src/inference/direct.rs`, `crates/node/src/inference/link.rs`, `crates/node/src/inference/infer_server/step.rs`, `crates/node/src/inference/prefill/tests.rs`
- Relatorio: `data/rdma-telemetria-pipeline-distribuido/report.md`

- [x] Testar correlacao, deduplicacao, ordem fora de ordem, lote de um token, lote de varios tokens e ausencia de eventos em modo normal.
- [x] Rodar as bancadas locais mais proximas do caminho E2E com profiling ativado.
- [x] Declarar a coleta E2E bloqueada quando o cluster/modelo nao estiver disponivel e nao transformar os numeros do captain em prova nova.
- [x] Calcular as medias observadas por token e comparar com o RTT de 51 ms e 807 ms; a captura E2E do captain ainda precisa ser repetida no mesmo cenario.

### Tarefa 5: Fechar relatorio e publicar somente a evidencia

**Arquivos:**
- Atualizar: `data/rdma-telemetria-pipeline-distribuido/report.md`
- Atualizar: `crates/node/src/inference/AGENTS.md` e `docs/subsystems/inferencia.md` se o contrato de telemetria mudar

- [x] Nomear a causa raiz somente com base nos eventos observados.
- [x] Separar piso fisico de RTT, custo evitavel de serializacao, callback e espera ociosa.
- [x] Implementar a recomendacao depois da autorizacao do captain e registrar a limitacao de nao haver ganho E2E local medido.
- [x] Rodar `rtk cargo check -p node`, testes focados, testes frontend afetados e `rtk git diff --check`.
- [ ] Fazer `git fetch origin`, rebasear sobre `origin/feat/implantar3` se necessario, publicar com `git push origin HEAD:feat/implantar3` e confirmar o SHA remoto.
