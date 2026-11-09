# `crates/node/src/models/`

> **Crate change:** `gguf.rs`, `multipart.rs` and `kv_budget.rs` moved to
> `crates/llama-engine` (the FFI side needs them). `node` re-exports them in
> `main.rs`; `crate::gguf::…` etc. keep working. `splitter.rs` stayed here and
> uses `crate::gguf` normally (in tests, `llama_engine::llama_cpp_sys_2`).

Models, GGUF reading, planning and slice files. The module is used by
`main.rs` and by the catalog, slicing, deployment and transfer handlers.

- `catalog.rs` sidecars - `Catalog.sidecars` lists vision projectors and
  external MTP drafts found under the shard folder and the GGUF libraries
  (`discover_sidecars`, filled by `enrich`); `sidecar_kind` recognizes them
  from the header (clip, `nextn` with few tensors and no split, or a `dflash`
  DSpark drafter, which is DeepSeek-V4.1's draft), so they are
  dropped from `models`. A file with the splitter's `pom.start_layer` /
  `pom.end_layer` is a slice and never a draft: a short tail range of a model
  with `nextn` has a draft's tensor count. `find_gguf` takes a file by its `.gguf` extension or,
  when the name has no extension (`is_extensionless_name`, as Hugging Face
  publishes some drafts), by its `GGUF` signature. The walk skips `.mtp-drafts/`, the engine's cache of
  completed shared drafts (`llama_engine::mtp_draft`). `Sidecar::compatible_with` is the projector/draft to
  model rule, `SidecarLink`/`SidecarMode` the link kept on the principal,
  `SidecarFile` the name + size identity and `find_local_sidecar` the on-disk
  lookup used at apply time. `DiscoveredModel.embedding_length` carries the
  width a sidecar must match.
- `catalog.rs` - discovers whole models and slices in the configured folders.
  `discover_fast` lists files and reads only `stat`/system metadata, without
  opening the GGUF content; `enrich` reads the headers afterwards and marks
  `Catalog.metadata_complete`. A read failure marks `metadata_error` on the item
  without blocking the valid models; after a terminal pass, `metadata_complete`
  becomes `true`. A new attempt only happens on a new refresh or invalidation.
  The capabilities the catalog publishes come only from the header; it does not
  walk the tensor table or the weights. The structural evidence of MTP stays in
  `gguf::read_capability_evidence`, today limited to the proofs that require
  that confirmation; the full layout computation stays separate.
  `discover` combines the two phases for synchronous callers that still need the
  complete answer. The catalog does not compute a content hash: hashes coming
  from manifests or from a slice's name are identity data only.
  While `metadata_complete` is false, the absence of a path means the enumeration
  has not finished yet, not that the file is outside the catalog; the selection
  handlers must expose that state so the client can try again.
  Numeric values of the GGUF header are accepted by the semantics of a
  non-negative integer, regardless of whether the producer wrote them as signed
  or unsigned; negative values and overflow are refused. This includes
  `split.tensors.count`, which the reference `gguf-split` writes as INT32.
  Each enriched result keeps only in memory the `path + size + mtime` identity of
  the file. The backend uses that mark to reuse metadata when a refresh creates
  another inventory, and to enrich only new or changed models - but only while
  the new inventory says the same about the file (`webui::catalog::
  same_inventory_facts`: name, slicing, model label, `pom_id`, TP rank). A slice
  indexed before its `.pom` existed is enriched again once the manifest shows
  up; reusing it kept the slice nameless, shown by its hash, through every
  rescan. The walk skips the `.slicing-*` working folders of running rounds
  (`pom::SCRATCH_PREFIX`), and `pom::finalize_run` writes the manifest before
  moving the parts into the model folder, so no scan sees a part without it.
  For a multipart GGUF source, `enrich` reads the headers and `stat`s the parts
  through `multipart::catalog_metadata`, without opening the full weight mapping;
  it sums the bytes and exposes the physical parts in the catalog. An incomplete
  declaration stays visible with a concrete error and an unknown size. The whole
  model's identity uses the hash of the complete part sequence only at the
  slicing or transfer boundary.
- `plan.rs` - computes ranges by budget or by real layer size and calls the GGUF
  extractor. The budget endpoint uses `GgufInfo` and `kv_budget.rs` to sum weight
  and KV in bytes before accepting a range. The plan validates the architecture's
  output-tensor contract before offering or generating a range; required heads
  are charged and copied from the source when the loader demands their presence
  in partial ranges.
- the engine's `gguf` module - reads the scalar or per-layer KV geometry
  (`attention.head_count_kv`, `key_length` and `value_length`) and the MoE
  evidence (`<arch>.expert_count`/`expert_used_count`) without loading the
  weights. It also reads `tokenizer.chat_template`, the real control tokens and
  the `nextn` fields/tensors to produce the thinking and MTP evidence the planner
  uses. The canonical key `<architecture>.nextn_predict_layers` and the
  prefix-less form are accepted; the presence of the `nextn` tensors is
  structural proof when the producer omits the metadata. A missing read stays
  indeterminate. `read_layout` validates the dtypes against the supported GGML
  table, discards that information after the validation, uses physical offsets
  for the tensor cost, and computes the serialized size of each slice including
  header, globals, alignment and duplications, and the architecture-specific
  heads. `validate_slice_ranges` shares the decision with the slicer.
  `deepseek4` requires six non-block tensors and accepts no fallback to
  `token_embd`; non-final ranges copy from the source the real head tensors the
  loader requires and keep the identity norm already used by the trunk. If the
  source has no copyable tensor, the refusal names the missing tensors.
  `LayerCountSemantics` classifies metadata that llama.cpp compares with the
  local index: counts from the start are rebased by `start_layer` and capped to
  the slice's size, while the count from the end of the NextN is zeroed outside
  the last range. A non-scalar value, or one that does not fit in `u32`, refuses
  the layout with the key and the cause.
  `GgufLayout::required_head_bytes` measures the real payload repeated per
  intermediate slice.
  `read_layout` uses the unified table of multipart.rs, so the first file of a
  multipart GGUF, even without tensors, represents the whole set. The cost and
  the selection also consider tensors present in every physical part, including
  the third or later ones.
- multipart.rs - maps a simple GGUF or all the parts declared by split.count,
  split.no and split.tensors.count. It validates the N-of-M.gguf convention,
  resolves each tensor to a part and a relative offset, and sums/hashes only the
  physical sequence, without reassembling weights.
 - The cut options for a whole model come from
  `GgufLayout::valid_prefix_end_layers`: in a dense layout, any whole-layer end
  is valid; in a hybrid layout, only the ends derived from `arch_boundaries` are
  valid. A kind change at the index of a start-relative count (the dense prefix
  of a MoE, `leading_dense_block_count`) is not a boundary, because the slicer
  rebases that count. `prefix_cut_granularity` only reports a step when every cut has the same
  distance; the consumer uses the cut list when the boundaries are irregular.
- `splitter.rs` - maps the source to read the header and the weights without
  keeping a GGUF fd open. The logical table is not a storage order: the selected
  payloads are sorted by `(part_index, src_offset)` before the copy to avoid
  regressive page faults on a source larger than RAM. Before the output, the
  slicer checks the list of required tensors the loader creates for the
  architecture. For `deepseek4`, the final range carries the complete head and
  each intermediate range copies the real required weights, without fabricating
  `output.weight` or HC tensors. Count metadata relative to the start is
  rewritten to the range's local numbering; metadata relative to the end keeps
  its value only in the final range. The size estimate includes the repetition
  and the plan exposes the extra cost. The TP estimate sums the replication of
  shared tensors per rank and reserves the generated header.
  In a merge, scalar arrays under the architecture's namespace are rebuilt from
  each slice's coverage, instead of copying the first one; any tail of special
  layers is preserved once. In `qwen4exp`, `ple.layers` is filtered and rebased
  to the local numbering, while `ple.layer_multipliers`, `ple.head_offsets` and
  `ple.head_vocab_sizes` are preserved whole. `per_layer_token_embd.weight` only
  enters the slice that contains a PLE layer, and the three required
  `output_hc_*` tensors are copied into every slice. The synthetic
  `output_norm.weight` identity injected into every non-final range (to satisfy
  the generic loader) is skipped for `qwen4exp` (`gguf::uses_output_norm`): that
  architecture's final hyper-connection mixer carries that role and the loader
  never calls `create_tensor` for `output_norm`, so the physical slice cannot hold
  an extra unclaimed tensor - that was the cause of "wrong number of tensors" on
  the first slice of a real Qwen3.8 Flash Next. The result is validated by
  `gguf::read_layout` before the backend names it by content.
  `write_virtual_slice` writes the same header with no weights: each source
  tensor points at its bytes in the source file(s) through
  `general.external_data.files`/`tensor_files`/`tensor_offsets`, which the
  llama.cpp fork's loader reads (branch `pom`).
  Tensors with data in the header (the identity `output_norm`) come first.
  `plan::write_virtual_shard` is the apply step that uses it behind
  `POM_VIRTUAL_SLICES=1`; `plan::virtual_slice_refusal` keeps on real
  slicing only a header it cannot read and, when the inference mode is `moe`
  or `auto`, a MoE model in several files (under `dense` llama.cpp reads the
  parts itself), since the MoE external executor follows a virtual slice into one source file
  (`moe::format_q4_0::map_tensor_source` in the engine)
  (`docs/superpowers/specs/2026-11-07-layer-range-deployment-design.md`, Part A).
- `kv_budget.rs` - a pure byte policy of the KV cache. It replicates the row size
  of the supported types and refuses a width that does not close a block or any
  overflow, without consulting FFI or a device.
- `storage.rs` - computes the recursive usage of the slices folder, records and
  ignores unreadable children. The retention routine stays available for an
  explicit action, but slicing does not call it: generating a range does not
  silently erase earlier slices.
- `provider.rs` - the `ModelProvider` contract, the common types of discovery,
  download, progress, license, storage and manifest.
- `ollama.rs` - the optional Ollama client; generation keeps the backend's long
  timeout. `is_available` returns the last cached value immediately and schedules
  a 500 ms probe at most every 15 s, so `/health` does not open a connection to
  the optional service on every request.
- `huggingface.rs` - the `HuggingFaceProvider` implementation over native HTTP,
  with input validation, range resumption, its own cache and a local manifest.

The Hugging Face token is resolved on each request in this order: `HF_TOKEN`,
`HUGGING_FACE_HUB_TOKEN`, the secret in `pom.db` and, last, the cache file
(`HF_TOKEN_PATH`, `HF_HOME/token` or
`~/.cache/huggingface/token`). The UI's configuration lives in memory in the
`HfTokenStore` and in the `pom.db`, so saving on the screen updates authentication
without restarting the node.
No endpoint returns the raw token; errors keep going through `redact_sensitive`.

The internal messages of `HfError` keep going through `redact_sensitive`, which
accepts borrowed or owned text without changing the redaction of tokens,
authorization headers and `HF_TOKEN`. The owned text test lives in `provider.rs`;
the suite also covers that public errors do not return provider details.

The provider uses the Hub's JSON endpoints and the `resolve` URL for search,
details, files, license, authentication and downloads. The download streams to
`.pom-hf/partials`, resumes with `Range` and `If-Range` only after validating the
size and the remote identity, cancels by removing the partial, and verifies
SHA-256 before finalizing. The backend keeps the status JSON compatible with the
screen, but the `cli.installed` field is true when the Rust provider is
available. Internal callers must consume `ModelProvider`/`ProgressSender`, never
build Hugging Face URLs directly.

`pom.rs` keeps the per-model original manifest: in the only v1 format, `pom_id`
is the sha256 of the source GGUF, `parts` gathers alternative layer slices and
`tp_parts` gathers Tensor Parallelism shards. New slices of the same model are
merged into the same folder and the same manifest named after the folder; the
catalog only exposes the parts whose file exists on disk. `catalog::ShardSet`
keeps the two lists separate and preserves `tp_rank`/`tp_size` so the UI groups
by size and shows `1ofN`.
A standalone range finds the folder by `pom_id`, adds its part to the existing
manifest, and only reuses a part when the file matches by SHA-256 and size.

`deployment::DeploymentIdentity` persists the proof of the artifact that went
into service: `pom_id` for a round of slices, or the SHA-256 of the whole GGUF.
The deployment's full record lives in `backend::db::PomDb`; the JSON in
`deployments/` is a legacy source kept for the throwaway tool. Old records without
that proof can only be associated by canonical path on the same peer, never by
name.

`Deployment` is versioned (`DEPLOYMENT_VERSION = 2`): `versions:
Vec<DeploymentVersion>` keeps every configuration that already ran under the same
`deploy_id` and `active_version` points to the one in the air; `nodes` is the
flattened list of the active version, kept in sync. What distinguishes a version
is the whole deployment's `RuntimeParams` (`params`); per-machine adjustments
enter the `nodes` snapshot but do not fork a version. `upsert_version` reuses the
version with equal `params` or appends `max+1`; `set_active_version` only swaps
the active one; `ensure_versions` synthesizes the v1 of a legacy v1 record
(without `versions`), and is called on every read (`PomDb`, `find_record`,
listing). `matching_shape` is the pure function that decides whether a
`POST /api/ui/deploy` reuses a `deploy_id`: same `kind`, same proven
`DeploymentIdentity` and the same set of `(node_id, range, tp_rank)`. Without a
proven identity it mints a new id.

`DeploymentKind::Mixed` represents a whole GGUF and a sliced head in the same
deployment. `Deployment::append_version` always creates `max+1` for topology
changes, even with the same `RuntimeParams`, and keeps the new version's set of
machines in the history. A partial removal preserves the version as active; the
last removal writes an empty active version, which is valid for the history but
is not chosen by `active()` as a live deployment.

The last entry of a size-based plan is always the rest of the model. The storage
limit applies only to the slices folder; whole-model folders are read-only and
never enter automatic retention.

`lastslice.rs` serializes the whole `NodeConfig` with `serde(flatten)`. So the
local scheduler settings (`batching_enabled`, the queue limits, the microbatch
window and limits) also survive a restart and come back with the same slice in
service. The global KV cache types (`cache_type_k` and `cache_type_v`) and the
fit safety margin are serialized in the effective state too. The cache types keep
being refilled from the `pom.json` by the `NodeRuntime` before loading the model;
the margin comes from the deployment's parameters. `save_at` is crate-private so
the `/health` watcher writes to the configuration path it captured at startup,
without following a later change of `POM_CONFIG` in the process.

The slicer first classifies GGUF arrays by namespace: only arrays under the
architecture's namespace, except the structural rope arrays, may be per-layer
metadata. In those arrays, a length at least equal to the original block count is
cut to the local range, preserving the enlargement that supports vectors like
`compress_ratios` with an extra tail. llama.cpp reads the first `block_count`
elements of those vectors; that is why extra values are not preserved in the
slice. Global arrays, including `tokenizer.ggml.tokens` and string arrays, are
copied intact before any length comparison. Length 1 is broadcast and stays as
is; a positive length smaller than `block_count` in an architecture array is a
named refusal with key and size. The four `qwen4exp.ple.*` arrays are handled by
named semantics: `layer_multipliers`, `head_offsets` and `head_vocab_sizes`
represent shared table structure and are copied whole into every slice;
`ple.layers` is the list of PLE layers, filtered to the range and rebased to the
local index. The tensor `per_layer_token_embd.weight` accompanies only the slices
that contain one of those layers. `deepseek41` follows the same model: its
`attention.kv_source_layers`, `attention.indexer.source_layers` and `ple.layers`
are filtered and rebased, `attention.indexer.candidate_source_layer` is rebased
or left out, and the other Engram arrays are copied whole. Its valid cuts come
from `GgufLayout::restricted_slice_starts`; `merge` and tensor parallel refuse
the architecture by name.

### Multimodal projector

`catalog.rs::resolve_mmproj_sibling` reads only the GGUF headers of the
candidates and exposes every compatible projector by shared identity or projection
dimension. Quantization never chooses `mmproj`. The identity comes from
`general.name`, `general.basename` and `general.base_model.*`; when there are
slices, `ShardSet.mmproj_candidates` inherits the whole model's candidates through
the manifest label, without comparing the model's payload or computing SHA-256.
The first ordered candidate is only the interface's pre-selection.
`mmproj_reason` explains how to resolve an absence. The interface must not scan
the disk: it shows the candidates and sends the explicit choice in the
deployment. The projector is not a selectable model in the general catalog.

### Separate MTP draft

`catalog.rs::find_mtp_draft_sibling` looks for a sibling `.gguf` in the same
directory with the `mtp-` prefix and chooses by the closest quantization. There is
no established convention for that file outside Unsloth itself, and a substring
would risk a false positive on a model whose name contains "mtp". Unlike mmproj,
this path does **not** live in the central catalog
(`DiscoveredModel`/`ShardSet`/`Plan`): the discovery happens directly on the
machine that loads the model (`webui::run_apply`, `webui::reconfigure_local`),
because only it needs the file - neither the deploy orchestrator nor the other
machines of the pipeline. No opt-in in the UI: the existing `mtp_enabled` switch
is already the operator's intent, and finding (or not) the file only decides how
MTP runs when the target model does not carry `nextn` built in. See
`crates/llama-engine/src/mtp.rs` for the full mechanics
(`LoadedModel::load_mtp_draft`, `EngineConfig.mtp_draft_path`) and the
prerequisite on the fork's side: the target must export `h_nextn` from its own
graph (`qwen4exp.cpp` gained that in `pieceofmindcloud/llama.cpp`,
`pom-tensor-parallel`, without regressing the early cut of the last layer's rows
when MTP was not requested).

## Defects noted

- `deepseek41` slices are written, but a pipeline across them does not run yet.
  `deepseek41.cpp` keeps `pre_prev`, the pre-mix of the previous layer's FFN,
  which is not part of the residual, so a slice starting mid-model would
  collapse its first attention input with the identity instead. The same graph
  has no expanded `hc` input and sets `n_embd_out = n_embd`, so
  `validate_hidden_tensor` refuses the wide residual on the receiving peer
  instead of computing a wrong answer. Serving needs the fork to accept an
  expanded input and carry `pre_prev` across the boundary.

- `ModelSource::Folder` is labelled `"Pasta"` (`catalog.rs`, the source label
  the catalog sends), so the Full models screen shows a Portuguese word in the
  English interface. The label should be a code the UI translates.
- The engine's `gguf::read_layout` (pom-plugin-llama.cpp, pinned `rev`) refuses
  in Portuguese ("dtype GGML 99 desconhecido no tensor ...", "tem comprimento
  3"), so `splitter::tests::preview_refuses_an_unknown_dtype_by_name` and
  `unknown_architecture_array_length_is_refused_by_name`, which expect the
  English text, fail. The fix belongs to the engine repository, then a `rev`
  bump here.

## Maintaining this file

This file is a short map of the folder. When adding model metadata or changing
the boundary between catalog, GGUF and planning, update here and the corresponding
subsystem in the same commit. Implementation details live in the code or in
`docs/subsystems/`.

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.