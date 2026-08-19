# Armazenamento, paralelismo e transferências seguras - Plano de Implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Corrigir a última fatia, limitar e visualizar o armazenamento de fatias, documentar pipeline e TP e impedir transferências por caminho arbitrário.

**Architecture:** O backend continuará sendo a autoridade para planejamento, caminhos e segurança. A configuração persistirá apenas o limite da pasta de fatias; um módulo de armazenamento calculará uso e retenção, enquanto `webui` integra isso aos endpoints existentes. O frontend enviará somente basenames GGUF resolvidos pelo catálogo do servidor.

**Tech Stack:** Rust, Axum, Tokio, serde, filesystem local, React, TypeScript, React Query, Vitest e documentação Markdown embutida na UI.

**Spec:** `docs/superpowers/specs/2026-08-19-armazenamento-paralelismo-transferencias-design.md`

## Global Constraints

- O limite de armazenamento vale somente para a pasta de fatias e tem default de 100 GB.
- Pastas de modelos inteiros nunca são apagadas automaticamente.
- O envio aceita somente arquivos `.gguf` presentes no catálogo local.
- Nenhum caminho recebido do frontend é usado diretamente para abrir arquivo.
- A última faixa de um plano é sempre o resto do modelo.
- O fork `llama.cpp` não pode ser modificado.
- Todo teste novo segue RED, GREEN e verificação completa.
- Os `AGENTS.md` das pastas modificadas e os READMEs de subsistema afetados devem ser atualizados no mesmo commit.

---

### Task 1: Fazer a última fatia fechar sempre a cobertura

**Files:**
- Modify: `crates/node/src/models/plan.rs`
- Modify: `apps/frontend/src/routes/SliceModels.tsx`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `crates/node/src/models/plan.rs` tests
- Test: `apps/frontend/src/routes/SliceModels.test.tsx`

**Interfaces:**
- `plan_by_size(layout, slices)` deve tratar o último item como `SizedSlice { target_mb: None }`, mesmo se um cliente antigo enviar um valor.
- `plan_equal(layout, parts)` deve produzir targets reais somente antes da última faixa e resto na última.
- A UI deve enviar `{}` na última posição e renderizar o último campo como texto fixo, sem `<input>`.

- [ ] **Step 1: Escrever os testes Rust que falham**

Adicionar casos em `plan.rs` que criem um layout de 10 camadas e verifiquem:

```rust
#[test]
fn the_last_size_entry_ignores_a_target_and_takes_the_rest() {
    let plan = plan_by_size(
        &layout(10, 100.0),
        &[SizedSlice::of_mb(300.0), SizedSlice::of_mb(1.0)],
    ).unwrap();

    assert_eq!(plan.ranges.last().unwrap().target_mb, None);
    assert!(plan.ranges.last().unwrap().is_rest);
    assert_eq!(plan.ranges.last().unwrap().end_layer, 9);
    assert!(plan.complete);
}

#[test]
fn equal_parts_leave_the_last_entry_as_rest() {
    let plan = plan_equal(&layout(10, 100.0), 3).unwrap();
    assert!(plan.ranges.last().unwrap().is_rest);
    assert_eq!(plan.ranges.last().unwrap().target_mb, None);
    assert_eq!(plan.ranges.last().unwrap().end_layer, 9);
}
```

- [ ] **Step 2: Rodar os testes e confirmar RED**

Run: `rtk cargo test -p node plan::tests::the_last_size_entry_ignores_a_target_and_takes_the_rest plan::tests::equal_parts_leave_the_last_entry_as_rest`

Expected: FAIL because the last finite target is still used by `plan_by_size` and `plan_equal` assigns a target to every range.

- [ ] **Step 3: Implementar o mínimo no planner**

Em `plan_by_size`, tornar `last_requested` parte da condição `take_rest`, zerar `target_mb` no `SizedRange` final e marcar `is_rest: true`. Em `plan_equal`, usar `SizedSlice::rest()` na última posição. Preservar avisos de entradas vazias quando as faixas anteriores já cobrem o modelo.

- [ ] **Step 4: Atualizar a tela para remover o input final**

Em `SliceModels.tsx`, renderizar para `i === slices.length - 1` um bloco somente leitura com `t("slice.rest")`; não renderizar `Input`, `GB` nem edição nessa linha. Os corpos de preview e start devem mapear a última entrada para `{}`. O atalho equal deve criar as linhas anteriores com GB e a última com string vazia.

- [ ] **Step 5: Adicionar teste frontend e rodar GREEN**

Adicionar teste que renderize a tela com catálogo e plano e confirme que existe um texto de resto, que a última linha não possui input de tamanho e que o POST contém `target_mb` apenas nas entradas anteriores.

Run: `cd apps/frontend && rtk npm test -- --run src/routes/SliceModels.test.tsx`

Expected: PASS.

- [ ] **Step 6: Rodar a suíte Rust do planner**

Run: `rtk cargo test -p node plan::tests`

Expected: PASS.

---

### Task 2: Criar configuração, validação e domínio de armazenamento

**Files:**
- Modify: `crates/node/src/utils/config.rs`
- Create: `crates/node/src/models/storage.rs`
- Modify: `crates/node/src/models/mod.rs` or module declaration used by `node`
- Modify: `crates/node/src/models/AGENTS.md`
- Test: `crates/node/src/models/storage.rs` tests
- Test: `crates/node/src/utils/config.rs` tests

**Interfaces:**
- `Config.shard_max_gb: f64` com default `100.0`.
- `Config::shard_max_bytes() -> u64` converte o valor validado em bytes.
- `storage::directory_usage(root: &Path) -> io::Result<StorageUsage>` soma arquivos recursivamente.
- `storage::retained_files(root: &Path, protected: &HashSet<PathBuf>) -> io::Result<Vec<StorageFile>>` ordena candidatos do mais antigo ao mais novo.
- `storage::ensure_capacity(root, max_bytes, required_bytes, protected, outputs) -> Result<RetentionReport>` remove apenas candidatos antigos antes de uma nova escrita.

- [ ] **Step 1: Escrever testes de default e rejeição de limite**

Testar desserialização de JSON antigo sem `shard_max_gb`, valor 100.0, conversão de 100 GB para bytes e recusa de zero, negativo e `NaN` no helper de validação.

- [ ] **Step 2: Rodar os testes e confirmar RED**

Run: `rtk cargo test -p node config::tests::shard_max`

Expected: FAIL because the field and helpers do not exist.

- [ ] **Step 3: Implementar o campo de configuração**

Adicionar `#[serde(default = "default_shard_max_gb")] pub shard_max_gb: f64`, `default_shard_max_gb() -> f64 { 100.0 }` e `shard_max_bytes()` com validação finita e positiva. Não alterar o formato de outros campos.

- [ ] **Step 4: Escrever testes de uso e retenção em diretório temporário**

Criar arquivos `.gguf` de tamanhos distintos, ajustar seus tempos de modificação quando possível e testar que o uso soma subpastas, que o arquivo protegido nunca é escolhido e que os candidatos são ordenados do mais antigo ao mais novo. Testar também a recusa quando `protected + outputs` já excede o limite.

- [ ] **Step 5: Implementar o módulo de armazenamento**

Usar `read_dir` recursivo, `metadata.len()`, `created()` com fallback para `modified()`, caminhos canônicos para proteção e remoção somente depois de calcular o conjunto de candidatos. Considerar arquivos GGUF e parciais de transferência como artefatos da pasta de fatias; nunca tocar nos diretórios de modelos inteiros.

- [ ] **Step 6: Rodar GREEN e formatar somente os arquivos alterados**

Run: `rtk cargo test -p node models::storage config::tests::shard_max`

Expected: PASS.

Run: `rtk rustfmt --check crates/node/src/models/storage.rs crates/node/src/utils/config.rs`

Expected: PASS.

---

### Task 3: Integrar armazenamento e validação de pastas na API

**Files:**
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/backend/gateway.rs` if the active runtime path is needed by retention
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/subsystems/build-execucao.md` only if the persisted setting needs environment documentation
- Test: `crates/node/src/backend/webui.rs` tests

**Interfaces:**
- `GET /api/ui/paths` retorna `shard_storage: { used_bytes, max_bytes, usage_pct }`.
- `POST /api/ui/paths` aceita `shard_max_gb` e recusa caminhos iguais ou sobrepostos.
- `check_dir` deve preservar os caminhos digitados, mas a validação de relação usa `canonicalize`.
- `run_slice` chama `storage::ensure_capacity` antes da primeira escrita e informa a falha no job.

- [ ] **Step 1: Escrever testes de validação de caminhos que falham**

Adicionar testes puros para: pasta de fatias igual à pasta de modelos, fatias dentro de modelos, symlink para a mesma pasta e pastas independentes. A resposta esperada é erro de validação antes de salvar a configuração.

- [ ] **Step 2: Rodar RED**

Run: `rtk cargo test -p node webui::tests::paths_reject`

Expected: FAIL because `save_paths` atualmente só verifica existência e tipo.

- [ ] **Step 3: Implementar validação canônica**

Após validar cada diretório, canonicalizar o shard e cada model dir. Recusar igualdade e `shard.starts_with(model)`. Manter a configuração anterior intacta quando qualquer entrada falhar.

- [ ] **Step 4: Adicionar uso e limite ao endpoint**

Ler `cfg.shard_max_bytes()`, chamar `storage::directory_usage` fora do lock de configuração e serializar o bloco `shard_storage`. O POST deve salvar `shard_max_gb` somente depois de validar o valor.

- [ ] **Step 5: Integrar retenção ao fatiamento**

Calcular as saídas planejadas a partir de `SizedRange`, proteger o modelo ativo quando houver configuração de runtime e chamar `ensure_capacity` antes do `tokio::spawn` ou no início do job antes de abrir o primeiro arquivo. Se não houver capacidade, marcar o job como `failed` sem criar fatias parciais.

- [ ] **Step 6: Rodar GREEN e testes de API**

Run: `rtk cargo test -p node webui::tests`

Expected: PASS.

---

### Task 4: Renomear a experiência de pastas para Armazenamento

**Files:**
- Modify: `apps/frontend/src/routes/ModelPaths.tsx`
- Modify: `apps/frontend/src/App.tsx`
- Modify: `apps/frontend/src/components/nav.ts`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Test: `apps/frontend/src/routes/ModelPaths.test.tsx`

**Interfaces:**
- Rota nova: `/config/armazenamento`.
- Rota antiga: `/config/pastas` redireciona para a nova.
- `PathsResponse.shard_storage` contém `used_bytes`, `max_bytes` e `usage_pct`.
- POST `/api/ui/paths` envia `model_dirs`, `shard_dir` e `shard_max_gb`.

- [ ] **Step 1: Escrever o teste de tela que falha**

Renderizar a tela com uma resposta de paths contendo 25 GB usados e 100 GB máximos. Verificar título Armazenamento, input de limite, texto 25/100 GB e uma progressbar com 25%.

- [ ] **Step 2: Rodar RED**

Run: `cd apps/frontend && rtk npm test -- --run src/routes/ModelPaths.test.tsx`

Expected: FAIL because the current screen has no storage field, no usage block and the old title.

- [ ] **Step 3: Implementar o estado e a barra**

Carregar `shard_max_gb` sem lutar contra a digitação, enviar o valor normalizado e renderizar `Meter` com `usage_pct`. Model directories continuam sem input de limite e sem ação de limpeza.

- [ ] **Step 4: Atualizar navegação, aliases e textos**

Trocar o label do menu para Armazenamento, apontar para `/config/armazenamento`, adicionar redirecionamento da rota antiga e atualizar textos em inglês e português. Ajustar links de tela vazia de modelos para a rota nova.

- [ ] **Step 5: Rodar GREEN e build obrigatório da UI**

Run: `cd apps/frontend && rtk npm test -- --run src/routes/ModelPaths.test.tsx`

Expected: PASS.

Run: `cd apps/frontend && rtk npm run build`

Expected: PASS.

---

### Task 5: Documentar Pipeline Parallelism e Tensor Parallelism

**Files:**
- Create: `docs/subsystems/paralelismo.md`
- Modify: `apps/frontend/src/guides/content.ts`
- Modify: `docs/subsystems/inferencia.md` if links or terminology need reconciliation
- Modify: `docs/subsystems/implantacao.md`
- Modify: `docs/AGENTS.md` root map if the new subsystem needs an index entry
- Modify: `apps/frontend/src/guides/AGENTS.md`

**Interfaces:**
- O guia permanente deve descrever o código atual, não uma arquitetura futura.
- O caminho de documentação "O caminho de uma pergunta" deve ligar ao guia novo.

- [ ] **Step 1: Escrever o conteúdo em português**

Documentar fluxo de uma pergunta, pipeline por faixas completas e hidden states, TP por shards de tensor e reduces, tabela de custo e recomendações WAN/LAN. Explicar que o TP atual não transforma 64 reduces em um único reduce e que uma combinação só é recomendada quando os ranks TP estão próximos.

- [ ] **Step 2: Adicionar a entrada no guia embutido**

Adicionar um aprofundamento em `fatiar-implantar` ou no bloco de caminho da pergunta, com os mesmos conceitos em inglês e português, apontando para `docs/subsystems/paralelismo.md`.

- [ ] **Step 3: Atualizar índices e validar documentação**

Atualizar o mapa de subsistemas e os `AGENTS.md` afetados. Rodar o teste de documentação e o build da UI para garantir que o bundle copia o novo documento.

Run: `cd apps/frontend && rtk npm test -- --run src/routes/Documentation.test.tsx`

Expected: PASS.

---

### Task 6: Fechar a fronteira segura de transferências no backend

**Files:**
- Modify: `crates/node/src/backend/transfer.rs`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/subsystems/transferencia.md`
- Test: `crates/node/src/backend/transfer.rs` tests

**Interfaces:**
- `SendRequest { to_node_id: String, file_name: String }` substitui `path`.
- `local_send_path(state, file_name)` resolve exatamente um basename no catálogo local.
- `ensure_gguf` exige extensão `.gguf`, além da assinatura GGUF.

- [ ] **Step 1: Escrever testes de segurança que falham**

Adicionar testes para `../../etc/passwd`, `/tmp/model.gguf`, `folder/file.gguf`, `model.txt`, nome sem extensão, basename inexistente e basename duplicado. Adicionar teste de oferta com `name = "../../escape.gguf"` recusada antes de `dir.join`.

- [ ] **Step 2: Rodar RED**

Run: `rtk cargo test -p node transfer::tests::send_name`

Expected: FAIL porque `SendRequest` ainda aceita `path` e `ensure_gguf` permite arquivos sem extensão.

- [ ] **Step 3: Implementar validação do basename**

Criar helper que exija um único componente de caminho, rejeite `..`, separadores e extensão diferente de `.gguf`. Não usar `Path::join` com valor não validado.

- [ ] **Step 4: Implementar resolução pelo catálogo**

Usar as mesmas raízes de `GatewayState` que alimentam `/api/ui/models`, executar a varredura fora do runtime Tokio e filtrar por `file_name()`. Retornar erro para zero ou mais de um resultado. Canonicalizar o resultado e passar por `ensure_gguf`.

- [ ] **Step 5: Atualizar envio e recebimento**

`send_to_peer` recebe `file_name`, resolve antes de criar a concessão e continua enviando o caminho somente dentro da comunicação autenticada entre peers. `ensure_gguf` passa a exigir `.gguf`. Validar também o basename da oferta antes de criar destino em `recebidos/`.

- [ ] **Step 6: Rodar GREEN e a suíte de transferências**

Run: `rtk cargo test -p node transfer::tests`

Expected: PASS.

---

### Task 7: Ajustar todos os consumidores frontend de envio

**Files:**
- Modify: `apps/frontend/src/routes/SendFile.tsx`
- Modify: `apps/frontend/src/routes/WholeModels.tsx`
- Modify: `apps/frontend/src/routes/Shards.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Test: `apps/frontend/src/routes/SendFile.test.tsx`
- Test: `apps/frontend/src/routes/WholeModels.test.tsx` or existing model row tests
- Test: `apps/frontend/src/routes/Shards.test.tsx`

**Interfaces:**
- `SendFile` mantém apenas `file_name` no estado e no payload.
- Combobox mostra `m.name` e `m.path`, mas seu `value` é o basename `.gguf`.
- `WholeModels` usa `?file_name=` e não `?path=`.
- `Shards` chama `/api/transfer/send` com `{ to_node_id, file_name }`.

- [ ] **Step 1: Escrever testes frontend que falham**

Testar que o input de caminho não existe, que opções `.gguf` mostram nome e caminho, que a opção sem extensão não aparece, que o POST usa `file_name` e que o botão de modelo e o combobox de fatias enviam o basename.

- [ ] **Step 2: Rodar RED**

Run: `cd apps/frontend && rtk npm test -- --run src/routes/SendFile.test.tsx src/routes/WholeModels.test.tsx src/routes/Shards.test.tsx`

Expected: FAIL porque os componentes ainda enviam `path` e aceitam digitação livre.

- [ ] **Step 3: Implementar o seletor seguro**

Criar helper local `fileNameOf(path)` e `isGgufPath(path)`. Filtrar o catálogo para `.gguf`, deduplicar por id/path e enviar o basename. Preencher a seleção inicial a partir de `?file_name=` somente se ela existir na lista atual.

- [ ] **Step 4: Ajustar modelos e fatias**

Trocar links `?path` por `?file_name`, esconder o botão para entradas extensionless e alterar `Shards.send` para o novo payload. Preservar o destino e os estados de loading.

- [ ] **Step 5: Rodar GREEN e build da UI**

Run: `cd apps/frontend && rtk npm test -- --run src/routes/SendFile.test.tsx src/routes/WholeModels.test.tsx src/routes/Shards.test.tsx`

Expected: PASS.

Run: `cd apps/frontend && rtk npm run build`

Expected: PASS.

---

### Task 8: Verificação integrada, documentação de pasta e entrega

**Files:**
- Modify: `crates/node/src/models/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `apps/frontend/src/guides/AGENTS.md`
- Modify: `docs/subsystems/fatiamento.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/subsystems/transferencia.md`
- Modify: `docs/subsystems/implantacao.md`
- Modify: `docs/superpowers/plans/2026-08-19-armazenamento-paralelismo-transferencias.md`

- [ ] **Step 1: Atualizar todas as documentações de código entregue**

Descrever os novos campos, rotas, retenção, contrato `file_name`, última faixa como resto e links de paralelismo nos `AGENTS.md` e subsistemas correspondentes. Não editar arquivos gerados em `apps/frontend/src/docs`.

- [ ] **Step 2: Rodar verificações Rust**

Run: `rtk cargo fmt --all -- --check`

Expected: sem erros introduzidos nos arquivos alterados; se houver divergências antigas fora do escopo, registrar os caminhos sem reformatação ampla.

Run: `rtk cargo test -p node`

Expected: todos os testes do nó passam.

- [ ] **Step 3: Rodar verificações frontend**

Run: `cd apps/frontend && rtk npm test -- --run`

Expected: todos os testes passam.

Run: `cd apps/frontend && rtk npm run build`

Expected: build concluído e documentação embutida gerada pelo script existente.

- [ ] **Step 4: Verificar segurança e fork**

Run: `rtk git diff --check && rtk git -C llama.cpp status --short`

Expected: diff sem whitespace inválido e fork `llama.cpp` sem modificações.

- [ ] **Step 5: Revisar o diff e preparar entrega**

Run: `rtk git status --short --branch && rtk git diff --stat`

Confirmar que somente os arquivos da especificação, plano, código e documentação afetados aparecem. Atualizar este plano com `[x]` nos passos concluídos e registrar testes reais antes do commit final.
