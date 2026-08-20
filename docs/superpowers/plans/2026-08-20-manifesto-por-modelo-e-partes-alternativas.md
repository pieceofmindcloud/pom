# Manifesto por modelo original e partes alternativas Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`-`) syntax for tracking.

**Goal:** Fazer o manifesto de fatias ser estável por SHA-256 do GGUF original, agregar alternativas de camadas e shards TP, e mesclar manifestos remotos sem perder partes locais.

**Architecture:** O formato v2 usa o hash completo do GGUF original como `pom_id`, mantém um manifesto lógico por modelo e separa `parts` de `tp_parts`. Uma camada de persistência resolve o diretório estável, une entradas por SHA-256 sob lock e grava por rename atômico. Catálogo, transferência e os fluxos de slicing usam essa camada, enquanto manifestos v1 continuam somente para leitura.

**Tech Stack:** Rust, Tokio, serde, SHA-256, Axum, testes unitários do crate `node`, documentação Markdown.

**Spec:** `docs/superpowers/specs/2026-08-20-manifesto-por-modelo-e-partes-alternativas-design.md`

## Global Constraints

- `pom_id` v2 é exatamente o SHA-256 completo do GGUF original.
- Um merge nunca remove uma parte já registrada no manifesto local.
- `parts` nunca contém shard TP; `tp_parts` nunca é usado como faixa de pipeline.
- O mesmo SHA-256 não aparece duas vezes no mesmo array.
- Partes sobrepostas são armazenáveis e o planejador escolhe uma cobertura válida.
- Manifestos v1 continuam legíveis e não são migrados sem hash verificável.
- Não modificar `CHANGELOG.md`, arquivos gerados ou `vendor/llama-cpp-sys-2/llama.cpp`.
- Ao modificar uma pasta, atualizar o `AGENTS.md` e os subsistemas afetados no mesmo conjunto de mudanças.
- Não incluir `.superpowers/` nem `deployments/` nos commits.

---

### Task 1: Formato v2, identidade por fonte e merge atômico

**Files:**
- Modify: `crates/node/src/models/pom.rs`
- Test: `crates/node/src/models/pom.rs` no módulo `tests`

**Interfaces:**
- Produces `PomManifest { pom_version: 2, pom_id: source_sha256, kind, parts, tp_parts }`.
- Produces `PomKind::Combined` quando os dois arrays tiverem conteúdo.
- Produces `PomManifest::new_for_source(...)`, `PomManifest::merge_from(...)`, validation do `pom_id` como hash da fonte e um resolver de diretório estável.
- Preserves the existing v1 loader and v1 validation rule where `pom_id` is derived from `parts`.

- [ ] **Step 1: Write the failing tests**

Add tests with these behaviors:

```rust
#[test]
fn v2_pom_id_e_a_hash_do_modelo_original() {
    let model_hash = "a".repeat(64);
    let manifest = PomManifest::new_for_source(
        pom_model(),
        PomKind::Layers,
        "node-1",
        model_hash.clone(),
        vec![layer_part("b", 0, 7)],
    );

    assert_eq!(manifest.pom_version, 2);
    assert_eq!(manifest.pom_id, model_hash);
    manifest.validate().unwrap();
}

#[test]
fn merge_preserva_alternativas_locais_e_adiciona_tp_parts() {
    let mut local = manifest_with_source("a", vec![layer_part("b", 0, 7)], vec![]);
    let remote = manifest_with_source(
        "a",
        vec![layer_part("c", 0, 3)],
        vec![tp_part("d", 0, 2, 0)],
    );

    local.merge_from(remote).unwrap();

    assert_eq!(local.parts.len(), 2);
    assert_eq!(local.tp_parts.len(), 1);
    assert!(local.parts.iter().any(|p| p.start_layer == 0 && p.end_layer == 7));
    assert!(local.parts.iter().any(|p| p.start_layer == 0 && p.end_layer == 3));
}

#[test]
fn merge_repetido_nao_duplica_sha() {
    let part = layer_part("b", 0, 7);
    let mut local = manifest_with_source("a", vec![part.clone()], vec![]);
    local.merge_from(manifest_with_source("a", vec![part], vec![])).unwrap();
    assert_eq!(local.parts.len(), 1);
}
```

Before these tests, add the following helpers to the existing test module:

```rust
fn sha(seed: char) -> String;
fn pom_model() -> PomModel;
fn layer_part(seed: &str, start: u32, end: u32) -> PomPart;
fn tp_part(seed: &str, rank: u32, size: u32, start: u32) -> PomPart;
fn manifest_with_source(source_seed: &str, parts: Vec<PomPart>, tp_parts: Vec<PomPart>) -> PomManifest;
```

The helper bodies must call the real constructors and return valid 64-character hashes, not mocks or placeholder values.

- [ ] **Step 2: Run the focused tests and verify the expected failure**

Run:

```bash
rtk cargo test -p node --bin pom v2_pom_id_e_a_hash_do_modelo_original -- --nocapture
```

Expected: FAIL because the v2 constructor, `tp_parts` and merge API do not exist yet.

- [ ] **Step 3: Implement the minimal v2 data model**

In `pom.rs`:

1. Raise `POM_VERSION` to 2.
2. Add `PomKind::Combined`.
3. Add `tp_parts: Vec<PomPart>` with `#[serde(default)]` to `PomManifest`.
4. Add `new_for_source(model, kind, created_by, source_sha256, parts)` and initialize `tp_parts` empty.
5. Keep a legacy constructor or update test-only constructors so v1 fixtures still validate with the old parts-derived id.
6. Make v2 validation require a lower-case 64-character `pom_id` and treat it as the original-file hash.
7. Validate that layer entries have no TP metadata and TP entries have valid `PomTp` metadata.
8. Implement `merge_from` to require equal v2 source ids, union each array by SHA-256, preserve local model metadata, sort deterministically, and recompute `kind`.
9. Add `tp_complete(size)` and make `complete()` evaluate whether at least one exact layer chain exists, rather than requiring every stored range to be non-overlapping.

- [ ] **Step 4: Add stable directory and atomic merge primitives**

Implement:

```rust
pub fn source_run_dir(shard_dir: &Path, model_name: &str, source_sha256: &str) -> PathBuf;
pub fn locate_source_dir(shard_dir: &Path, source_sha256: &str) -> Option<PathBuf>;
pub fn merge_manifest(shard_dir: &Path, incoming: &PomManifest) -> Result<(PathBuf, PomManifest)>;
```

`locate_source_dir` must scan v2 manifests and compare the full `pom_id`, so a changed display name reuses the existing folder. `merge_manifest` must serialize writes with a local lock file, load the existing manifest if present, union it with the incoming one, and call the existing temporary-file-plus-rename writer.

- [ ] **Step 5: Run the focused tests and verify they pass**

Run:

```bash
rtk cargo test -p node --bin pom v2_pom_id_e_a_hash_do_modelo_original -- --nocapture
rtk cargo test -p node --bin pom merge_preserva_alternativas_locais_e_adiciona_tp_parts -- --nocapture
rtk cargo test -p node --bin pom merge_repetido_nao_duplica_sha -- --nocapture
```

Expected: PASS, with existing v1 tests still compiling and passing when the `pom` module is run.

- [ ] **Step 6: Commit the isolated format change**

```bash
git add crates/node/src/models/pom.rs
git commit -m "feat: estabilizar manifesto pelo hash do modelo"
```

### Task 2: Hash da fonte e integração com fatiamento local e apply

**Files:**
- Modify: `crates/node/src/models/pom.rs`
- Modify: `crates/node/src/backend/webui.rs`
- Test: `crates/node/src/models/pom.rs` and `crates/node/src/backend/webui.rs` existing test modules

**Interfaces:**
- Consumes `source_run_dir`, `locate_source_dir` and `merge_manifest` from Task 1.
- Produces source-hash-aware `finalize_run` behavior for `run_slice` and `seal_single_part`.

- [ ] **Step 1: Write the failing tests**

Add tests for stable source identity:

```rust
#[test]
fn duas_particoes_do_mesmo_arquivo_reusam_a_mesma_pasta() {
    let source_hash = "a".repeat(64);
    let first = manifest_with_source(&source_hash, vec![layer_part("b", 0, 7)], vec![]);
    let second = manifest_with_source(&source_hash, vec![layer_part("c", 0, 3)], vec![]);

    let first_dir = dir_for_source_fixture(&first);
    let second_dir = dir_for_source_fixture(&second);
    assert_eq!(first_dir, second_dir);
}

#[test]
fn merge_de_manifestos_nao_apaga_arquivo_de_parte_existente() {
    let root = tempdir();
    let local = manifest_with_source("a", vec![layer_part("b", 0, 7)], vec![]);
    let remote = manifest_with_source("a", vec![layer_part("c", 0, 3)], vec![]);
    write_manifest_with_part(root.path(), &local, "b");

    let (_, merged) = merge_manifest(root.path(), &remote).unwrap();
    assert!(merged.parts.iter().any(|p| p.sha256.starts_with('b')));
}
```

Add these test helpers beside the existing filesystem helpers:

```rust
fn dir_for_source_fixture(manifest: &PomManifest) -> PathBuf;
fn write_manifest_with_part(root: &Path, manifest: &PomManifest, part_seed: &str);
```

The helpers must create the source-based directory, write a real GGUF-named file with `part_path`, and call the production merge path. Add a webui-level regression around sealing a layer part and a TP part for the same source hash, asserting one directory and one manifest with both arrays.

- [ ] **Step 2: Run the new tests and verify they fail**

Run:

```bash
rtk cargo test -p node --bin pom duas_particoes_do_mesmo_arquivo_reusam_a_mesma_pasta -- --nocapture
```

Expected: FAIL because finalization still derives the directory from the parts hash and the apply path still creates a one-part manifest.

- [ ] **Step 3: Implement source hashing once per operation**

Add a helper that hashes the original GGUF in a blocking task before the slicing loop. In `slice_locally`, calculate the source hash once and pass it into `run_slice`. In `run_apply`, calculate it once before the layer or TP branch. Keep `hash_file` streaming and reuse the existing path/size/mtime cache if available.

- [ ] **Step 4: Integrate `run_slice` with the aggregate manifest**

Change `run_slice` to construct a v2 source manifest, put all newly produced layer parts in `parts`, and finalize through the source-aware merge. The scratch directory may still be unique per job; only the final model directory is shared.

- [ ] **Step 5: Integrate `seal_single_part` with both arrays**

Change the signature to receive the original source hash. Put layer entries in `parts` and TP entries in `tp_parts`. Use source-aware finalization for both branches. When the target directory already contains the same content hash, keep the existing file and only merge the metadata.

- [ ] **Step 6: Run focused webui and pom tests**

Run:

```bash
rtk cargo test -p node --bin pom source -- --nocapture
rtk cargo test -p node --bin pom seal_single_part -- --nocapture
```

Expected: PASS. If a test name is private or not discoverable by the filter, run the enclosing module test target and inspect only the relevant output.

- [ ] **Step 7: Commit the slicing integration**

```bash
git add crates/node/src/models/pom.rs crates/node/src/backend/webui.rs
git commit -m "feat: agregar fatias do mesmo modelo ao manifesto"
```

### Task 3: Catálogo e alternativas de pipeline

**Files:**
- Modify: `crates/node/src/models/catalog.rs`
- Modify: `crates/node/src/backend/deploy.rs`
- Test: `crates/node/src/models/catalog.rs` and `crates/node/src/backend/deploy.rs`

**Interfaces:**
- Consumes the v2 manifest arrays from Task 1.
- Produces one layer `ShardSet` per source `pom_id`, with all physically present layer alternatives.
- Keeps TP entries out of layer `ShardSet` planning.

- [ ] **Step 1: Write the failing catalog tests**

Add a test that writes one v2 manifest containing `L0-7`, `L0-3`, and a TP entry, then discovers the directory:

```rust
#[test]
fn um_manifesto_expoe_alternativas_de_camadas_e_separa_tp() {
    let catalog = discover_manifest_with_parts(
        vec![layer_part("b", 0, 7), layer_part("c", 0, 3)],
        vec![tp_part("d", 0, 2, 0)],
    );

    assert_eq!(catalog.shard_sets.len(), 1);
    assert_eq!(catalog.shard_sets[0].shards.len(), 2);
    assert!(catalog.shard_sets[0].complete);
    assert!(catalog.models.iter().all(|m| m.tensor_parallel.is_none()));
}
```

Define this fixture in the catalog test module:

```rust
fn discover_manifest_with_parts(parts: Vec<PomPart>, tp_parts: Vec<PomPart>) -> Catalog;
```

It must write every listed part under one v2 manifest, write valid GGUF test bytes for each file, call `discover(&[], shard_root)`, and return the real catalog. Add a deploy planner regression proving a candidate chain can choose `L0-3` for one stage and another range afterward without refusing merely because `L0-7` is also present.

- [ ] **Step 2: Run the tests and verify the expected failure**

Run:

```bash
rtk cargo test -p node --bin pom um_manifesto_expoe_alternativas_de_camadas_e_separa_tp -- --nocapture
```

Expected: FAIL because `part_index` reads only `parts`, `DiscoveredModel` has no TP marker, and `ShardSet::complete` treats the aggregate as one ordered coverage.

- [ ] **Step 3: Index both arrays with explicit part kind**

Extend `PartContext` and `DiscoveredModel` with the TP metadata needed by the catalog. Index `m.parts` as layer parts and `m.tp_parts` as tensor parts. Add a layer-specific predicate used by `build_shard_sets`, so TP files never become pipeline candidates.

- [ ] **Step 4: Make aggregate completeness search for one valid chain**

Replace the current contiguous scan in `build_shard_sets` with an interval reachability check that accepts an aggregate containing overlaps when at least one exact chain reaches `total_layers`. Keep all physical alternatives in `shards`.

- [ ] **Step 5: Preserve alternative candidates in deploy survey**

Ensure `parse_shard`, `candidates`, `auto_chain` and the manual-selection validation continue to deduplicate only identical layer ranges and do not reject a second range starting at layer zero. Add or adjust the regression test at the planner boundary, not only a helper test.

- [ ] **Step 6: Run focused catalog and deploy tests**

Run:

```bash
rtk cargo test -p node --bin pom um_manifesto_expoe_alternativas_de_camadas_e_separa_tp -- --nocapture
rtk cargo test -p node --bin pom shard_set_completeness -- --nocapture
rtk cargo test -p node --bin pom overlapping -- --nocapture
```

Expected: PASS, with the planner accepting alternative inventories while still refusing actual gaps in the selected chain.

- [ ] **Step 7: Commit the catalog change**

```bash
git add crates/node/src/models/catalog.rs crates/node/src/backend/deploy.rs
git commit -m "feat: catalogar alternativas do mesmo modelo"
```

### Task 4: Transferência com merge do manifesto local

**Files:**
- Modify: `crates/node/src/backend/transfer.rs`
- Modify: `crates/node/src/models/pom.rs` only if a transfer-specific resolver is needed
- Test: `crates/node/src/backend/transfer.rs`

**Interfaces:**
- Consumes source-aware manifest resolution and merge from Task 1.
- Produces offer, pull and receive flows that preserve the receiver's local manifest entries.

- [ ] **Step 1: Write the failing transfer test**

Extend the existing transfer test fixture so the receiver starts with a local v2 manifest containing `L0-7`, the sender offers `L0-3`, and the completed receiver manifest contains both:

```rust
#[tokio::test]
async fn receber_parte_mescla_manifesto_remoto_ao_manifesto_local() {
    let (cliente, servidor, local) = transfer_fixture_with_local_part("b", 0, 7).await;
    send_one_part_from(&servidor, "c", 0, 3).await;
    accept_and_wait(&cliente).await;

    let merged = load_manifest_for(&cliente, "c").await;
    assert!(merged.parts.iter().any(|p| p.start_layer == 0 && p.end_layer == 7));
    assert!(merged.parts.iter().any(|p| p.start_layer == 0 && p.end_layer == 3));
}
```

Define the fixture operations using the existing transfer testkit:

```rust
async fn transfer_fixture_with_local_part(local_seed: &str, start_layer: u32, end_layer: u32) -> (GatewayState, GatewayState, PomManifest);
async fn send_one_part_from(sender: &GatewayState, seed: &str, start_layer: u32, end_layer: u32);
async fn accept_and_wait(client: &GatewayState);
async fn load_manifest_for(client: &GatewayState, seed: &str) -> PomManifest;
```

Implement these by adapting `no_com_parte`, the existing fake peer server and current offer acceptance helpers. Do not add a second network harness. Add the TP equivalent, asserting that the received rank is added to `tp_parts` without removing existing layer entries.

- [ ] **Step 2: Run the focused transfer test and verify it fails**

Run:

```bash
rtk cargo test -p node --bin pom receber_parte_mescla_manifesto_remoto_ao_manifesto_local -- --nocapture
```

Expected: FAIL because `alvo_da_oferta` and the pull path currently write the incoming manifest directly.

- [ ] **Step 3: Merge before writing the destination manifest**

Replace direct `write_atomic` calls in `alvo_da_oferta`, the pull/download setup, and any manifest-fetch path with the source-aware merge. Use the returned resolved directory for the destination path. Keep a missing physical file out of the catalog until the download is complete.

- [ ] **Step 4: Search both arrays when validating a transferred part**

Update `servable_path`, `manifest_for`, `fetch_pom` consumers and every `m.parts.iter()` hash lookup involved in transfer to check `parts` and `tp_parts`. Preserve the current refusal for a hash that has no valid manifest.

- [ ] **Step 5: Run focused transfer tests**

Run:

```bash
rtk cargo test -p node --bin pom transfer -- --nocapture
rtk cargo test -p node --bin pom receber_parte_mescla_manifesto_remoto_ao_manifesto_local -- --nocapture
```

Expected: PASS, including the existing path-safety and manifest-required tests.

- [ ] **Step 6: Commit the transfer change**

```bash
git add crates/node/src/backend/transfer.rs crates/node/src/models/pom.rs
git commit -m "fix: mesclar manifesto local ao receber partes"
```

### Task 5: Document the v2 contract and perform focused verification

**Files:**
- Modify: `crates/node/src/models/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/subsystems/fatiamento.md`
- Modify: `docs/subsystems/implantacao.md`
- Modify: `docs/subsystems/transferencia.md`

**Interfaces:**
- Documents the behavior delivered by Tasks 1-4.
- Does not edit generated frontend docs or `CHANGELOG.md`.

- [ ] **Step 1: Update the model and backend AGENTS**

Document that v2 `pom_id` is the original GGUF hash, that `parts` and `tp_parts` are merged inventories, and that transfer handlers union local and remote manifests.

- [ ] **Step 2: Update subsystem documentation**

Change the storage examples from a per-round directory to the stable source-hash directory. Explain that overlapping layer alternatives and TP groups can coexist, and that a received manifest never replaces the receiver's local entries.

- [ ] **Step 3: Run formatting and focused verification**

Run:

```bash
rtk cargo fmt --all
rtk cargo test -p node --bin pom pom::tests -- --nocapture
rtk cargo test -p node --bin pom catalog::tests -- --nocapture
rtk cargo test -p node --bin pom transfer -- --nocapture
rtk git diff --check
```

Expected: PASS for the focused Rust tests and no whitespace errors. Do not run the full workspace suite unless a focused test exposes an integration failure that requires it.

- [ ] **Step 4: Build the frontend contract**

Run:

```bash
cd apps/frontend
rtk npm run build
```

Expected: PASS. No frontend source change is expected unless the catalog JSON change reveals an existing type assumption.

- [ ] **Step 5: Review the final diff and commit documentation**

```bash
git status --short
git diff --check
git diff --stat
git add crates/node/src/models/AGENTS.md crates/node/src/backend/AGENTS.md docs/subsystems/fatiamento.md docs/subsystems/implantacao.md docs/subsystems/transferencia.md
git commit -m "docs: documentar manifesto v2 e merge de partes"
```

Leave `.superpowers/` and `deployments/` untracked and do not include them.
