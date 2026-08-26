# Catálogo de metadata sem laço de leitura - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fazer o enriquecimento do catálogo terminar uma vez, publicar as arquiteturas válidas e marcar arquivos inválidos sem reler GGUFs indefinidamente.

**Architecture:** O catálogo continuará em duas fases. A fase rápida enumera e faz `stat`; a fase de metadata usará somente o leitor de cabeçalho e uma sondagem limitada da tabela de tensores, sem `GgufSource::open` ou hash. Cada modelo terá sucesso ou erro terminal dentro daquela geração do catálogo, e a resposta completa será reutilizada até uma invalidação ou refresh explícito.

**Tech Stack:** Rust, Tokio `spawn_blocking`, parser GGUF existente, testes unitários de `node`.

**Spec:** Evidência e requisitos da tarefa `v1 launch-brief` sobre o laço de metadata do catálogo.

## Global Constraints

- Não calcular hash integral nem varrer dados de tensores no caminho do catálogo.
- Um erro de um modelo não pode manter `metadata_complete` falso para os demais.
- Uma nova leitura só pode ocorrer por nova geração do catálogo, não por cada requisição da interface.
- Toda documentação alterada deve permanecer em pt-BR e usar hífen simples.
- Commit e push devem nomear arquivos individualmente; nunca usar `git add -A` ou `git commit -a`.

---

### Task 1: Capturar o contrato terminal do enriquecimento

**Files:**
- Modify: `crates/node/src/models/catalog.rs` - teste de modelo GGUF inválido e fixture de arquivo grande.
- Modify: `crates/node/src/models/AGENTS.md` - contrato de sucesso/erro terminal do enriquecimento.

**Interfaces:**
- Consumes: `catalog::discover_fast` e `catalog::enrich`.
- Produces: `DiscoveredModel.metadata_error` e `Catalog.metadata_complete` verdadeiro após uma passagem, mesmo com falha isolada.

- [x] **Step 1: Write the failing test**

Atualizar o teste de arquivo ilegível para exigir `metadata_complete == true`, `architecture == None` e um erro observável no modelo. Adicionar um teste com GGUF válido e payload esparso de 1 GiB, exigindo arquitetura preenchida e conclusão; o fixture terá cabeçalho pequeno e dados sem conteúdo para provar que metadata não depende do peso.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node catalog::tests::enrichment_marks_unreadable_model_terminal_without_blocking_catalog catalog::tests::enrichment_does_not_require_validating_tensor_payload`

Expected: a compilação falha nos 12 erros preexistentes de bindings/FFI já registrados na linha de base, antes dos testes; se a suíte Rust ficar compilável, o teste antigo falha porque o catálogo atual publica `metadata_complete: false`.

- [x] **Step 3: Write minimal implementation**

Adicionar `metadata_error` ao modelo e fazer `enrich` publicar resultados válidos e falhos como uma única geração completa.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node catalog::tests::enrichment_marks_unreadable_model_terminal_without_blocking_catalog catalog::tests::enrichment_does_not_require_validating_tensor_payload`

Resultado nesta worktree: a compilação ficou bloqueada pelos mesmos erros de FFI
da baseline; nenhum erro dos arquivos alterados apareceu.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/models/catalog.rs crates/node/src/models/AGENTS.md
git commit -m "fix: concluir enriquecimento parcial do catalogo"
```

### Task 2: Tirar multipart e hash integral do caminho de metadata

**Files:**
- Modify: `crates/node/src/models/gguf.rs` - expor contagens e identificação de partes durante a leitura do cabeçalho.
- Modify: `crates/node/src/models/multipart.rs` - construir somente o resumo físico por `stat` e cabeçalhos.
- Modify: `crates/node/src/models/catalog.rs` - usar o resumo leve, mantendo `GgufSource` para slicing/transferência.
- Modify: `crates/node/src/models/AGENTS.md` - registrar que o catálogo não mapeia partes inteiras.
- Modify: `docs/subsystems/fatiamento.md` - separar leitura leve do catálogo da abertura completa usada para fatiar.

**Interfaces:**
- Consumes: `gguf::read_info`, convenção de nomes multipart e `std::fs::metadata`.
- Produces: metadados de partes para a UI sem leitura de payload; `GgufSource::open` continua exclusivo dos consumidores que precisam da tabela/layout completo.

- [x] **Step 1: Write the failing test**

Estender o teste de fixture esparso para usar uma fonte declarada como multipart incompleta e confirmar que o catálogo retorna erro concreto e tamanho desconhecido sem tentar abrir/mapear o payload.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node multipart::tests::catalog_metadata_reads_headers_without_opening_tensor_source`

Expected: falha por símbolo ausente ou por tentativa do caminho atual de abrir `GgufSource` para montar metadata.

- [x] **Step 3: Write minimal implementation**

Adicionar os campos de contagem ao resultado do leitor de cabeçalho, implementar o resumo multipart baseado em headers/stats e substituir a chamada de `GgufSource::open` em `catalog::enrich`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node multipart::tests::catalog_metadata_reads_headers_without_opening_tensor_source`

Resultado nesta worktree: a compilação ficou bloqueada pelos mesmos erros de FFI;
a busca de referências confirma que `catalog.rs` não chama `hash_sha256`,
`hash_file` ou `GgufSource::open`.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/models/gguf.rs crates/node/src/models/multipart.rs crates/node/src/models/catalog.rs crates/node/src/models/AGENTS.md docs/subsystems/fatiamento.md
git commit -m "fix: limitar metadata multipart ao cabecalho"
```

### Task 3: Impedir relançamento por polling e expor o resultado reutilizado

**Files:**
- Modify: `crates/node/src/backend/webui.rs` - publicar resultado terminal mesmo com falhas por modelo e não reabrir uma geração concluída.
- Modify: `crates/node/src/backend/AGENTS.md` - documentar o ciclo terminal e a reutilização do cache.
- Modify: `docs/subsystems/web-ui.md` - documentar `metadata_complete` e erros por modelo.

**Interfaces:**
- Consumes: `Catalog.metadata_complete` e `DiscoveredModel.metadata_error`.
- Produces: `GET /api/ui/models` responde o mesmo catálogo completo até uma invalidação; o polling não repete leituras.

- [x] **Step 1: Inspect the cache transition**

Confirmar no fluxo `local_catalog`/`spawn_enrichment` que uma geração publicada não agenda
uma segunda tarefa enquanto `metadata_complete` é falso.

- [x] **Step 2: Reproduzir a causa no código existente**

Verificar que o código anterior descartava o resultado incompleto e que o polling
reativava o enriquecimento quando a tarefa terminava.

Expected: o código anterior mantinha o catálogo incompleto e `local_catalog` agendava nova tarefa quando o request seguinte chegava.

- [x] **Step 3: Write minimal implementation**

Fazer `spawn_enrichment` sempre publicar o catálogo retornado, considerar a tarefa completa quando cada modelo recebeu sucesso ou erro terminal e remover o gatilho baseado em `!metadata_complete` após publicação.

- [x] **Step 4: Verify the cache transition**

Run: `rg 'enriching|!c\.catalog\.metadata_complete' crates/node/src/backend/webui.rs`

Expected: não há gatilho de repetição por `metadata_complete`; o cache permanece
estável depois da publicação e só TTL/invalidação inicia outra geração.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/backend/webui.rs crates/node/src/backend/AGENTS.md docs/subsystems/web-ui.md
git commit -m "fix: reutilizar resultado do enriquecimento do catalogo"
```

### Task 4: Validar baseline, integração e entrega compartilhada

**Files:**
- No new source files. Validate the named files from Tasks 1-3.

**Interfaces:**
- Consumes: commits locais da correção e `origin/feat/implantar` atualizado.
- Produces: branch compartilhada com a correção rebased e pushed, sem alterar arquivos fora do escopo.

- [x] **Step 1: Run formatting and focused tests**

Run: `cargo fmt --all -- --check` e os testes focados de catálogo/multipart/webui; registrar que a compilação continua limitada aos mesmos 12 erros de bindings se persistir.

- [x] **Step 2: Run frontend validation when the backend contract changes**

Run: `cd apps/frontend && npm run build`; a mudança é backward-compatible porque só acrescenta `metadata_error` opcional.

- [x] **Step 3: Rebase on the latest shared branch**

Run: `git fetch origin` e `git pull --rebase origin feat/implantar`; se a branch continuar anexada a outra worktree, usar somente o rebase equivalente sobre `origin/feat/implantar`, sem forçar ou reescrever commits remotos.

- [x] **Step 4: Inspect the exact named-file status**

Run: `git status`; remover do index/worktree qualquer arquivo fora de `crates/node/src/models/gguf.rs`, `crates/node/src/models/multipart.rs`, `crates/node/src/models/catalog.rs`, `crates/node/src/backend/webui.rs`, os AGENTS/READMEs listados e este plano antes do commit final.

- [ ] **Step 5: Push without force**

```bash
git push origin HEAD:feat/implantar
```

Expected: push fast-forward para `origin/feat/implantar`; registrar a SHA final no status file.
