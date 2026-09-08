# Corrida llama.cpp puro com cache KV unificado

Registro versionado da rodada corrigida na ksia para o deployment
`d-e8deb94f`, usado para comparar o POM com o llama.cpp puro no mesmo GGUF.

## Execução oficial

Snapshot:
`/Users/jairpereira/scripts/reports/2026-09-07_125931-Qwen3.6-35B-A3B-UD-IQ3_XXS.config.json`

```text
scripts/bench-llamacpp.sh \
  /Users/jairpereira/scripts/reports/2026-09-07_125931-Qwen3.6-35B-A3B-UD-IQ3_XXS.config.json \
  --deploy-id d-e8deb94f --output-dir .bench-llamacpp-unified \
  --cuda-architectures 120
```

O alvo foi `kind=whole`, nó `5b03580398da` em `ksia`, com o GGUF
`/data/models/Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`. O script encontrou 15.847 MiB
livres em cada GPU antes do launch, não alterou o POM e confirmou
`cleanup_ok=true` no metadata. O contexto solicitado de 250.000 tokens coube,
portanto não houve fallback de `n_ctx`.

Flags efetivas do servidor:

```text
-c 250000 -b 512 -ngl 99 -np 4 --kv-unified
--cache-type-k q4_0 --cache-type-v q4_0 --reasoning off
--spec-type draft-mtp --spec-draft-n-max 3 --spec-draft-p-min 0.75
```

`max_sessions=2` vira `-np 4` porque o runtime POM reserva duas sequências por
sessão. Com cache unificado, `-c` permanece 250.000 e o log confirmou
`n_slots = 4`, `n_ctx_slot = 250112`, `kv_unified = 'true'`. O mesmo log
confirmou a criação do contexto MTP e linhas de `draft acceptance`.

## Medianas e comparação com o POM

Os números llama.cpp vêm do CSV oficial da corrida, com um warmup descartado e
três repetições por prompt. Os números POM são as medianas da origem
`ksia-local` do relatório POM do mesmo snapshot. Delta de geração é
`(llama.cpp - POM) / POM`; positivo favorece o llama.cpp.

| prompt | POM gen tok/s | llama gen tok/s | delta gen | POM prefill ms / tok/s | llama prefill ms / tok/s | POM TTFT / total ms | llama TTFT / total ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| p-curto-repetitivo | 140.85 | 109.69 | -22.12% | 29 / 310.34 | 83.011 / 50.33 | 1,987 / 2,203 | 5,381.3 / 7,196.1 |
| p-curto-variado | 93.19 | 102.04 | +9.49% | 29 / 310.34 | 82.864 / 50.38 | 2,034 / 3,944 | 5,414.2 / 7,348.7 |
| p-longo-repetitivo | 133.33 | 122.62 | -8.04% | 59 / 155.17 | 97.525 / 42.81 | 2,063 / 2,291 | 5,345.6 / 6,936.6 |
| p-longo-variado | 84.41 | 114.15 | +35.23% | 60 / 152.54 | 93.669 / 43.26 | 2,065 / 4,231 | 5,336.3 / 7,099.0 |
| p-prefill-aleatorio | 76.92 | 106.08 | +37.91% | 2,533 / 2,255.03 | 3,342.804 / 2,193.07 | 4,537 / 4,563 | 8,744.3 / 8,833.5 |

O POM permanece à frente nos prompts repetitivos. O llama.cpp fica à frente nos
prompts variados e no prefill aleatório. TTFT e total não são equivalentes em
transporte: o llama.cpp mede a chamada SSH+curl até o primeiro SSE, enquanto o
POM mede a rota distribuída com o seu cache, transporte e tracing.

## Diferença em relação à corrida antiga

A corrida anterior usava `-c 500000 -np 2` sem `--kv-unified`. Ela foi
invalidada para a comparação final. A rodada oficial deste documento usa a
geometria equivalente ao POM: `-c 250000`, `-np 4` e `--kv-unified`, além dos
tipos de cache K/V observados no config runtime do deployment.

MTP foi replicado com a cabeça `nextn` do próprio GGUF. O lookup especulativo,
`hidden_state_dtype`, `fit_memory`, transporte distribuído e sharding do POM
não são reproduzidos pelo `llama-server`. Deployments `shards` e
`tensor_parallel` continuam recusados pelo harness.

Artefatos da corrida oficial: `.bench-llamacpp-unified/2026-09-07_174246-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.{csv,summary.md,metadata.json}`.

## Corrida E2E pelo wrapper local

O wrapper `/Users/jairpereira/scripts/pom-bench-llama.sh` foi testado com o
deployment real. Ele chamou diretamente o clone por `RDMA_P2P_DIR`, usou o
snapshot fixado por `LLAMACPP_CONFIG_JSON` e gravou os artefatos em
`/Users/jairpereira/scripts/reports/`:

```text
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS-llama.md
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.csv
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.summary.md
2026-09-07_175925-Qwen3.6-35B-A3B-UD-IQ3_XXS.llamacpp.metadata.json
```

As medianas dessa execução foram 116.77, 107.61, 129.56, 114.12 e 104.55
generation tok/s nos cinco prompts. O log confirmou MTP, cache unificado e
aceitação de draft entre 0.80 e 1.00. O POM não expõe atualmente contadores de
tokens draft propostos/aceitos por pedido. A diferença de velocidade entre
prompts repetitivos e variados no POM, ausente no llama.cpp, fica como pergunta
aberta: seria preciso instrumentar a telemetria MTP do POM para comparar a taxa
de aceitação por prompt. Não foi alterada a lógica de inferência do POM.

O wrapper falha explicitamente quando não encontra o clone e aceita
`RDMA_P2P_DIR`; as flags após `--` são encaminhadas ao harness. A CLI completa
do harness está no cabeçalho de `scripts/bench-llamacpp.sh --help` e o wrapper
usa o snapshot mais recente de `~/scripts/reports` quando
`LLAMACPP_CONFIG_JSON` não é definido.
