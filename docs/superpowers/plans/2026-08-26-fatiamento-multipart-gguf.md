# Fatiamento multipart GGUF Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ler conjuntos GGUF multipart diretamente, recortar metadata indexada por camada e bloquear na UI qualquer entrada multipart incompleta ou inválida.

**Architecture:** Um módulo de fonte GGUF abre o primeiro arquivo e os irmãos, mantendo um mmap por parte e uma tabela unificada de tensores com origem física. `gguf`, `splitter`, catálogo e identidade consumirão essa fonte; o frontend receberá fatos do catálogo e do plano, sem heurística por nome.

**Tech Stack:** Rust, `memmap2`, `sha2`, serde, Axum, React/TypeScript, Vitest.

**Spec:** `docs/superpowers/specs/2026-08-26-fatiamento-multipart-gguf-design.md`

## Global Constraints

- Não executar fatiamento real nem escrever na ksia.
- Não introduzir comportamento condicionado a nome de modelo, família ou arquivo.
- Não tocar nos bindings preexistentes de `llama-cpp-sys-2`.
- Manter o GGUF único como fonte de uma parte, com comportamento equivalente.
- Arrays escalares arquiteturais com comprimento >= `block_count` são recortados pela faixa; vetores curtos ou aninhados ambíguos são recusados.
- Atualizar `AGENTS.md` e os READMEs de subsistema tocados no mesmo commit.
- Não editar arquivos gerados em `apps/frontend/src/docs/` ou CHANGELOG.

## Mapa de arquivos

- Criar `crates/node/src/models/multipart.rs`: parser de partes, tabela unificada, mmap e hash lógico.
- Modificar `crates/node/src/main.rs`: declarar o módulo compartilhado.
- Modificar `crates/node/src/models/splitter.rs`: consumir a fonte unificada nos três caminhos, preservar part index nos segmentos e generalizar arrays por camada.
- Modificar `crates/node/src/models/gguf.rs`: ler layout da fonte unificada, somar tamanho físico e aplicar o mesmo predicado de arrays.
- Modificar `crates/node/src/models/catalog.rs`: publicar descrição multipart e tamanho total.
- Modificar `crates/node/src/backend/webui.rs`: usar identidade multipart e transportar bloqueio/descrição no plano.
- Modificar `apps/frontend/src/lib/types.ts`, `apps/frontend/src/routes/SliceModels.tsx` e `apps/frontend/src/i18n/locales.ts`: contrato, selo, partes, diagnóstico e bloqueio.
- Atualizar `crates/node/src/models/AGENTS.md`, `docs/subsystems/fatiamento.md`, `docs/subsystems/web-ui.md` e `apps/frontend/src/i18n/AGENTS.md`.
- Testes ficam nos módulos Rust existentes e em testes novos da tela de fatiar.

### Task 1: Fixar os contratos puros com testes falhando

**Files:**
- Create: `crates/node/src/models/multipart.rs`
- Modify: `crates/node/src/main.rs`
- Test: `crates/node/src/models/splitter.rs` e `crates/node/src/models/multipart.rs`

**Interfaces:**
- Produces `multipart::GgufSource::open(&Path) -> anyhow::Result<GgufSource>`.
- Produces `multipart::SourceTensor { name, dims, dtype, part_index, offset, byte_size }`.
- Produces `multipart::GgufSource::source_bytes() -> u64`, `part_paths() -> &[PathBuf]` e `hash() -> anyhow::Result<String>`.

- [ ] Escrever testes sintéticos para primeira parte sem tensor, três partes com índices e offsets locais, hash ordenado, parte secundária e contagem divergente.
- [ ] Escrever testes puros para seleção de faixa de array: vetor alternado com especiais de cabeça/cauda, vetor com excedentes e vetores curtos/aninhados.
- [ ] Rodar `rtk cargo test -p node multipart::tests splitter::tests::layer_metadata` e confirmar falha por símbolos ausentes antes da implementação.

### Task 2: Implementar a fonte GGUF multipart

**Files:**
- Modify: `crates/node/src/models/multipart.rs`

- [ ] Implementar parser GGUF de header, KV e tensor table, mantendo offsets de bytes do primeiro header e o `part_index` dos tensores.
- [ ] Derivar irmãos somente do padrão numérico `-NNNNN-of-NNNNN.gguf`, validar `split.count`, `split.no` e `split.tensors.count`, e rejeitar entradas secundárias.
- [ ] Calcular byte size por diferenças de offsets dentro de cada mmap e tratar offsets empatados como o mesmo tensor físico.
- [ ] Implementar hash SHA-256 e soma de bytes por ordem de `split.no`, sem arquivo temporário.
- [ ] Rodar os testes da Task 1; esperar os casos multipart verdes e manter o baseline Rust apenas nos erros de binding já registrados.

### Task 3: Migrar layout e os três caminhos do splitter

**Files:**
- Modify: `crates/node/src/models/gguf.rs`
- Modify: `crates/node/src/models/splitter.rs`

- [ ] Fazer `read_layout` consumir `GgufSource`, usar a tabela completa e preencher `source_file_bytes` com o total físico.
- [ ] Reescrever o recorte de KV para usar `array.count >= original_block_count`, manter broadcasts e recusar comprimentos curtos com chave e tamanho.
- [ ] Fazer `split`, `validate_tensor_parallel` e `shard_tensor_parallel` abrirem `GgufSource`; todos os segmentos devem carregar `part_index` e copiar do mmap correto.
- [ ] Manter `merge` no caminho de fatias POM, que continua recebendo arquivos únicos já produzidos pelo POM.
- [ ] Reexecutar os testes sintéticos e adicionar regressão para GGUF único, array por camada e vetor maior com os valores da faixa real.

### Task 4: Propagar identidade e diagnóstico no backend e catálogo

**Files:**
- Modify: `crates/node/src/models/catalog.rs`
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `apps/frontend/src/lib/types.ts`

- [ ] Detectar multipart na descoberta mantendo apenas a primeira parte como item lógico, mas publicar `multipart`, `multipart_count`, `multipart_parts` e `source_bytes` somado.
- [ ] Usar o hash lógico da fonte em `source_model_sha256` para manifesto e `model_identity`; manter hash de fatias POM inalterado.
- [ ] Fazer o plano de slice transportar `blocked` e warning explicando quando a fonte é uma parte secundária, falta irmão, índice inválido ou a metadata não pode ser recortada com evidência.
- [ ] Testar o catálogo com arquivos GGUF sintéticos e a serialização do plano com mensagem concreta.

### Task 5: Mostrar e impedir multipart incorreto na UI

**Files:**
- Modify: `apps/frontend/src/routes/SliceModels.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/routes/SliceModels.test.tsx` ou teste auxiliar do catálogo

- [ ] Adicionar no seletor/painel o selo MULTIPARTE, tamanho somado em GB e tabela com índice, nome e bytes de cada parte.
- [ ] Mostrar o aviso de operação: manter todas as partes juntas e fatiar pela primeira parte com o POM.
- [ ] Desabilitar o botão quando o backend marcar o plano como bloqueado e renderizar o motivo retornado, incluindo propriedade observada e ação recomendada.
- [ ] Cobrir arquivo único sem selo, multipart completo com preview e multipart bloqueado sem chamada de `/api/ui/slice`.
- [ ] Rodar os testes frontend normais e `rtk npm run build` em `apps/frontend`.

### Task 6: Documentar, revisar e validar

**Files:**
- Modify: `crates/node/src/models/AGENTS.md`
- Modify: `docs/subsystems/fatiamento.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `apps/frontend/src/i18n/AGENTS.md`

- [ ] Atualizar a documentação com a fonte multipart, o predicado de arrays >= block count, o hash lógico e a fronteira de transferência.
- [ ] Rodar `rtk cargo check -p node` uma vez após a implementação e comparar cada erro com a lista de baseline: nenhum erro novo pode aparecer.
- [ ] Rodar os testes/build frontend, `rtk git diff --check`, `rtk git status` e revisar que não houve fatiamento remoto.
- [ ] Commitar com mensagem sem coautor automático.
- [ ] Empurrar somente `fm/rdma-multipart-gguf-fatiamento` e abrir PR com mudanças, critério genérico, validação e limites honestos.

