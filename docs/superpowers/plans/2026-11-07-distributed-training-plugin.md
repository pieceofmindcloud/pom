# Distributed training plugin - implementation plan

Spec: `docs/superpowers/specs/2026-11-07-distributed-training-plugin-design.md`
Date: 2026-11-07

Repository: a new `pom-plugin-train` checkout next to the other
`pom-plugin-*` repositories, `plugin_code` = `train`. Every task is TDD:
failing test -> minimal implementation -> passing test -> commit. Mark a
finished step with `- [x]`.

Layout at the end:

```
pom-plugin-train/
  src/lib.rs          dispatch (template) + rpc.train.* / peer.train.*
  src/run.rs          run model, state machine, persistence
  src/roles.rs        authorization of rpc and peer calls
  src/chunks.rs       staged blobs, chunk reads, SHA-256 verification
  src/node_api.rs     host_api client (copied from pom-plugin-ghostly)
  src/coordinator.rs  outer loop on the coordinator node
  src/supervisor.rs   Python worker lifecycle (recipe B)
  src/worker_link.rs  loopback JSON-lines protocol to the worker
  worker/             Python package pom_train (pytest)
  ui/                 Training runs / New run screens
  tests/contract.rs   template contract tests + plugin specific ones
```

Constraints from the plugin contract that shape the tasks: `query` returns
within 3 s, so all slow work is on threads the plugin owns; refusals are `Ok`
bodies `{"error": ...}`; peer bodies are at most 256 KiB; children never
inherit stdin/stdout and die with the host.

---

## Phase 0 - Scaffold

### Task 0.1 - Create the repository

- [x] From the POM root:
  ```bash
  bash .claude/skills/creating-pom-plugins/scripts/new-plugin.sh \
    ../pom-plugin-train --code train --name "Training"
  cd ../pom-plugin-train && git init -b main
  scripts/build-ui.sh && POM_UI_REQUIRE_DIST=1 cargo test --locked
  ```
- [x] Contract tests pass unchanged. Commit "Scaffold from the template".

---

## Phase 1 - Python worker (single node first)

The worker is testable without POM. It is a loopback JSON-lines server: the
plugin sends commands, the worker answers and emits events.

### Task 1.1 - Protocol

Test `worker/tests/test_protocol.py`:

```python
from pom_train.protocol import parse_command, event

def test_parse_known_command():
    cmd = parse_command('{"id": 1, "cmd": "status"}')
    assert cmd.id == 1 and cmd.cmd == "status"

def test_unknown_command_is_rejected():
    import pytest
    with pytest.raises(ValueError):
        parse_command('{"id": 2, "cmd": "rm -rf"}')

def test_event_is_one_line():
    line = event("progress", step=3, loss=1.5)
    assert "\n" not in line and '"kind": "progress"' in line
```

- [x] Implement `pom_train/protocol.py`:

```python
import json
from dataclasses import dataclass

COMMANDS = {"status", "load", "inner_steps", "export_delta",
            "apply_outer", "save", "merge_export", "stop"}

@dataclass
class Command:
    id: int
    cmd: str
    args: dict

def parse_command(line: str) -> Command:
    raw = json.loads(line)
    if raw.get("cmd") not in COMMANDS:
        raise ValueError(f"unknown command {raw.get('cmd')!r}")
    return Command(int(raw["id"]), raw["cmd"], raw.get("args") or {})

def event(kind: str, **fields) -> str:
    return json.dumps({"kind": kind, **fields}, separators=(", ", ": "))
```

### Task 1.2 - Dataset sharding

Test: `shard(rows, rank=1, world=3)` returns rows 1, 4, 7...; every row lands
in exactly one shard; JSONL with `messages` or `prompt`/`completion` both load;
a malformed line fails with its line number.

- [x] `pom_train/data.py`: `load_jsonl(path)`, `shard(rows, rank, world)`,
  `tokenize(rows, tokenizer, max_len)` using the tokenizer's chat template.

### Task 1.3 - QLoRA trainer, inner steps

Test (marked `gpu`, skipped without CUDA; a CPU variant uses
`hf-internal-testing/tiny-random-LlamaForCausalLM` with `load_in_4bit=False`):

```python
def test_inner_steps_reduce_loss(tiny_model, tiny_rows):
    t = Trainer(tiny_model, lora_rank=8, lr=1e-3, quantize=False)
    t.load_rows(tiny_rows)
    first = t.inner_steps(1)["loss"]
    last = t.inner_steps(30)["loss"]
    assert last < first
```

- [x] `pom_train/trainer.py`: loads the base with `bitsandbytes` 4-bit when
  `quantize`, wraps it with `peft` LoRA, AdamW inner optimizer,
  `inner_steps(n)` returns `{loss, tokens, seconds}`.

### Task 1.4 - Pseudo-gradient and outer step (DiLoCo)

Tests:

```python
def test_delta_is_difference_from_round_start(trainer):
    trainer.begin_round()
    trainer.inner_steps(5)
    delta = trainer.export_delta()
    for name, d in delta.items():
        assert torch.allclose(d, trainer.round_start[name] - trainer.adapter()[name])

def test_apply_outer_with_zero_delta_restores_round_start(trainer):
    trainer.begin_round(); trainer.inner_steps(5)
    trainer.apply_outer({k: torch.zeros_like(v) for k, v in trainer.round_start.items()})
    assert all(torch.equal(trainer.adapter()[k], v) for k, v in trainer.round_start.items())

def test_average_matches_mean(deltas_from_three_nodes):
    avg = average(deltas_from_three_nodes, weights=[1, 1, 2])
    ...
```

- [x] `pom_train/diloco.py`: `export_delta()` (round start minus current,
  bf16, written with `safetensors`), `average(deltas, weights)` (weighted by
  tokens processed), `apply_outer(avg)`: reset adapter to round start, then
  Nesterov SGD step (`outer_lr=0.7`, `momentum=0.9`, from the DiLoCo paper)
  with the average as gradient. Outer optimizer state saved by `save`.

### Task 1.5 - Merge and export GGUF

Test (CPU, tiny model): `merge_export(out_dir, quants=["f16"])` produces a
file that `gguf.GGUFReader` opens, with `general.architecture` set and the
tensor count of the base.

- [x] `pom_train/export.py`: `merge_and_unload()`, `save_pretrained` to a
  temp dir, run the fork's `convert_hf_to_gguf.py` (path from the run config),
  then `llama-quantize` per extra quant. Write a sidecar
  `<name>.train.json` (base, dataset SHA-256, hyperparameters, final loss).

### Task 1.6 - Server loop

Test: start `python -m pom_train.serve --port 0` as a subprocess, read the
chosen port from its first stdout line, send `status`, get `{"id": 1, "ok":
true, "phase": "idle"}`; `stop` exits with code 0. The worker refuses non
loopback binds.

- [x] `pom_train/serve.py`. Commit "Worker: protocol, data, trainer,
  DiLoCo, export".

---

## Phase 2 - Plugin core (Rust)

### Task 2.1 - Run model and state machine

Tests in `src/run.rs`:

```rust
#[test]
fn a_run_moves_forward_only() {
    let mut run = Run::new(sample_config());
    assert_eq!(run.phase, Phase::Draft);
    run.start(vec![node("a"), node("b")]).unwrap();
    assert_eq!(run.phase, Phase::Preparing);
    assert!(run.finish().is_err()); // cannot finish before training
}

#[test]
fn config_validation_names_the_field() {
    let mut c = sample_config();
    c.lora_rank = 0;
    assert_eq!(c.validate().unwrap_err(), "lora_rank must be at least 1");
}

#[test]
fn runs_round_trip_through_disk() { /* save to tempdir, load, equal */ }
```

- [x] `Phase`: `Draft, Preparing, Training{round}, Exporting, Done, Failed,
  Stopped`. `RunConfig`: base model path (HF), dataset path, participants,
  `lora_rank`, `lora_alpha`, `inner_lr`, `outer_lr`, `inner_steps`, `rounds`,
  `quants`, `delta_format` (`bf16`|`int8`). Persisted as JSON under
  `workspace_root/.train/runs/<id>.json`, schema field `"v": 1`.

### Task 2.2 - Authorization

Tests in `src/roles.rs`:

```rust
#[test]
fn only_admins_start_runs() {
    assert!(can_manage(&json!({"profile": "admin"})));
    assert!(!can_manage(&json!({"profile": "worker"})));
    assert!(!can_manage(&json!({"profile": "visitor"})));
}

#[test]
fn peer_join_requires_an_admin_caller() {
    for role in ["principal", "admin"] {
        assert!(peer_may_command(&json!({"caller": {"role": role}})));
    }
    for role in ["member", "visitor"] {
        assert!(!peer_may_command(&json!({"caller": {"role": role}})));
    }
}

#[test]
fn a_visitor_node_never_participates() {
    assert_eq!(eligibility(&node_info("visitor")).unwrap_err(),
               "visitors do not take part in training");
}
```

- [x] Implement; wire into `query` so every `rpc.train.*` mutation and every
  `peer.train.*` command checks first, refusing with an `Ok` error body.

### Task 2.3 - Chunked blobs

Tests in `src/chunks.rs`:

```rust
#[test]
fn chunks_reassemble_to_the_same_hash() {
    let dir = tempdir();
    let blob = Blob::stage(&dir, &random_bytes(1_000_000)).unwrap();
    let mut out = Vec::new();
    for i in 0..blob.chunk_count() {
        out.extend(blob.read_chunk(i).unwrap());
    }
    assert_eq!(sha256(&out), blob.sha256);
}

#[test]
fn a_chunk_fits_a_peer_body_after_base64() {
    assert!(CHUNK_SIZE.div_ceil(3) * 4 + 4096 < 256 * 1024);
}

#[test]
fn out_of_range_chunk_is_an_error() { ... }

#[test]
fn receiver_rejects_wrong_hash() { ... }
```

- [x] `CHUNK_SIZE = 160 * 1024` (192 KiB is exactly 256 KiB in base64, over the limit with the JSON around it). `Blob::stage`, `read_chunk`, `Receiver`
  (writes chunks to a `.part`, verifies SHA-256, renames). Staged blobs are
  deleted when their round is acknowledged.

### Task 2.4 - host_api client

- [x] Copy `pom-plugin-ghostly/src/node_api.rs` (loopback check, hidden
  token). Test: `Debug` never prints the token; a non loopback `host_api` is
  refused. Add `peer(node_id, op, params)`, `peers()`, `node()`.

### Task 2.5 - Peer operations

Tests via `Plugin::query` with a crafted `caller`:

- `peer.train.probe` (any member): returns `{eligible, reason?, gpu,
  vram_mb, worker_runtime}`; a visitor node answers `eligible: false`.
- `peer.train.join` (admin caller): accepts a run config, a rank and the
  coordinator `node_id`; a second join for another run while one is active
  returns `{"error": "a run is already active on this node"}`.
- `peer.train.round` (admin caller): `{run, round, inner_steps}` starts a
  round asynchronously and returns at once.
- `peer.train.delta_info` / `peer.train.chunk`: `{sha256, chunks, tokens}` and
  base64 chunk `i` of the staged delta; only the run's coordinator may read.
- `peer.train.apply`: tells the node the averaged delta for round `r` is
  staged on the coordinator; the node pulls it with `peer.train.chunk` on its
  own thread.
- `peer.train.stop`: stops the worker, keeps the checkpoint.
- [x] Each test asserts the call returns in well under 3 s (slow work is
  enqueued to the manager thread).

### Task 2.6 - Supervisor and worker link

- [x] `src/supervisor.rs` from recipe B: manager thread, `PR_SET_PDEATHSIG`,
  `env_clear` plus only `PATH`, `HOME`, `CUDA_VISIBLE_DEVICES`, HF cache dir;
  logs under `plugin_directory()/logs/<run>.log`. Runtime discovery order:
  container image (`docker`/`podman` present) then managed venv under
  `plugin_directory()/venv` (pinned `requirements.lock` with hashes, recipe
  C). Phases `installing`, `starting`, `ready`, `error` published to
  `host.services`.
- [x] `src/worker_link.rs`: connects to the port the worker prints, sends
  commands with ids, routes replies and events. Test against a fake worker
  (a Rust thread speaking the protocol) for: reply routing by id, a worker
  that dies mid command yields `Failed` with the last log lines.

### Task 2.7 - Coordinator loop

Pure logic test with fake peers (trait `Cluster` with `probe`, `join`,
`round`, `fetch_delta`, `publish_average`; an in-memory impl):

```rust
#[test]
fn a_round_averages_every_live_node_weighted_by_tokens() { ... }

#[test]
fn a_node_that_misses_the_round_deadline_is_left_out_of_that_round() {
    // node b never finishes; the round closes after the deadline with a only;
    // b is told to resync at the next round.
}

#[test]
fn inner_steps_scale_with_measured_throughput() {
    // a does 2x tokens/s of b -> a gets 2x inner steps, total kept.
}

#[test]
fn a_crash_resumes_from_the_last_finished_round() { ... }
```

- [x] `src/coordinator.rs`: per round: send `round` to all, wait for
  `delta_ready` (polled via `delta_info`), deadline = 2x the median round
  time, pull deltas in chunks, average in the local worker (`apply_outer`
  input is the average file), stage the averaged delta, send `apply`, save
  checkpoint, advance. After the last round: `Exporting` -> worker
  `merge_export` into `models/` -> `Done`.
- [x] The coordinator also trains (rank 0) unless the operator unticks it.

### Task 2.8 - RPC for the screens

- [x] `rpc.train.runs`, `rpc.train.run` (detail with per node progress and
  the loss series), `rpc.train.eligible` (probes peers, cached 30 s),
  `rpc.train.create`, `rpc.train.start`, `rpc.train.stop`,
  `rpc.train.resume`, `rpc.train.delete`, `rpc.train.logs`. Tests: each
  mutating op refuses a non admin profile; `rpc.train.run` never includes
  paths outside the workspace or any token.
- [x] Commit "Plugin core: runs, roles, chunks, peers, coordinator".

---

### As built (2026-11-07)

- Module names: `agent.rs` (the node side) and `service.rs` (rpc and
  coordinator threads) instead of `node_api.rs`/`worker_link.rs`; the client
  is `cluster.rs`, the worker link `worker.rs`; `sim.rs` runs whole runs in
  memory.
- A late node is not "resynced": it applies the average after its own steps
  (its work is dropped from that round); a node that errors leaves the run
  and comes back only when the run is resumed.
- Worker runtime: `python` and `worker_dir` settings (`rpc.train.settings`);
  the container image and the managed venv install are deferred (open items
  in the plugin `AGENTS.md`).
- `rpc.train.logs` returns the worker log tail; `rpc.train.run` returns the
  history, not live per node progress.

## Phase 3 - Screens

### Task 3.1 - Training runs

- [x] `ui/src/screens/Runs.tsx`: table (name, base model, phase, round x/y,
  participants, last loss), row opens the run detail: loss chart per round,
  per node throughput, logs tail, Stop / Resume, and "Open in Deploy" when
  `Done`. Polls `rpc.train.run` every 2 s.

### Task 3.2 - New run

- [x] `ui/src/screens/NewRun.tsx`: base model (HF folder under `models/`),
  dataset (file picker in the workspace), participants from
  `rpc.train.eligible` with ineligible ones shown with the reason, LoRA rank
  and alpha, learning rates, inner steps, rounds, output quants. If the
  dataset must be copied to peers, list the copies and ask before sending
  (the same consent as deploy transfers).
- [x] i18n keys in `en.json` and `pt-BR.json`, identical sets (the contract
  test checks).
- [x] Commit "Screens: runs and new run".

---

As built: one route with four views in local state (list, detail, new,
settings) instead of separate screens; the New run form does not copy the
dataset (it must already be on every machine), so the transfer consent step is
deferred with the dataset transfer.

---

## Phase 4 - End-to-end on a dev cluster

### Task 4.1 - Two nodes, one LAN

- [ ] Dev cluster with two nodes (`POM_DEV_MODE=1`, plugin mounted).
  Fine-tune a 1-3B instruct model on a small public instruction set for 3
  rounds of 100 inner steps.
- [ ] Check: loss falls across rounds; the GGUF appears in `models/`; Deploy
  slices it across both nodes; `/v1/chat/completions` answers.
- [ ] Record wall time, bytes per sync and tokens/s per node in
  `docs/reports.md` of the plugin.

### Task 4.2 - NAT punched link and cluster relay

- [ ] Repeat 4.1 with one node behind NAT, then forced through a cluster
  relay. Confirm with the node logs that no public relay carried a chunk.

### Task 4.3 - Failure drills

- [ ] Kill the worker of the non coordinator mid round: run continues, node
  resyncs next round. Restart the coordinator node mid run: `Resume` continues
  from the last checkpoint.

---

## Phase 5 - Documentation and release

- [ ] Plugin `AGENTS.md`, `README.md`, `docs/getting-started.md` with a
  screenshot; guides for both screens in en and pt-BR.
- [ ] In POM: add the plugin to `docs/subsystems/plugins-ui.md` only if it
  needed a contract change (it should not). Update the spec status to
  "implemented" with any decision that changed.
- [ ] `scripts/package.sh` for linux-x86_64 (CUDA) first; macOS after the
  Metal open item is settled.

## Out of scope for this plan

Full fine-tuning, pipeline training, unmerged adapter serving, peer
all-reduce and coordinator handover (see the spec's last section).
