# Tensor parallelism customizado (caminho C) - Plano de Implementação

**Goal:** tensor parallelism sem transferir peso: cada worker extrai o próprio
shard de tensor da cópia local do GGUF; a execução troca só ativações
(all-reduce). Exige forkar o llama.cpp, confirmado no spike.

**Architecture:** o spike leu ggml/src/ggml-backend.cpp e provou que o único hook
(ggml_backend_sched_eval_callback) é somente leitura: lê tensor e pode ABORTAR o
grafo, mas não injeta nó nem devolve valor ao meio do grafo. Logo o caminho C tem
uma parte central que é um fork do llama.cpp (fases 0 e 3) e partes independentes
que podem andar em paralelo (fases 1, 2, 4, 5).

**Tech Stack:** Rust (tokio, axum, serde, reqwest), C/C++ (fork do llama.cpp +
shim), React + TypeScript + Tailwind (apps/frontend), testes com cargo test e
vitest.

**Spec:** docs/superpowers/specs/2026-08-17-tensor-parallelism-design.md

## Global Constraints

- Nunca use travessão em texto. Use hífen. Vale para código, comentários, commits e interface.
- Comentários em português nos arquivos que já usam português; em inglês nos que já usam inglês.
- Todo campo novo de mensagem de rede leva #[serde(default)].
- O wire de estado oculto não muda; TP é transparente para fora do estágio.
- O split de tensor respeita blck_size do tipo ggml; tipo desconhecido é recusa.
- Depois de editar apps/frontend, rode npm run build lá dentro.
- O fork do llama.cpp é a dependência crítica: sem ele, o caminho C não existe.

## Estrutura de arquivos (visão geral)

**Criar:**
- vendor/llama.cpp/ (fork vendado) + patch de TP.
- crates/node/src/models/shard.rs (shard de tensor por linhas/colunas).
- crates/node/src/inference/collective.rs (all-reduce ring/tree/centralizado).
- crates/node/src/inference/tp_shim.cpp (ponte para o op de redução no fork).

**Modificar:**
- crates/node/src/models/splitter.rs (produzir N GGUFs de shard).
- crates/node/src/models/gguf.rs (tabela de tipos já existe; metadata pom.tp_*).
- crates/node/src/inference/{llm.rs,runtime.rs,infer_server.rs} (worker/primário).
- crates/node/src/network/network.rs, routing/pipeline.rs (papel TP, grupo, rank).
- crates/routing-contract/src/lib.rs (campos TP no PeerCapacity).
- crates/node/src/backend/{deploy.rs,webui.rs}, main.rs (CLI), apps/frontend/.
- Documentação e AGENTS.md afetados.

---

# Fase 0: fork do llama.cpp (a incógnita central)

- [ ] Vender o llama.cpp no repositório (vendor/) e trocar o llama-cpp-sys-2 pela
  árvore vendada, ou patchear a crate para compilar a nossa árvore.
- [ ] Op/backend ggml de redução em rede (all-reduce) como nó do grafo.
- [ ] Patch em build_llama (src/llama-graph.cpp): inserir a redução após wo e
  ffn_down, atrás de uma flag de tensor parallelism.
- [ ] Patch no loader: carregar GGUF com tensor parcial + metadata pom.tp_rank.
- [ ] Gate: teste que falha se o fork não estiver ativo.

# Fase 1: splitter de shard de tensor (independente)

- [x] Classificação row/column/não-shardado por arquitetura (Llama primeiro).
- [x] splitter.rs produz N GGUFs de shard (row/column split respeitando
  blck_size, metadata pom.tp_rank/pom.tp_size).
- [x] Testes com o GGUF sintético compartilhado (test_gguf_bytes).

# Fase 2: coletivas all-reduce na rede (independente)

- [x] collective.rs: reduce_sum + ReduceBarrier (barreira centralizada). O anel/
  árvore e o transporte wire.rs ficam como otimização junto do fork.
- [x] Testes em memória (tokio): soma alinhada, todos os ranks recebem a soma,
  rank duplicado e tamanho divergente são recusados.

# Fase 3: motor worker + primário TP (depende do fork)

- [ ] Primário carrega o GGUF shardado e executa com a flag TP + redução em rede.
- [ ] Worker expõe o op de redução e não carrega GGUF inteiro (só o shard).
- [ ] Regressão: resposta bate com o modelo inteiro numa máquina só.

# Fase 4: anúncio e roteamento de grupo TP

- [x] Announce/PeerInfo ganham TpIdentity (role, group_id, size, rank), tudo
  serde(default).
- [x] plan_pipeline exclui workers (PeerInfo::is_tp_worker); um hop = o primário.

# Fase 5: implantação e UI

- [ ] DeployKind::TensorParallel completo: survey/plan/launch montando o grupo
  primary + K workers. (O enum + nomeação já entrou; a descoberta de workers e
  a tela ficam junto do fork.)
- [x] webui::apply: worker extrai o shard local (plan::extract_tp_shard) e
  anuncia o grupo (Network::set_tp).
- [ ] Tela: escolher formato pipeline ou TP, desenhar o grupo no canvas.

# Fase 6: documentação, testes finais e observação

- [x] docs/subsystems/{fatiamento,inferencia,implantacao,rede-p2p}.md e AGENTS.md
  de inference/, network/, routing/. Falta build-execucao.md (o fork) e backend/.
- [ ] cargo test e cargo test --workspace verdes.
- [ ] Medir tok/s, memória por máquina e bytes de ativação por token, nos
  formatos pipeline puro, TP puro e misto.
