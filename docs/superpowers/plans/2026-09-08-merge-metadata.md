# Fusão de metadata por camada - Plano de Implementação

> **Para workers agenticos:** REQUIRED SUB-SKILL: Use superpowers:executing-plans para executar este plano task-by-task.

**Objetivo:** Fazer a fusão de fatias reconstruir arrays de metadata por camada para a cobertura unida, validar o GGUF antes de publicar a parte e o manifesto.

**Arquitetura:** O `splitter::merge` vai interpretar cada array escalar dentro do namespace da arquitetura como dados locais da fatia, concatenar a cobertura declarada por `MergePart` e preservar o sufixo que fica depois do bloco local. Arrays globais permanecem byte a byte da primeira fatia; contagens escalares continuam sendo reescritas segundo suas semânticas existentes. `webui::merge_parts` chama `gguf::read_layout` no arquivo temporário antes do rename e antes de `finalize_run`.

**Stack:** Rust, GGUF v3, `cargo test -p node`, `memmap2`, `tokio::task::spawn_blocking`.

**Spec:** `data/rdma-vermelho-webui-fatias/report.md` (relato fornecido na tarefa; ausente neste checkout).

## Restrições globais

- Manter `llama.cpp` vendorizado intocado.
- Manter `FIT_MAX_UBATCH` em 512.
- Manter `webui::tests::merge_parts_joins_two_slices_into_a_whole` e o total esperado de 32 camadas.
- Provar a fusão com arquivos produzidos por `split`, não com fatias montadas manualmente.
- Validar com `read_layout` antes de materializar a parte final e o manifesto.
- Publicar sem PR em `origin/feat/implantar3`, sem force push e sem tocar `main`.

### Task 1: Regressão real da fusão

**Arquivos:**
- Modificar: `crates/node/src/backend/webui/tests/core_a.rs`, teste `merge_parts_joins_two_slices_into_a_whole`
- Referência: `crates/node/src/models/splitter.rs`, `split`, `test_gguf_bytes`

**Interface produzida:** O teste escreve um GGUF inteiro de 32 blocos, chama `split` para `0..=15` e `16..=31`, entrega os dois caminhos reais a `merge_parts`, chama `read_layout` no resultado e afirma `block_count=32`, `layer_count=32`, `head_count_kv_by_layer` com 32 valores e `sliding_window_pattern` com 32 valores.

- [x] Reescrever o helper `write_part` para receber o GGUF inteiro e chamar `crate::splitter::split` em cada faixa antes de mover a saída para a pasta da parte.
- [x] Acrescentar asserções literais para os arrays: `[0..=31]` em `head_count_kv_by_layer` e `[true, false, ...]` com 32 posições em `sliding_window_pattern`, além do `read_layout` existente.
- [x] Rodar `rtk cargo test -p node merge_parts_joins_two_slices_into_a_whole -- --exact --nocapture` e confirmar que falha porque a fusão deixa o array com o comprimento da primeira fatia.

### Task 2: Reconstrução semântica da metadata

**Arquivos:**
- Modificar: `crates/node/src/models/splitter.rs`, parser e escritor de KVs de `merge`
- Teste: `crates/node/src/backend/webui/tests/core_a.rs`

**Interface produzida:** Um helper interno de merge recebe uma chave, as entradas `KvOut` de todas as fatias, o intervalo local de cada fatia e o total unido, e devolve um único `KvOut::Encoded` quando a chave é um array escalar por camada. Ele concatena somente os elementos cobertos por `start_layer..=end_layer` de cada entrada e anexa uma cauda comum que esteja depois do `block_count` local. Chaves fora do namespace da arquitetura, arrays rope e arrays globais seguem `Raw` da primeira fatia.

- [x] Estender a representação de KV parseado para expor tipo, elemento, contagem e payload de arrays sem copiar os bytes de pesos.
- [x] Classificar arrays com a mesma regra de `read_layout`: namespace da arquitetura, exclusão de rope, array escalar e comprimento 1 como broadcast.
- [x] Para arrays por camada, verificar que cada fatia contém pelo menos o intervalo local, concatenar os valores locais na ordem de `parts` e preservar uma cauda de elementos especiais uma única vez.
- [x] Reescrever `block_count`, `pom.*`, `start_layer`, `end_layer` e `attention.recurrent_layers` sem duplicatas; manter as semânticas `FromStart` e `FromEnd` para contagens escalares.
- [x] Rodar o teste regressivo e `rtk cargo test -p node splitter::tests::merged_slices_round_trip_into_a_whole -- --exact --nocapture`.

### Task 3: Barreira de publicação e documentação

**Arquivos:**
- Modificar: `crates/node/src/backend/webui/jobs_apply.rs`, `merge_parts`
- Modificar: `crates/node/src/models/AGENTS.md`
- Modificar: `crates/node/src/backend/AGENTS.md` se a regra de publicação ainda não estiver descrita

**Interface produzida:** O arquivo temporário só vira parte endereçada por conteúdo depois de `crate::gguf::read_layout(&temp)` retornar sucesso. Qualquer erro remove o temporário e impede `finalize_run`.

- [x] Executar `read_layout` dentro do mesmo `spawn_blocking`, imediatamente depois de `splitter::merge` e antes do `part_path`/rename.
- [x] Preservar a limpeza do temporário em toda falha, incluindo erro de layout.
- [x] Documentar nas instruções da pasta que arrays GGUF por camada são reconstruídos na fusão e que a validação de layout precede o manifesto.
- [ ] Rodar `rtk cargo test -p node` uma vez sem filtro, revisar `rtk git diff --check` e confirmar que não há arquivos vendorizados alterados.
- [ ] Commitar, fazer `rtk git pull --rebase origin feat/implantar3`, resolver somente conflitos reais sem descartar commits e fazer push sem `--force`.
