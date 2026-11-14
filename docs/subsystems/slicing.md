# Subsystem: slicing GGUF models

This subsystem cuts a whole GGUF model into slices by layer range and joins
adjacent slices back into a single file. It lives in `crates/node/src/models/`
(`catalog.rs`, `plan.rs`, `splitter.rs`, `model.rs`, `lastslice.rs`) and is
consumed by `crates/node/src/backend/` (`webui/`, `deploy/`, `gateway/`). The
GGUF reader is the engine's `gguf` module: it lives in `llama-engine` (the
private `pom-plugin-llama.cpp` repository) and `crates/node/src/main.rs`
re-exports it, which is why the node code writes `crate::gguf::`. Slicing is
local and on demand: each peer extracts
its own range from its own copy of the GGUF - no weight travels over the
network.

## 1. Discovery and catalog

`catalog.rs` scans `shard_dir` (default `<project_root>/models`, the only
folder a peer can pull from) and `model_dirs`, and builds the `Catalog` with
`models` and `shard_sets`. Each directory is read for what it is
(`detect_source`): `ModelSource::{LmStudio, Ollama, Local, Folder}`. The scan
order matters: `shard_dir` first (the reading that decides whether a peer can
pull the slice is the slice one, `ModelSource::Local`), then `model_dirs`
deduplicated by resolved path - a symlink is not scanned twice.

Image model files (diffusion models, text encoders, VAEs and the rest of what
ComfyUI loads) are read by `image_catalog.rs`, not by the GGUF walk. For a
`.safetensors` or `.sft` file it reads only the header (an 8-byte length and a
JSON table of tensor names and dtypes, refused above `MAX_SAFETENSORS_HEADER`,
100 MiB, or when it or the weights after it are cut short, as an interrupted copy leaves them) and classifies the file by its tensor names,
in a fixed order: `checkpoint` (a denoiser and a VAE in one file), `lora`,
`controlnet`, `vae`, `diffusion`, `clip_vision`, `text_encoder`, `upscale`,
else `other`. `family` is a display hint from distinctive tensor names
(`flux`, `qwen_image`, `z_image`, `sdxl`, `sd15`), else from
`__metadata__["modelspec.architecture"]`; `dtype` is the dtype most tensors
use. Pickle formats (`.ckpt`, `.pt`, `.pth`) are never opened: they are listed
by `stat` as `other`; `.bin` is not listed at all, being too generic. LLM
weights in safetensors are left out too: a part of a sharded set
(`-00001-of-00004`) or a file beside a transformers `config.json` that
declares `architectures` (`is_llm_weights`). Hidden files, `.part` downloads and the
slicer's working folders are skipped, a folder reached twice is read once, and
the classification is cached by path, size and mtime. The rules are tested
against the real headers in `models/fixtures/safetensors/`.

What holds everything up is the **manifest**, not the file name. A part is
named by its own sha256 (`<sha256>.gguf`), and the `.pom` next to it says which
model it belongs to and which range it covers. See `models/pom.rs`.

- `pom::load_dir` - reads the `.pom` files of a model folder. That is where the
  `shard_group` of each listed part comes from.
- `describe` - the range comes from the `pom.start_layer`/`pom.end_layer`
  stamp the slicer writes in the GGUF header. A `-00002-of-00003` part
  (`is_secondary_part`) is skipped.
- The catalog walkers keep discovering when a child entry is unreadable and
  log the path and the cause at `debug`; a missing catalog root is still
  represented as nonexistent.
- `parse_quant` - quantization from the name of a **whole** model; a part
  inherits the `.pom`'s.
- `build_shard_sets` - groups slices by the **original model**
  (`<model>-<short hash>`, the same name as the folder), sorts by range,
  measures the contiguous coverage from layer 0 and derives the total from the
  coverage when the slice does not declare it (`total_layers_inferred`). The
  `complete` flag says whether the coverage closes the model.

The old convention (`<base>_<start>_<end>.gguf`, read by `parse_shard_name`)
was removed: a file name stopped being proof of anything. Slices cut before
that need to be sliced again.

`total_layers` is an `Option` from catalog to plan: an old slice without
`pom.*` metadata declares nothing, and "unknown" and "zero" are different facts
(collapsing them was the 2026-08-10 bug, when a "complete" plan died in
`NodeConfig::validate`). The catalog serves the previous result while it
revalidates in the background: `local_catalog` uses `CATALOG_TTL = 20 s` with
stale-while-revalidate, and `warm_catalog` runs at startup so a slow volume
does not hold the gateway port (measured: the first cold load of
`/api/ui/models` dropped from 11.9 s to 0.18 s with the warm-up). The first
phase enumerates and runs `stat`; the second reads only the required headers. A
terminal error stays in the model's `metadata_error`, without blocking the
others, and the pass publishes `metadata_complete: true`. The backend keeps a
single scan and a single enrichment in flight, even when the screen is
reloaded or the selection changes. A refresh can change the inventory, but
keeps metadata by `path + size + mtime` identity; only a new model or a changed
file reads the header again. Catalog enrichment does not walk the tensor table,
does not create an `mmap` and does not touch the payload. When the operator
picks a whole model still pending, the planner calls
`POST /api/ui/model-metadata` with the already listed path and publishes only
that header; the neighbors continue in the background without blocking the
selection. If enumeration has not listed the path yet, the selection routes
return the pending discovery state instead of claiming the file does not
belong to the peer; the UI should wait for `metadata_complete: true` and try
again. A full hash is still restricted to the identity, slicing or transfer
boundary.

### Multimodal projectors

The catalog reads the GGUF headers of adjacent `mmproj` files and publishes
`mmproj_candidates` for each model and slice set. When a slice has no local
projector, the whole model's candidates are inherited by the manifest label.
This association does not compare the model payload with `pom_id` and does not
compute SHA-256 to discover or pick the projector. The UI sorts and preselects
the first path, but the operator can pick another compatible format during
deployment.

After enrichment, `backend/webui/` records the catalog in `pom.db`. The record keeps
the path as the local identity of the whole model, the slice set identity,
size, part hash when available, total layers, quantization, range, TP rank,
range ends and the first and last seen times. Recording the catalog does not
compute the SHA-256 of the weights. A refresh marks `present=0` for what
stopped showing up, but does not delete `model_history` or `model_slices`; so
the identification survives after the GGUF or a slice leaves the disk.

## 2. Reading the GGUF: header and tensor table

The engine's `gguf` module reads the GGUF without loading weights:

- `read_info` - only the KV header: architecture, `block_count`, `pom.*`
  (original/total), `shard_range`, `embedding_length` and the cache geometry
  (`attention.head_count_kv`, `key_length` and `value_length`). Each item can
  be a scalar or a per-layer array. What decides whether a file is a slice is
  `catalog::InstalledModel::is_shard`; the copy that existed in `GgufInfo` had
  no caller and was removed. The catalog also reads the multipart fields in
  that header, without mapping or validating the tensor payload.

  Numeric header fields are interpreted by their non-negative integer value,
  not by the rigidity of the signed/unsigned type. That includes
  `split.tensors.count`, which the reference `gguf-split` writes as INT32,
  while `split.no` and `split.count` are UINT16. Negative signed values and
  values that overflow the target are refused; the rule applies to the other
  numeric keys and is not specific to one model.
- The catalog only uses `read_info` and `multipart::catalog_metadata`, which
  read the parts' headers and `stat` the files. `has_nextn_tensors` and
  `read_layout` stay out of the general scan: the first is an optional
  structural proof and the second is needed for real cuts and costs.
- `read_layout` - tensor table -> `GgufLayout` with **real** bytes per layer
  (`layer_bytes`, `embedding_bytes`, `head_bytes`, `other_bytes`,
  `total_bytes`, `layer_kinds`). That is what makes the memory budget honest.
  An unknown ggml type refuses the layout instead of inventing. For multipart,
  the table comes from `multipart::GgufSource` and includes every tensor of
  the set. `head_bytes` includes the architecture-specific output tensors when
  known.
- `validate_slice_ranges`/`validate_slice_tensors` - check the contract the
  loader really enforces, by architecture and tensor table. For `deepseek4`,
  `deepseek4.cpp` requires `token_embd.weight`, `output_norm.weight`,
  `output.weight`, `output_hc_fn.weight`, `output_hc_base.weight` and
  `output_hc_scale.weight`; there is no fallback to `token_embd` nor an
  optional tensor flag. A non-final range copies from the source the real
  tensors that are not synthesized, and is refused before writing the file if
  any of them is missing. The final one is only accepted when all six tensors
  exist.
- `restricted_slice_starts` - `deepseek41` decides the cut from its
  cross-layer dependencies, not from `layer_kinds`. `deepseek41.cpp` resolves
  them inside one graph: a compressed layer reads the KV and the top-k of the
  latest source layer at or before it, Engram hashes token ids that only the
  first slice has (the residual carries none, and an embedding input skips
  Engram without an error), and the layers after the candidate source consume
  its pin. So a slice can only start at a layer listed as both a KV and an index
  source, after every Engram layer and not after the candidate source. The
  published DeepSeek-V4.1 Flash has one such layer, 20. An empty list means the
  model stays whole. `validate_slice_ranges` refuses any other start and
  `snap_end_layer` never returns a cut outside the list.
- `arch_boundaries`/`snap_end_layer` - hybrid models (Mamba + Attention) are
  not a stack of equal blocks; the cut only happens at a type boundary,
  otherwise the block renumbers to the wrong type (`missing tensor` or a
  silently wrong answer).
  A kind change exactly at the index of a start-relative count
  (`LayerCountSemantics::FromStart`, such as the dense prefix of a MoE with
  `leading_dense_block_count = 2`) is not a boundary: llama.cpp picks the
  block's kind by comparing its local index with that count, and the slicer
  rebases it, so any cut keeps each block on its kind. The layout keeps those
  values in `start_relative_layer_counts`.
- `nextn_predict_layers` - MTP/Qwen3.5+: the nextn block only lives in the
  last layer; a slice that does not end at the model's end must zero the
  hyperparameter or llama.cpp aborts the load looking for
  `blk.<last>.nextn.*`.
- `LayerCountSemantics` - metadata llama.cpp compares with the block's local
  index is classified by semantics, not by an architecture name. Counts from
  the start (`leading_dense_block_count`, `hash_layer_count` and
  `n_deepstack_layers`) are rebased by `start_layer` and capped at the slice's
  block count. The count from the end (`nextn_predict_layers`) is zeroed
  outside the final range and kept in it. A start-relative count that is not a
  non-negative scalar integer fitting in `u32` refuses the layout with the key
  and the cause, without copying an ambiguous value.
- `attention.recurrent_layers` - hybrid attention/SSM models, such as
  Qwen3.8-27B: without an explicit array under this key, llama.cpp decides on
  its own which layers are recurrent (they carry `ssm_conv1d.weight`) through a
  formula on the **absolute** layer index (`(i+1) % full_attention_interval !=
  0`). `blk.(start+i)` -> `blk.i` changes that index; without rewriting the
  array, a slice that does not start at layer 0 recomputes with the wrong local
  index and llama.cpp requires `ssm_conv1d.weight` in the wrong local block -
  `missing tensor 'blk.0.ssm_conv1d.weight'` on slice 35-64 of
  Qwen3.8-27B-Q3_K_S, on 2026-08-16, even though the genuine weight was at
  original 36. `splitter::split`/`merge` fix this by writing the explicit
  array, derived from the real presence of the tensor in the source file (not
  from the formula) - see §4.
- `slice_bytes(start, end, is_last)` - bytes of a slice: layers + `token_embd`
  (every slice) + the heads the architecture requires. For `deepseek4`,
  `output.weight` and the three real `output_hc_*` tensors also go into the
  intermediate ranges; `output_norm.weight` stays the trunk's synthetic
  identity and the full head stays in the final range. The
  `required_head_bytes` method reports the additional real payload repeated in
  each intermediate range. On the measured DeepSeek-V4-Flash, that extra is
  about 0.44 GB per range. That is what the plan adds up and the UI shows.
- `ggml_type_info` + `ggml_type_table_matches_ggml` - the type table is checked
  against `GGML_TRUTH` in the same file (a build test breaks if they diverge;
  numbers written by reading llama.cpp - `ggml.h` for the codes,
  `ggml_type_size`/`ggml_blck_size` for the sizes -, never from memory).
  Getting a type size wrong **raises no error**: the slice comes out and only
  the answer is wrong. That is why an unknown type is a refusal, not an
  estimate.

## 2.1 Multipart GGUF

The engine's `multipart` module treats a simple GGUF as a one-part set and a GGUF with
`split.count > 1` as one logical source. The catalog validates `split.no`,
`split.count` and `split.tensors.count`, rebuilds the `N-of-M.gguf` paths and
only reads each part's header and size. The full `GgufSource` path is reserved
for layout and slicing: there the headers and tensor tables are mapped with
`mmap`. The unified table keeps, for each tensor, the part index, the offset
relative to the data section and the size computed from the difference between
offsets.

No weight is reassembled in a temporary file: the slicer picks the tensors by
range and copies each one directly from the mmap of the part that holds it.
The same source is consumed by `gguf::read_layout`,
`splitter::validate_tensor_parallel` and `splitter::shard_tensor_parallel`. A
GGUF without `split.*` stays on the one-part path, with the same behavior.

The catalog only enumerates the first part, but after reading the header it
publishes the summed physical size and the ordered list of parts. If a part is
missing or the declared fields do not add up, the model stays visible with the
concrete reason, with no false byte count and no slicing plan. The hash used
for the identity of a source model is the SHA-256 of the ordered sequence of
parts. That hash is only computed at the slicing or transfer boundary, never
in the catalog's initial response.

The read diagnosis separates two causes that looked the same in the field
measurement. `GgufSource::open` maps every part with `mmap` and keeps no GGUF
fd open to copy weights. The hash walks the parts in physical order once. The
extractor, however, received the tensor table in the GGUF's logical order;
that order does not promise increasing offsets and could cause backward page
faults when the mapped source was larger than RAM.
`splitter::source_read_order` now sorts the accesses by
`(part_index, src_offset)`. That knocks down the hypothesis of one whole pass
per slice as the single explanation: there is one logical hash pass and one
pass over the selected tensors, plus globals legitimately repeated in each
output. Writing all ranges in a single pass is still a separate optimization,
not implemented yet. The field measurement gives the scale: three parts of
about 41.8, 39.5 and 48.1 GB add up to about 129.3 GB for a 119.4 GiB source;
with 276 GiB read while only the first part was written, the observed rate
amounts to about 2.3 effective passes per range. An implementation that walks
the source once in increasing offsets and writes the three outputs in
parallel would tend to reduce the projected read from over 800 GiB to close to
the source's 120 GiB. That is an I/O estimate, not a time promise: there is
still the cost of building three headers and writing about 129.3 GB.
Transfers are still operations on individual POM files. The send endpoint
refuses a whole source that declares multipart, with the advice to keep the
full set or produce POM slices; transferring a whole multipart model was not
generalized in this step.

## 3. Planning

`plan.rs` turns intent (memory or size) into layer ranges. The rounding rule is
to a whole layer and stays visible - the screen shows side by side the
requested target, the resulting range and the real size.

- `plan_split` - per-peer budgets (`PeerBudget { node_id, label, budget_mb,
  has_model }`) -> ranges, greedy from layer 0 up. Each peer reserves
  `DEFAULT_RESERVE_PCT` (15%) for KV cache, activations and runtime; the last
  peer must also fit the output head (`is_last_peer` changes the cost);
  `snap_end_layer` adjusts the cut to the type boundary. It warns about a peer
  left without layers and a peer that does not have the chosen model
  installed.
- `plan_split_with_kv` - the `POST /api/ui/plan` path that charges the cost in
  bytes for each range: real weights from the tensor table plus KV for
  `n_ctx * max_sessions`, with the configured K and V types. The reserve is
  applied to the peer's budget before the arithmetic. The result carries
  `weight_bytes`, `kv_bytes` and `total_bytes`; missing GGUF geometry, a width
  incompatible with a quantized block and overflow refuse the plan instead of
  producing a silent estimate.
- `plan_by_size` - targets in MB (`SizedSlice { target_mb }`) -> ranges
  (`SizedRange { index, start_layer, end_layer, layers, bytes, target_mb,
  is_rest, is_last, too_small, empty }`). `target_mb: None` = "the rest of the
  model". A minimum indivisible layer shows up as `too_small`/`empty`; the last
  entry of the list is always converted to the rest, even if an old client
  sends a numeric target; incomplete coverage is **not** a refusal -
  `SizePlan { covered_layers, complete, missing_range, missing_bytes, blocked,
  warnings }` reports how much is missing.
- `plan_equal`/`even_split` - "split into N": equal in layers (difference <=
  1), with the target of the first ranges filled with the real size and the
  last as the rest. Only accepts 1..=64 slices.
- The result of a `plan_by_size` is a `SizePlan`, serialized as a preview at
  `POST /api/ui/slice/plan` (the screen calls it on every keystroke, with the
  layout cached per file).

  `SizePlan.model_bytes` is the sum of the GGUF files the ranges will write,
  including repeated embeddings, global tensors, header, tensor table,
  alignment and the synthetic norm of non-final slices. `source_model_bytes`
  keeps the size of the source GGUF for comparison. `required_head_bytes`,
  when present, is the real head weight payload repeated by each non-final
  range. Each range's total still includes header, table, alignment and the
  other serialization bytes. The cost refuses by name any unknown GGML dtype;
  no unknown type enters the preview as zero.

`POST /api/ui/slice/plan` also accepts `{ mode: "tensor_parallel", tp_size }`.
In that case the answer is a `TpSlicePlan` with one item per rank, all covering
`0..total_layers-1`. `POST /api/ui/slice` uses the same mode to write all ranks
at once and seal `tp_parts` in the unified manifest. The rank kept in the
manifest is zero-based; the UI shows the one-based operational identifier
`1ofN`, `2ofN` etc. The physical file is still addressed by the content hash,
following the hash storage rule.

The plan endpoint also accepts `range: { start_layer, end_layer }` for a
single range. The screen only offers starts and ends derived from the
architecture, and the backend repeats the validation at click time. The output
is an independent part, with the same per-layer metadata reindexing as the
other slices. `finalize_run` finds the manifest by the source hash and adds
the part without replacing the earlier ones; an existing range is only reused
after checking the file's SHA-256. The slicing path runs no automatic
retention that could silently delete an older range. Since the source identity
is still computed before the cut, a smaller range reduces writing and final
space, but does not remove the full hash read.

### What layer 0 means

Whoever holds layer 0 processes the whole prompt of every request, so that
slice should go to the machine with the most memory. It is the operator's
choice, not the plan's: the screen names the roles ("processes the prompt" /
"generates the tokens").

## 4. Running the slicing

### Cuts chosen during deployment

Deploying a whole model queries `GgufLayout::valid_prefix_end_layers` before
offering a slider. In a dense layout, every whole-layer end is accepted; in a
hybrid layout, only ends derived from the real architectural boundaries are
accepted. `prefix_cut_granularity` is only a description when the spacing is
uniform: irregular cuts are still represented by the explicit list.
`POST /api/ui/model-layer-options` exposes that list, and
`POST /api/ui/slice-ranges` uses the selected ranges to run the same local
slicer and seal the manifest before the transfer.

`extract_shard` (`plan.rs`) runs `splitter::split` in a `spawn_blocking` and
deletes the partial file on error - otherwise the next scan would take it for a
valid slice. The cut goes to a work folder (`pom::scratch_dir`), each part
first written to a `<uuid>.part` and renamed to `<sha256>.gguf` when it ends -
the name is only known at the end, because the name **is** the content. The
sha256 comes from the same pass that writes the bytes (`pom::HashingWriter`),
without costing a second read.

At the end, `pom::finalize_run` stores the parts in
`<shard_dir>/<model>-<short hash of the original>/` and seals
`<model>-<short hash of the original>.pom` there. In the single v1 format,
`pom_id` is the full sha256 of the logical source: in a simple GGUF it is the
file; in multipart it is the ordered sequence of parts, not a reassembled file.
So slicing the same model again does not create another folder or another
manifest: new parts are merged by `sha256`, even when they cover alternative
ranges such as `L0-7` and `L0-3`. The same manifest holds `parts` for the
pipeline and `tp_parts` for the Tensor Parallelism shards.

The merge is protected by a lock per `pom_id` and writes the manifest
atomically. There is no compatibility with any other manifest format.

### Output contract and repairing old slices

The output contract is a property of the architecture, not of the model name.
`gemma3` and `gemma4`, for example, mark `output.weight` as optional in
llama.cpp and fall back explicitly to `token_embd.weight`; that is why the
existing head rule works for them. `deepseek4.cpp` creates all six tensors
listed above with required flags. `output_hc_fn.weight` is not a replaceable
vocabulary head: it transforms the HC state before the norm, while
`output.weight` projects onto the vocabulary.

DeepSeek4 slices already produced without those tensors cannot be fixed with
metadata alone. The loader does not consult `pom.*` to relax the requirement.
There is also no cheap append at the end of the GGUF: the new tensors'
descriptors sit before the payload, and growing the table shifts the existing
data. A repair would have to rewrite the whole slice and still obtain the real
head weights; fabricating an identity for `output.weight` would not preserve
the model. The slicer still refuses early a source that lacks the real weights
for the copy required in the intermediate range; when they exist, the new
slice is produced with the correct descriptors and payloads. It does not
promise a repair tool that would fabricate another file the loader refuses.

`splitter::split` (`splitter.rs`) is the Rust slicer, which replaced the old
Python `split_model.py`:

1. Opens a simple GGUF or every part declared by `split.*` with mmap, validates
   the `GGUF` magic and versions 2/3, without reassembling the weights.
2. Reads the unified table; each tensor's size comes from the **difference
   between offsets** (more reliable than the type table: it copes with unknown
   quants and weight-tied tensors). A tensor with byte_size 0 is a **refusal**,
   not an estimate - slicing would produce a corrupt file. An implausible rank
   is also a refusal.
3. Selects the range's tensors (`want_tensor`), remaps `blk.(start+i).*` ->
   `blk.i.*`.
4. Validates the output contract before creating the target. A non-final slice
   injects an identity `output_norm.weight` in the architectures where that
   adaptation is enough (llama.cpp needs it to read the slice), copies the real
   required tensors from the source and zeroes `nextn_predict_layers` outside
   the last one; it also updates `{arch}.block_count`. Metadata classified as
   a count from the start is rebased by `start_layer` and capped at the range
   size; metadata from the end is only kept in the final range. For
   `deepseek4`, `output.weight` and `output_hc_*` are copied into each
   intermediate range, never invented. The final range keeps the full head plus
   `token_embd.weight`.
5. Hybrid model (any `blk.N.ssm_conv1d.weight` tensor in the table): writes an
   explicit `{arch}.attention.recurrent_layers`, sized and indexed for the
   slice, from which **absolute** layers really have the tensor - never
   reimplementing llama.cpp's formula. A plain model does not get the key.
   `merge` does the same over the 0-based numbering of the joined file.
6. In `deepseek41`, `attention.kv_source_layers`,
   `attention.indexer.source_layers` and `ple.layers` are filtered to the range
   and rebased, and `attention.indexer.candidate_source_layer` is rebased or
   left out when its layer is outside the range. `ple.layer_multipliers`,
   `ple.head_offsets` and `ple.head_vocab_sizes` hold one set of hash
   constants per Engram layer, back to back (`ple.ngram_size` multipliers and
   `(ple.ngram_size - 1) * ple.heads_per_ngram` heads each), and llama.cpp
   checks that count against `ple.layers`: a slice keeps the sets of its own
   Engram layers, with the head offsets unchanged because they index the
   whole table the slice carries. `engram.token_map` is copied whole. The
   generic rule would cut these arrays by layer without any error, since they
   can be longer than the block count. `per_layer_token_embd.weight` only
   enters the slice with Engram layers. `merge` refuses `deepseek41`, because
   the merged header would inherit the first slice's source lists, and
   `validate_tensor_parallel` refuses it by name.

   For every other array, it first decides whether the array is per layer: it must be under the
   architecture's namespace and not be a structural rope array. There is an
   explicit exception for the `qwen4exp` PLE metadata: `layer_multipliers`,
   `head_offsets` and `head_vocab_sizes` describe the shared lookup table, not
   a series indexed by blocks, and are copied whole, byte for byte, into each
   slice. `qwen4exp.ple.layers` is different: it lists the absolute indexes of
   the layers that use PLE, so it is filtered to the range and rebased to the
   local numbering. The `per_layer_token_embd.weight` tensor is only copied
   when the range contains one of those layers. Only the other scalar arrays
   whose length is at least `block_count` are cut by the range's absolute
   indexes and written with the local count. Length 1 stays a broadcast; a
   length below `block_count` is a refusal by name. Global arrays, such as
   `tokenizer.ggml.tokens`, including string arrays, are copied intact before
   the length comparison. The `>=` rule stays for architecture vectors with an
   extra tail, such as `compress_ratios`; selection is by position, without
   repeating a periodic step. In the inverse operation, `merge` concatenates
   the covered values of each per-layer scalar array in slice order and keeps
   any tail of special layers once. Broadcast arrays, global arrays, rope
   arrays and the structural PLE arrays are still inherited from the first
   slice. In the real Qwen3.8 Flash Next GGUF, `layer_multipliers` has three
   values because the PLE n-gram has three positions; `head_offsets` and
   `head_vocab_sizes` have 16 values because they describe the hash heads.
   llama.cpp's `qwen4exp.cpp` reads those full lists as table parameters, while
   `attention.compress_ratios` is still a 48-position vector, one per block.
   Since the source can spread the tensor table across three or more physical
   files, the layout and the copy use the part index resolved by
   `multipart::GgufSource`; there is no two-part limit. The loss observed on
   Qwen3.8 was the filtering of the non-block tensors
   `per_layer_token_embd.weight` and `output_hc_*`, not opening the third part.
7. Writes `pom.original_block_count/start_layer/end_layer` in the KV - the
   metadata that makes the total layer count reliable for new slices.
8. Sorts the payloads by part and physical offset, writes with a 32-byte
   `ALIGN` and copies data in `CHUNK` blocks (8 MB), calling
   `on_progress(0..100)`. That progress is per file, not per job (100% of one
   slice is 1/N of the work); the UI job also exposes the read/hash phase,
   bytes read, current file, rate and ETA when there is a basis to estimate
   it.

### Reuse

An existing file is reused without rewriting: `SliceOutcome { output_path,
bytes, reused }` tells "produced" from "reused", and the UI shows it.

### Shard by tensor (tensor parallelism, path C)

`splitter::shard_tensor_parallel(src, dst, tp_size, tp_rank, on_progress)`
produces a GGUF with the tensors of rank `tp_rank` of a group of `tp_size`. It
is the dimension orthogonal to slicing by layer range: every layer stays
present, just with smaller tensors. There is no name remapping and no change
to `block_count`.

Classification is by name (`tp_role`), Megatron style:

- **Column** (split by `ne[1]`, the output dimension; contiguous slice, no
  reduction): `attn_q/k/v.weight`, `ffn_gate/up.weight`.
- **Row** (split by `ne[0]`, the contiguous input dimension; partial sum to be
  all-reduced): `attn_output.weight`, `ffn_down.weight`. Only valid for a
  tensor whose **input already arrives partitioned** - these two consume the
  output of the "column" ones in the same block.
- **Replicated** (the same on every rank): `*_norm.weight`,
  `token_embd.weight`, rope/pos_embd and the output head `output.weight`.

The head used to be "row" and that was a defect: its input is the output of
`output_norm.weight`, which is replicated, so the hidden state reaching it has
full width (`n_embd`) and is never partitioned. Cutting it by `ne[0]` produced
a head expecting `n_embd/size` input - a width that exists nowhere in the
graph - and llama.cpp refused to load the shard: `check_tensor_dims: tensor
'output.weight' has wrong shape; expected 4096, 128256, got 2048, 128256`
(Llama-3.1-8B-Q4_K_M, 08/17). `validate_tensor_parallel` could not catch it: it
only checks whether the dimension **divides**, and 4096/2 divides. Cutting the
head by vocabulary (`ne[1]`) would be the classic alternative, but would need
an all-gather of logits - the fork's collective is a reduce - and rewriting the
vocabulary per rank, which would break the tokenizer. Replicating costs memory
and is correct; `token_embd.weight` has the same shape and was already
replicated.
- **Refused** (not safely shardable, v1): 3D MoE tensors (`ffn_gate_exps`
  etc.), unrecognized names, and `output.weight` tied to `token_embd` (weight
  tying).

### The collective must sum twice per layer

There are two row-parallel points per block - the attention output projection
and `ffn_down` -, so a decode on a 32-layer model fires **64** reduces. The
engine counts them and warns when the number does not match (its `llm` module,
after `llama_decode`).

That is a safety net against the 08/18 defect: the fork had `tp_reduce` in a
**single** overload of `build_attn` (`attn_no_cache`), and llama.cpp has seven.
Every model with a KV cache only summed the FFN - 32 reduces instead of 64 -
and each rank ran attention with half of the heads without ever receiving the
other half.

The symptom is the most dangerous disguise in this subsystem: **the answer
comes out grammatical and fluent, only the content is empty**. It does not look
like a math error, it looks like a bad model. That is why the count became an
automatic warning instead of living in the debugger's head.

### Rule stamp: `pom.tp_format`

Every tensor shard writes `pom.tp_format` (`splitter::TP_SHARD_FORMAT`), the
version of the rule that wrote it, next to `pom.tp_size`/`pom.tp_rank`.
`extract_tp_shard` only reuses an existing file when all three match; a
missing or different stamp means regeneration, with the reason in the log.

**Whoever changes what the shard contains bumps this number** - cut axis,
rewritten hyperparameter, a tensor that becomes replicated.

It exists because of a whole lost day: the reuse criterion was "exists and is
larger than 1 KB", which does not tell a current shard from one written by a
buggy version. On 08/17 a CUDA server loaded the same `_tp0of2.gguf` with an unsplit
header through three fixes in a row, and the log announced "reusing" on every
attempt - the symptom was identical to the already fixed defect, which sent the
search into code that was already right.

### The header is split together with the tensors

Cutting the tensor is not enough: the shard's KV must describe the tensor the
shard contains. Three keys name exactly the partitioned widths and are
rewritten as `value / tp_size`:

| Key | Dimension it describes |
|---|---|
| `{arch}.attention.head_count` | columns of `attn_q` (`n_head * head_dim`) |
| `{arch}.attention.head_count_kv` | columns of `attn_k/v` (`n_head_kv * head_dim`) |
| `{arch}.feed_forward_length` | columns of `ffn_gate/up` and rows of `ffn_down` |

`embedding_length` and `block_count` do **not** change: the residual axis
crosses the all-reduce whole, and every layer stays present on every rank.
Letting the three keys through untouched was the 08/17 defect - the tensor came
out halved while the header promised full width, so llama.cpp refused with
`'blk.0.attn_q.weight' has wrong shape; expected 4096, 4096, got 4096, 2048`.

The **head count** is divided, never `head_dim`: that is what keeps each head
whole on a single rank. A head split in half would give wrong attention with no
shape error to flag it. A width that does not divide by `tp_size` is a refusal
naming the key, on both paths (`validate_tensor_parallel` on the screen and
`shard_tensor_parallel` on write).

The row split requires `ne[0]` to divide by `tp_size` at a ggml type block
boundary (otherwise the cut splits a quantization block and the tensor becomes
unreadable); the column split requires `ne[1]` divisible by `tp_size`. Both
refuse instead of estimating, under the same rule as the rest of the slicer.

The output metadata is `pom.tp_shard=1`, `pom.tp_size`, `pom.tp_rank` (through
`brand::gguf_key`). A shard is NOT a self-sufficient model: today's llama.cpp
does not load it alone - it is the half the llama.cpp fork (path C) loads
together with the all-reduce collective. Tested with the synthetic GGUF
(`test_gguf_bytes`) for rank 0 and rank 1, including the refusal of an invalid
group.

The same decision has a dry run: `splitter::validate_tensor_parallel(src,
tp_size)` reads the tensor table and returns `Err` naming the tensor that cannot
be split, **without writing anything**. `shard_tensor_parallel` calls it as its
entry gate (there is a single classification), and the route
`POST /api/ui/tp/check` (`webui::tp_check`) exposes it to the deploy
coordinator, which refuses the tensor-parallel group **on the deploy screen**
(before the job) instead of letting the apply fail halfway. MoE/SSM/weight-tied
and dimensions that do not divide land here.

`plan::extract_tp_shard` wraps this for the `apply` path (webui): it runs in a
`spawn_blocking`, reuses an already complete file and writes the
content-addressed part, like any other.

### Virtual slices (header only)

`splitter::write_virtual_slice` writes the header `split` would write and no
weights: three `general.external_data.*` keys give, per tensor, the source file
and the absolute offset of its bytes, and only the injected identity
`output_norm` keeps its data in the file. A llama.cpp build with the fork's
external tensor data loads it as the same model as the real slice. A node
uses it at apply time behind `POM_VIRTUAL_SLICES=1` (`deployment.md` section
8); see
`docs/superpowers/specs/2026-11-07-layer-range-deployment-design.md`.

## 5. Merge

`splitter::merge` joins adjacent slices into a single file (`MergePart { path,
start_layer, end_layer }`). Used by the deploy with `merge` on: a machine that
already has two adjacent ranges becomes a merged unit, serving the union.
`parse_merged_src` validates magic, version and plausibility; tensors are
selected by `blk.*` (remapped to their original place) + output head (only
from the last) + `token_embd` (only from the first); a slice with a tensor of
byte_size 0 is a refusal ("merging would produce a corrupt file"). The output
file is written as a temp with a unique suffix and renamed at the end - a
half-written file never carries the final name.

## 6. The served slice survives a restart

`lastslice.rs` records in `<config>.last-slice.json` the slice this node serves
(path derived from the config, otherwise two nodes on the same machine
overwrite each other). The record carries a **state**, and the state is law:

- `pending` - written by `runtime::apply`. It only records what `apply`
  decided, not what the server managed to load - it resumes nothing.
- `serving` - written **only** by `spawn_health_watch`, when the inference
  server's `/health` answers `model_loaded`. It is the only point in the code
  that proves the slice is live, and the only one that authorizes resuming it
  on restart.
- deleted - by `stop` (the operator's decision) and by the branch that gives
  up at `MAX_RESTARTS`. An orphan file (no live node) resurrects nothing.

At startup the file is **consumed and deleted** (`take`): `Serving` reapplies
the slice; `Pending` with its attempts exhausted
(`MAX_AUTOMATIC_ATTEMPTS`), or a `Pending` that `resume_attempt` refuses, calls
`note_unloadable_slice` (the reason goes
to `status.detail`, which is what the panel shows - an empty, silent node is
indistinguishable from a machine that never got a slice). Without this
mechanism a CUDA kernel segfault (2026-08-13) took the node down, the container
restarted and the node came back without a slice - silently dropping out of
the pipeline. The flip side is the infinite loop of 2026-08-15: recording in
`apply` what only `/health` knows made a slice that does not fit in memory kill
the process before loading, the container restart, and the same slice
resurrect forever (`MAX_RESTARTS` is an in-memory counter and dies with the
process). The end-to-end reproduction lived in a handoff folder that has since
been removed from the repository.

## 7. Memory estimate and profiles

`model.rs` has `ModelProfile { name, total_layers, memory_per_layer_mb,
overhead_mb, description }` and `guess_from_name`, which estimates the profile
from the file name when the slice carries no metadata (see defects below).

## 8. Routes

| Route | Role |
|---|---|
| `GET /api/ui/models` | local catalog (models + shard_sets + scanned directories) |
| `POST /api/ui/model-metadata` | reads only the header of the selected whole model and publishes it in the catalog |
| `POST /api/ui/slice/plan` | slicing preview (`plan_by_size`/`plan_equal`) |
| `POST /api/ui/slice` | runs local slicing (`slice_locally` -> `extract_shard`) |
| `POST /api/ui/model-layer-options` | prefix cuts accepted by the selected whole GGUF |
| `POST /api/ui/slice-ranges` | materializes ranges of the whole GGUF and returns paths by hash |
| `POST /api/ui/plan` | cluster budget plan (delegates to the peer that owns the GGUF when needed) |
| `POST /api/ui/apply` | applies a slice (`skip_split`, `extract_shard` or `merge_parts`) |

## 9. Tests

`splitter.rs` exposes `test_gguf_bytes(n_blocks)` - a small but structurally
valid GGUF, shared with the `webui` tests to exercise the merge path with real
files. In `plan.rs`, the tests prove: a target smaller than one layer is
refused with the number in the message; slices that do not cover the model
report how much is missing (`missing_range`) without blocking; asking for more
slices than the model has leaves the extra ones empty; the
`nextn_predict_layers` cut only applies on the last layer.
`ggml_type_table_matches_ggml` breaks the build if the Rust type table diverges
from `GGML_TRUTH` in the same file (the engine's `gguf` module) - numbers written by reading
llama.cpp, not from memory.

## 10. Defects noted

- `deepseek41` slices are written, but a pipeline across them does not run yet.
  `deepseek41.cpp` keeps `pre_prev`, the pre-mix of the previous layer's FFN,
  which is not part of the residual, so a slice starting mid-model would
  collapse its first attention input with the identity instead. The same graph
  has no expanded `hc` input and sets `n_embd_out = n_embd`, so
  `validate_hidden_tensor` refuses the wide residual on the receiving peer
  instead of computing a wrong answer. Serving needs the fork to accept an
  expanded input and carry `pre_prev` across the boundary.

- **DEFECT NOTED: the `plan.rs` module doc talks about `split_model.py`**
  (`crates/node/src/models/plan.rs:9-12`) - the doc says slicing "stays in
  `split_model.py`, which has always taken `--start-layer`/`--end-layer`", but
  the code calls `splitter::split` (the Rust one). The Python script no longer
  exists in the repository.
- **DEFECT NOTED: `guess_from_name` invents layers and sizes**
  (`crates/node/src/models/model.rs:109`) - an old slice without `pom.*`
  metadata has no way to correct the total, and the guess comes from the file
  name heuristic (any name containing `qwen` becomes `qwen35` 32/380/700;
  "9b"/"8b"/"7b" -> 7b-llm 32/380/700; "3b"/"4b" -> 3b-llm 32/200/400;
  "1b"/"2b" -> 1b-llm 28/130/300; "small-llm" -> 40/600/1200; no match ->
  "unknown-llm" 32/300/600). The catalog derives the total from coverage when
  it can, but `guess_from_name` is the last resort - it already caused the
  2026-08-10 bug (a "complete" plan that died in `NodeConfig::validate`).
