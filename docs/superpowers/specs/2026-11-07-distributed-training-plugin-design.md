# Distributed training plugin

Date: 2026-11-07 - Status: proposal for review

## Problem

POM serves models across machines but cannot produce them. An operator who
wants a model adapted to their own data fine-tunes it elsewhere, converts it
to GGUF by hand and copies it back before slicing and deploying it. The
machines of a POM network already have GPUs, identities, a trust chain and
encrypted transport between them; most of them sit idle between requests.

## Goals

- A plugin, `pom-plugin-train`, that fine-tunes a model across several nodes
  of one network in parallel, each node training on its own share of the
  data.
- Training works over the links POM already has (home internet, NAT punched
  connections, a cluster relay), not only over a fast LAN.
- The output of a run is a GGUF placed in `models/`, which the existing slicer
  and deploy flow handle with no change.
- Training traffic, datasets and checkpoints follow the same identity,
  membership and transfer rules as the rest of POM.

## Non-goals (v1)

- Pretraining from scratch.
- Models that do not fit on a single participating node (pipeline training,
  layers split across machines). See "Later phases".
- Serving LoRA adapters at inference time without merging them.
- Training across networks; a run spans one network.

## How the work is distributed

The plugin uses **DiLoCo** style data parallelism (low communication local SGD)
with **QLoRA**:

```
Node A: shard A -> H local steps -> ┐
                                     ├─ outer sync: average adapter deltas -> repeat
Node B: shard B -> H local steps -> ┘
```

- Every participant holds the full base model (4-bit quantized) and trains a
  LoRA adapter on its own data shard.
- Between syncs (H inner steps, typically a few hundred, i.e. minutes) a node
  trains alone with no network traffic.
- At each outer sync the nodes exchange the change of their adapter weights
  (a pseudo-gradient), average it and apply it with an outer optimizer
  (Nesterov momentum, as in DiLoCo).
- Traffic per sync is the adapter size, about 10-100 MB. On a 100 Mbps link
  that is seconds per round of minutes, so a relay or a punched connection is
  enough.
- Heterogeneous machines: each node's H is scaled to its measured throughput
  so that all nodes reach the sync at about the same time.
- A node that drops out is excluded from the current round; the run continues
  with the remaining ones. A node can rejoin at the next round by pulling the
  current adapter.

Classic per-step data parallelism (DDP/FSDP) and pipeline training need
gradients or activations on every step and are out of scope for v1 for that
reason.

## Decisions

### 1. Wrap an existing trainer, do not write one

The worker is Python: `torch`, `transformers`, `peft`, `bitsandbytes`, with
the DiLoCo outer loop taken from **OpenDiLoCo / prime** (Prime Intellect).
Their own peer discovery (Hivemind DHT) is **not** used: POM provides the
peer list and the transport (decision 3). Petals, Psyche/DisTrO and
DeepSpeed/FSDP were considered; they are candidates for the later phases.

### 2. Process layout

```
pom ── C ABI / IPC ── pom-plugin-host ── pom-plugin-train (Rust)
                                              │ spawns and supervises
                                              ▼
                                     train worker (Python, container or venv)
```

The Rust plugin, scaffolded with the `creating-pom-plugins` skill, owns the
screens, the RPC and peer calls, the run state and the worker's lifecycle
(start, stop, restart, log streaming). The Python worker only trains and
talks to the plugin over a local loopback socket.

Runtime: the plugin ships a training container image (CUDA and CPU variants)
and falls back to a managed venv on hosts without a container runtime (Metal).
The POM release image is not changed.

### 3. Rendezvous and transport through POM

- The **coordinator** is the node where an admin starts the run. It chooses
  the participants from the peer list, assigns ranks and shards, and drives
  the outer rounds.
- The outer sync goes through the plugin's peer calls between nodes, so it
  rides POM's mutual TLS and membership checks. A peer call body is at most
  256 KiB and shares the host's 3 s IPC timeout, so a delta moves as numbered
  chunks: the sender stages it on disk with its SHA-256, the receiver pulls
  the chunks (`peer.train.chunk`) and verifies the hash. The worker never opens a
  public port. v1 uses a star (each node sends its delta to the coordinator,
  which averages and sends back the result); an all-reduce among peers is a
  later optimization.
- Pseudo-gradients are sent in bf16; optional int8 quantization of the deltas
  is a run setting.

### 4. Roles and authorization

- Starting, stopping and deleting a run takes an **admin of the trust chain**,
  the same rule as deploy, checked on every participant, not only on the
  screen.
- Training holds model weights, so a node takes part only if it announces the
  plugin's `training` capability and its role allows holding a model. An
  inference only node refuses, like it refuses a transfer.
- A visitor never participates.

### 5. Data and weights

- The dataset is a local file (JSONL, chat or completion format) on the
  coordinator, or already present on each node. Distributing shards is an
  explicit file transfer the operator confirms, using POM's existing transfer
  (direct, punched, or a cluster relay; never a public relay).
- The base model must be available as Hugging Face safetensors on each
  participant. v1 requires the HF checkpoint; conversion from a non quantized
  GGUF via `gguf-py` is an open item.
- Checkpoints (adapter + outer optimizer state) are saved on the coordinator
  at each outer round, so a run can resume after a crash.

### 6. Output: a sliceable GGUF

At the end of a run the coordinator:

1. merges the adapter into the base weights (`peft` `merge_and_unload`);
2. converts to GGUF with the fork's `convert_hf_to_gguf.py` and quantizes it
   with `llama-quantize` to the formats the operator chose;
3. writes the files to `models/` with a sidecar describing the run (base
   model, dataset hash, hyperparameters, final loss).

From there the model appears in the Deploy screen and the existing slicer adds
the `pom.*` metadata. The unmerged adapter is also kept for later runs.

### 7. Screens

- **Training runs**: list, status, participants, current round, loss chart,
  throughput per node.
- **New run**: base model, dataset, participants, LoRA rank/alpha, learning
  rates, inner steps, number of rounds, output quantizations.
- Per run: logs per node, stop, resume, open the resulting model in Deploy.

Each screen gets its guide in `docs/guides/` when it is built.

### 8. Compatibility

The plugin adds a capability and new peer routes under its own namespace; no
existing contract or persisted file changes. Peer messages carry a
`train/v1` version from the first release.

## Hardware expectations

| Base model | QLoRA VRAM per node (approx.) |
|---|---|
| 1-3B | 4-6 GB |
| 7-8B | 6-10 GB |
| 13-14B | 12-16 GB |

A node below the requirement is shown as ineligible in the New run screen.

## Open items

- GGUF to HF conversion for base models only held as GGUF.
- Metal support for 4-bit training (`bitsandbytes` is CUDA first); MLX or
  full precision LoRA as the fallback on Apple Silicon.
- Mixed CUDA and Metal nodes in one run: numerical agreement of averaged
  deltas.
- How the coordinator role moves if the coordinator leaves mid-run.

## First milestone

Two nodes on one LAN QLoRA-fine-tune a 1-3B model with the plugin, the run
produces a GGUF, it is sliced across both nodes and served. Then the same run
over a NAT punched connection, and then through a cluster relay.

## Out of scope (later phases)

- Full fine-tuning with compressed pseudo-gradients.
- Pipeline training for models larger than one node (Petals style, mapping
  onto POM's layer ranges); needs low latency links.
- Serving adapters without merging (`convert_lora_to_gguf.py` and adapter
  loading in the engine).
- Peer all-reduce instead of the coordinator star.
