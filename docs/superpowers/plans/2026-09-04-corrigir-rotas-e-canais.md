# Corrigir rotas de retorno, entrega direta e canais por aresta - Plano de Implementação

> **Para agentes:** SUB-SKILL OBRIGATÓRIA: use `superpowers:executing-plans` para executar este plano tarefa por tarefa. Os passos usam checkbox (`- [ ]`) para acompanhamento.

**Goal:** Corrigir as rotas de retorno, a entrega direta, o stream remoto, o pool de canais e a leitura de `/health` sem alterar o Dockerfile ou o llama.cpp vendorizado.

**Architecture:** A memória de falhas continuará reordenando candidatos, nunca removendo todos. A entrega direta usará falhas consecutivas para quarentena e um teto longo para o último candidato. O pool manterá somente operações rápidas sob o mutex global e usará um mutex de abertura por chave, permitindo que arestas independentes abram em paralelo. `/health` capturará o diretório de slots fora do lock do motor.

**Tech Stack:** Rust, Tokio, Axum, reqwest, testes `cargo test -p node --bin pom`, documentação Markdown em pt-BR.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-revisao-2-dias/report.md`

## Global Constraints

- Partir e permanecer na worktree `/Users/jairpereira/.treehouse/rdma-p2p-242c70/3/rdma-p2p`.
- Integrar em `origin/feat/implantar3`, preservando alterações de outros trabalhadores; nunca usar `--force`.
- Não tocar na máquina ksia, no Dockerfile, em `FIT_MAX_UBATCH` ou no llama.cpp vendorizado.
- Não modificar `CHANGELOG.md` nem cópias geradas.
- Atualizar o `AGENTS.md` de cada pasta alterada e `docs/subsystems/inferencia.md` no mesmo commit.
- Registrar no handoff a medição anterior de 609,55 s e a medição nova da suíte, incluindo todos os vermelhos que permanecerem.

---

### Task 1: Fechar o contrato das rotas e do stream direto

**Files:**
- Modify: `crates/node/src/network/peer_client.rs:86-365` e testes locais do módulo
- Modify: `crates/node/src/inference/direct.rs:64-805` e testes locais do módulo
- Modify: `crates/node/src/inference/infer_server.rs:6650-6720` e testes `remote_stream_tests`
- Test: os testes unitários dos três módulos e `direct_lab`/`link_lab` afetados

**Interfaces:**
- `PeerClients::route_order(&str, &[String]) -> Vec<String>` sempre devolve todos os candidatos distintos quando a entrada não é vazia.
- `PeerClients::remember_route_failure` conta falhas consecutivas e `remember_route_success` zera a conta.
- `direct::deliver_stream_batch` devolve `direct::Delivery`, distinguindo aceito, `410 Gone` e falha de transporte.
- `emit_remote_stream_batches` entrega um lote terminal de erro quando a entrega do lote normal falha.

- [x] **Step 1: Confirmar os testes de regressão herdados e acrescentar a prova de entrega lenta viva**

  Manter os testes já presentes para lista totalmente em quarentena, rota preferida, falha isolada, entrega a `410` e stream. Acrescentar em `direct.rs` um teste que mantenha um único endpoint vivo, faça-o responder depois de 200 ms e verifique `Delivery::Accepted` e `route_failure_streak == 0`; o caso deve usar o último candidato para exercitar o teto longo.

- [x] **Step 2: Rodar os testes direcionados antes de qualquer nova alteração de produção**

  Executar:

  ```bash
  rtk cargo test -p node --bin pom peer_client::tests -- --nocapture
  rtk cargo test -p node --bin pom inference::direct::tests -- --nocapture
  rtk cargo test -p node --bin pom inference::infer_server::remote_stream_tests -- --nocapture
  ```

  Esperado: os testes herdados de rota e stream passam; o novo teste de entrega lenta deve passar com a implementação já deixada pelo trabalhador pausado. Se algum teste falhar, corrigir somente a causa desse teste antes de continuar.

- [x] **Step 3: Acrescentar a prova de erro explícito no stream**

  Usar uma closure de entrega que devolve `Delivery::Failed` no primeiro lote e aceita o segundo. Consumir o segundo lote e verificar que ele contém `error` e não tokens. Isso fixa que o emissor não retorna silenciosamente depois de uma falha conhecida.

- [x] **Step 4: Rodar as regressões de caminho real**

  Executar:

  ```bash
  rtk cargo test -p node --bin pom direct_lab::the_cost_per_token_does_not_depend_on_where_the_entry_point_is -- --nocapture
  rtk cargo test -p node --bin pom direct_lab::direct_prefill_preserva_posicao_absoluta_com_e_sem_reuso -- --nocapture
  rtk cargo test -p node --bin pom link_lab::canal_indisponivel_degrada_para_o_caminho_por_requisicao -- --nocapture
  ```

  Esperado: os dois `direct_lab` associados ao defeito de rota não esperam o deadline e o fallback HTTPS do `link_lab` não termina com motivo vazio.

- [x] **Step 5: Commitar somente se a tarefa for isolável**

  Se os testes estiverem verdes, revisar o diff herdado para não perder alterações do trabalhador pausado. O commit final será feito após as Tasks 2 e 3 para que código e documentação sejam publicados juntos.

### Task 2: Abrir canais persistentes sem travar arestas independentes

**Files:**
- Modify: `crates/node/src/inference/link.rs:80-445` e testes locais
- Modify: `crates/node/src/network/peer_client.rs` apenas se for necessário expor a constante de cooldown já usada pela rota
- Modify: `crates/node/src/inference/AGENTS.md` na descrição de `link.rs` e das entregas
- Test: novo teste em `crates/node/src/inference/link.rs` para duas arestas simultâneas

**Interfaces:**
- `Pool` preserva o mapa global de canais, mas `channel_for` só segura esse mapa para verificar/inserir canais.
- Uma tabela `opening` fornece um `Arc<tokio::sync::Mutex<()>>` por `target_key`, deduplicando a abertura concorrente da mesma aresta sem serializar outras chaves.
- `OPEN_FAILURE_COOLDOWN` é igual a `crate::peer_client::ROUTE_FAILURE_COOLDOWN`.

- [x] **Step 1: Escrever o teste vermelho de isolamento entre arestas**

  Subir um listener de loopback que aceita a abertura da aresta morta e não envia cabeçalhos, esperando o timeout de abertura. Depois de confirmar a aceitação, iniciar `send_batch` numa segunda aresta saudável e exigir que ela complete antes de `LINK_OPEN_TIMEOUT`. A tarefa morta deve terminar em erro e a saudável deve abrir o canal e responder o quadro.

- [x] **Step 2: Rodar somente o teste novo para confirmar a falha no desenho atual**

  Executar:

  ```bash
  rtk cargo test -p node --bin pom inference::link::tests::abertura_de_aresta_morta_nao_bloqueia_outra -- --nocapture
  ```

  Esperado antes da correção: falha por timeout ou por a aresta saudável esperar a abertura morta sob o mutex global.

- [x] **Step 3: Implementar o guarda por aresta**

  Obter o lock de abertura por `target_key`, reconsultar canal e quarentena depois de adquiri-lo, executar `open(...)` fora do lock de `channels`, e inserir/remover/quarentenar sob locks curtos. Usar a constante de 300 s compartilhada com a memória de rotas. Não alterar a semântica de recusa HTTP: recusa de quadro continua final e falha de transporte continua fallback.

- [x] **Step 4: Rodar o teste vermelho agora verde e a suíte de link**

  Executar:

  ```bash
  rtk cargo test -p node --bin pom inference::link::tests::abertura_de_aresta_morta_nao_bloqueia_outra -- --nocapture
  rtk cargo test -p node --bin pom inference::link::tests -- --nocapture
  rtk cargo test -p node --bin pom link_lab -- --nocapture
  ```

  Esperado: a aresta saudável completa durante a abertura morta, a mesma aresta ainda abre no máximo um canal e todas as provas de multiplexação, recusa e fallback passam.

### Task 3: Tirar a leitura de slots de dentro do lock de inferência

**Files:**
- Modify: `crates/node/src/inference/infer_server.rs:2070-2110,3470-3610`
- Test: teste unitário do contador de entradas de slot e testes existentes de `/health`
- Modify: `crates/node/src/inference/AGENTS.md` para dizer que `/health` conta entradas fora do lock do motor

**Interfaces:**
- `count_slot_entries(&Path) -> usize` conta sidecars `.json` de snapshot sem abrir nem parsear o conteúdo.
- `handle_health` captura `slot_dir`, libera o `InferState`, calcula a contagem e só então monta a fotografia do motor.

- [x] **Step 1: Escrever teste vermelho para contar sem parsear JSON**

  Criar diretório temporário com um `.json` de snapshot corrompido, `prefix-auto-index.json` e um arquivo não JSON. Exigir que `count_slot_entries` retorne `1`, provando que o contador é estrutural e não lê/parsa cada sidecar.

- [x] **Step 2: Rodar o teste para confirmar a ausência do helper**

  Executar:

  ```bash
  rtk cargo test -p node --bin pom inference::infer_server::tests::slot_count_usa_somente_entradas_do_diretorio -- --nocapture
  ```

  Esperado: falha de compilação por helper ausente ou falha da asserção antes da implementação.

- [x] **Step 3: Implementar a leitura fora do lock**

  Extrair o `slot_dir` numa seção curta protegida por `lock_inference_state`, liberar o guard, chamar `count_slot_entries`, e depois adquirir a fotografia necessária para o JSON de saúde. Substituir `list_slots(dir).len()` por esse contador e manter o índice automático fora da contagem.

- [x] **Step 4: Rodar a regressão de health e compilar o módulo**

  Executar:

  ```bash
  rtk cargo test -p node --bin pom inference::infer_server::tests -- --nocapture
  rtk cargo test -p node --bin pom health -- --nocapture
  ```

  Esperado: o teste estrutural passa e as respostas de `/health` conservam os campos existentes.

### Task 4: Atualizar documentação viva e anotações de defeitos

**Files:**
- Modify: `docs/subsystems/inferencia.md:522-570,1825-1910`
- Modify: `crates/node/src/inference/AGENTS.md` nas descrições de `link.rs`, `deliver_result`, stream e `/health`
- Modify: `crates/node/src/network/AGENTS.md` na descrição de `peer_client.rs`/rota

**Interfaces:**
- A documentação deve refletir 300 s como cooldown compartilhado, abertura fora do lock global, entrega lenta separada de quarentena, erro explícito de stream e contagem de slots sem parse sob o mutex.

- [x] **Step 1: Remover anotações já corrigidas e registrar o estado atual**

  Em `§11`, remover o vermelho de `pipeline::ipv6_tests::f8_is_refused...`, adicionar `link_lab::canal_indisponivel_degrada_para_o_caminho_por_requisicao`, e corrigir a causa/sintoma dos dois `direct_lab` para a memória de rotas que esvaziava candidatos e para o stream que encerrava sem erro. Remover a entrada stale do `llama-engine` que o relatório prova verde somente se ela ainda estiver presente nessa seção.

- [x] **Step 2: Atualizar os guias de pasta**

  Descrever em `inference/AGENTS.md` que o canal usa guarda por aresta, que o último candidato pode usar teto longo, que falhas consecutivas e sucesso controlam a memória de rota, e que o stream entrega lote terminal de erro. Descrever em `network/AGENTS.md` que `route_order` nunca esvazia uma entrada não vazia e que a quarentena é reordenação, não exclusão.

- [x] **Step 3: Conferir a documentação contra o código**

  Executar:

  ```bash
  rtk rg -n "30 s|30 segundos|lista vazia|motivo vazio|IPv6|fit_mode_caps|canal_indisponivel|direct_prefill|slot_count|global" docs/subsystems/inferencia.md crates/node/src/inference/AGENTS.md crates/node/src/network/AGENTS.md
  ```

  Corrigir somente referências contraditórias introduzidas ou afetadas por esta tarefa.

### Task 5: Verificação final, medição e publicação

**Files:**
- Modify: nenhum arquivo adicional além dos anteriores
- Test: suíte completa do binário `pom` e testes dos crates afetados

- [x] **Step 1: Rodar formatação e verificações direcionadas**

  Executar `rtk cargo fmt --all -- --check` e repetir os testes direcionados das Tasks 1 a 3.

- [x] **Step 2: Medir a suíte exigida**

  Medir com `/usr/bin/time -p rtk cargo test -p node --bin pom -- --skip prefix_reuse_lab`, registrando o tempo real e a lista completa de testes vermelhos. A linha de base comprovada pelo relatório é `609,55 s`, `1022` verdes, `8` vermelhos.

- [x] **Step 3: Repetir a medição de isolamento do pool**

  Executar o teste de duas arestas e registrar o tempo da aresta saudável enquanto a abertura blackhole aguarda. O critério é completar antes do timeout de abertura, em vez dos `1,95 s` de parada global medidos no desenho anterior.

- [x] **Step 4: Rodar as verificações amplas**

  Executar `rtk cargo test -p node --bin pom`, `rtk cargo test -p llama-engine` e `rtk cargo test --workspace` se o tempo e os recursos permitirem. Não esconder testes vermelhos não relacionados: reportar cada nome e a razão baseada no relatório ou na execução atual.

- [x] **Step 5: Revisar, commitar e rebasear antes de publicar**

  Conferir `rtk git diff`, preservar todas as mudanças preexistentes, commitar sem co-autor de agente, atualizar a branch a partir de `origin/feat/implantar3` com rebase sem descartar commits e publicar sem `--force` conforme a ordem do captain. Após a publicação, registrar no status uma única linha `done:` com `<antes>s -> <depois>s` e `<vermelhos antes> -> <vermelhos depois>`.
