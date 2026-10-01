# Plain llama.cpp run with unified KV cache

Versioned record of the corrected run on the CUDA test server for deployment `d-e8deb94f`,
used to compare POM with plain llama.cpp on the same GGUF.

## Official run

Snapshot:
`<reports-dir>/2026-09-07_125931-Qwen3.6-35B-A3B-UD-IQ3_XXS.config.json`

```text
scripts/bench-llamacpp.sh \
  <reports-dir>/2026-09-07_125931-Qwen3.6-35B-A3B-UD-IQ3_XXS.config.json \
  --deploy-id d-e8deb94f --output-dir .bench-llamacpp-unified \
  --cuda-architectures 120
```

The target was `kind=whole`, node `5b03580398da` on the CUDA test server, with the GGUF
`/data/models/Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`. The script found 15,847 MiB
free on each GPU before launch, did not change POM and confirmed
`cleanup_ok=true` in the metadata. The requested context of 250,000 tokens
fit, so there was no `n_ctx` fallback.

Effective server flags:

```text
-c 250000 -b 512 -ngl 99 -np 4 --kv-unified
--cache-type-k q4_0 --cache-type-v q4_0 --reasoning off
--spec-type draft-mtp --spec-draft-n-max 3 --spec-draft-p-min 0.75
```

`max_sessions=2` becomes `-np 4` because the POM runtime reserves two
sequences per session. With a unified cache, `-c` stays at 250,000 and the log
confirmed `n_slots = 4`, `n_ctx_slot = 250112`, `kv_unified = 'true'`. The same
log confirmed the creation of the MTP context and `draft acceptance` lines.

## Medians and comparison with POM

The llama.cpp numbers come from the run's official CSV, with one discarded
warmup and three repetitions per prompt. The POM numbers are the medians of the
local origin on the CUDA test server in the POM report of the same snapshot. Generation delta is
`(llama.cpp - POM) / POM`; positive favors llama.cpp.

| prompt | POM gen tok/s | llama gen tok/s | gen delta | POM prefill ms / tok/s | llama prefill ms / tok/s | POM TTFT / total ms | llama TTFT / total ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| p-curto-repetitivo | 140.85 | 109.69 | -22.12% | 29 / 310.34 | 83.011 / 50.33 | 1,987 / 2,203 | 5,381.3 / 7,196.1 |
| p-curto-variado | 93.19 | 102.04 | +9.49% | 29 / 310.34 | 82.864 / 50.38 | 2,034 / 3,944 | 5,414.2 / 7,348.7 |
| p-longo-repetitivo | 133.33 | 122.62 | -8.04% | 59 / 155.17 | 97.525 / 42.81 | 2,063 / 2,291 | 5,345.6 / 6,936.6 |
| p-longo-variado | 84.41 | 114.15 | +35.23% | 60 / 152.54 | 93.669 / 43.26 | 2,065 / 4,231 | 5,336.3 / 7,099.0 |
| p-prefill-aleatorio | 76.92 | 106.08 | +37.91% | 2,533 / 2,255.03 | 3,342.804 / 2,193.07 | 4,537 / 4,563 | 8,744.3 / 8,833.5 |

(The prompt ids are the harness's own names: short/long, repetitive/varied
and random prefill.)

POM stays ahead on the repetitive prompts. llama.cpp is ahead on the varied
prompts and on the random prefill. TTFT and total are not equivalent in
transport: llama.cpp measures the SSH+curl call up to the first SSE, while POM
measures the distributed route with its cache, transport and tracing.

## Difference from the old run

The previous run used `-c 500000 -np 2` without `--kv-unified`. It was
invalidated for the final comparison. The official run in this document uses
the geometry equivalent to POM: `-c 250000`, `-np 4` and `--kv-unified`, plus
the K/V cache types observed in the deployment's runtime config.

MTP was replicated with the GGUF's own `nextn` head. POM's speculative lookup,
`hidden_state_dtype`, `fit_memory`, distributed transport and sharding are not
reproduced by `llama-server`. `shards` and `tensor_parallel` deployments are
still refused by the harness.

Artifacts of the official run: `.bench-llamacpp-unified/2026-09-07_174246-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.{csv,summary.md,metadata.json}`.

## E2E run through the local wrapper

A local wrapper script outside the repository was tested with
the real deployment. It called the clone directly through `RDMA_P2P_DIR`, used
the snapshot pinned by `LLAMACPP_CONFIG_JSON` and wrote the artifacts to
`<reports-dir>/`:

```text
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS-llama.md
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.csv
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.summary.md
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.metadata.json
```

The medians of that run were 116.77, 107.61, 129.56, 114.12 and 104.55
generation tok/s on the five prompts. The log confirmed MTP, unified cache and
draft acceptance between 0.80 and 1.00. POM does not currently expose counters
of proposed/accepted draft tokens per request. The speed difference between
repetitive and varied prompts in POM, absent in llama.cpp, remains an open
question: POM's MTP telemetry would need instrumentation to compare the
acceptance rate per prompt. POM's inference logic was not changed.

The wrapper fails explicitly when it cannot find the clone and accepts
`RDMA_P2P_DIR`; flags after `--` are forwarded to the harness. The harness's
full CLI is in the header of `scripts/bench-llamacpp.sh --help`, and the
wrapper uses the most recent snapshot in `~/scripts/reports` when
`LLAMACPP_CONFIG_JSON` is not set.
