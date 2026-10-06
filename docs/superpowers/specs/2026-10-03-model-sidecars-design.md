# Model sidecar files: vision projector (mmproj) and MTP draft

Date: 2026-10-03 - Status: proposal for review

## Problem

Deploying a sliced `qwen4exp` model (Qwen3.8 Flash Next, UD-Q2_K_XL) does not
start MTP, and vision only shows up when the projector sits in the same folder
as the model.

- The MTP draft is only found as a sibling of the loaded file
  (`catalog::find_mtp_draft_sibling`, called from `webui/jobs_apply.rs`). For a
  slice the loaded file is `<shard_dir>/<run>/<sha>.gguf`, so an `mtp-*.gguf`
  under a whole-model folder is never found.
- Vision only has candidates when the catalog finds an `mmproj` next to a whole
  model with the same label (`resolve_mmproj_sibling`). When the whole model is
  not on that machine, or the projector lives in another folder, there is no
  candidate.
- The deploy screen has one narrow vision select that does not say which
  machine holds the file, and has no MTP select.
- Sidecars are hidden from the catalog (`mmproj*`, `mtp-*`), so there is no
  place to pick them.

Observed layout on one machine: the whole Q2_K_XL model in one models folder,
its slices in the shard folder, the MTP draft in the folder of a different
quantization of the same model, and the projector loose at the root of another
models folder.

## Decisions

- A sidecar missing on the machine that needs it is **copied over the network**
  before launch, like slices.
- The model -> sidecar link is **stored on the principal**, valid network-wide,
  and resolved to each machine's local path.
- Vision loads on the **head** (layer 0); an external MTP draft loads on the
  **tail** (last layer). This is the current runtime rule and does not change.

## Design

### 1. Sidecar inventory (per node)

Each node's catalog lists sidecars found in every folder it already scans
(`model_dirs`, `shard_dir`), separately from models:

```
Sidecar {
  kind: "mmproj" | "mtp",
  path, file_name, size_bytes,
  architecture,   // "clip", or the model architecture (e.g. "qwen4exp")
  dim,            // clip.vision.projection_dim | <arch>.embedding_length
  general_name,
  quant,          // when the file name carries one
}
```

Recognition uses GGUF metadata, not only the name:

- **mmproj**: `general.architecture == "clip"` (or `general.type == "mmproj"`).
- **mtp**: `<arch>.nextn_predict_layers > 0` and the file does **not** carry the
  model blocks (no `blk.0.*` weights; a few dozen tensors). The `mtp-` prefix
  stays a hint, not a requirement - the observed draft's `general.name` says
  nothing about the model it belongs to.

Compatibility with a model (same rule for the link and for the plan):

- mmproj: `projection_dim == embedding_length` of the model (the current
  `validate_mmproj_pair` rule).
- mtp: same `architecture` and same `embedding_length`.

Exposed in `/api/ui/models` (new `sidecars` field) and, aggregated per peer, in
`/api/ui/cluster/models`. The deploy survey reads the same field.

### 2. Persistent link on the principal

Table in the principal's `pom.db`:

```
model_sidecar_links (
  model_key TEXT,     -- model name: `model.name` of the `.pom` manifest, or the
                      -- whole GGUF name without `-0000N-of-0000M`
  kind TEXT,          -- 'mmproj' | 'mtp'
  mode TEXT,          -- 'file' | 'embedded' | 'none'
  file_name TEXT,     -- portable identity of the file
  size_bytes INTEGER,
  updated_at TEXT,
  updated_by TEXT,
  PRIMARY KEY (model_key, kind)
)
```

The file is stored by **name + size**, not by path: the same file lives in
different folders on each machine, and that is how each machine resolves it.

Routes:

- `GET /api/ui/model-sidecar-links` - same profiles that already read the
  catalog. A node that is not the principal reads it from the principal over
  the signed control channel (the same path API key registration uses).
- `PUT /api/ui/model-sidecar-links` - admin only; a secondary admin forwards it
  signed to the principal.

With no saved link the current defaults apply: verified sibling for vision,
`mtp-` sibling for MTP, embedded `nextn` when the GGUF has it.

### 3. Shards and Models screens

Each slice set (Shards) and each whole model (Models) gets two combos:

- **Vision**: "No vision" + every compatible mmproj.
- **MTP**: "Embedded in the model" (only when the GGUF has `nextn`), "No MTP" +
  every compatible MTP draft.

Each option shows the file name and, on a second line, the machines that hold
it with the path on each. Incompatible files are not listed; with none
compatible the combo says why. Saving writes to the principal; the list shows
the current link.

### 4. Deploy screen

- Two selects: **Vision (head)** and **MTP (tail)**, prefilled from the link.
- Each option shows name, machines and path, wide enough to read (each select
  gets its own row instead of the current narrow slot).
- The plan states, for head and tail: "already has the file" or "will copy X GB
  from <machine>". If nobody in the network holds the chosen file, the plan is
  blocked and names it.

### 5. Plan and launch

- `planning.rs` resolves the link (or the screen's choice) per step: the head
  gets `mmproj`, the tail gets `mtp`. For each, a file with the same name and
  size on the target machine is used in place; otherwise a source is chosen and
  added to `transfer_bytes`.
- `LaunchTarget` gains `mtp_draft_path: Option<String>` (next to the existing
  `mmproj_path`). `jobs_apply.rs` uses the explicit path; without it, it keeps
  the sibling lookup.
- Copy: the existing pull by path, which lands in `<shard_dir>/recebidos/`
  (a folder the catalog scans) and is checked by the MD5 the receiver computes.
  The source only serves a path outside the shard folder when that path is in
  its own sidecar inventory (a new rule in `servable_path`, in the same spirit
  as grants). The target receives the file by name + size and finds it on its
  own disk at apply time, so the catalog cache never hides a fresh copy.

## Out of scope

- Choosing the head or tail machine because of where a sidecar lives.
- Copying sidecars outside a deployment (manual sending stays in Transfers).

## Tests

- Catalog: recognizes mmproj and MTP from metadata (including an MTP draft
  without the `mtp-` prefix), rejects incompatible files, never lists sidecars
  as models.
- Link: write/read on the principal; a secondary forwards; a non-admin cannot
  write.
- Plan: head gets mmproj and tail gets MTP; a local file is reused by
  name + size; a missing one becomes a transfer; missing network-wide blocks.
- Launch: an explicit `mtp_draft_path` reaches the tail's `NodeConfig`.
- Transfer: a listed sidecar is served; a path outside the inventory is refused.
- Frontend: Shards/Models combos, deploy selects with machine and path.
