#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/bench-rotas.sh"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bench-rotas-test.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

if [[ ! -x "$SCRIPT" ]]; then
    echo "bench-rotas.sh não existe ou não é executável" >&2
    exit 1
fi

if ! grep -Fq 'base64 -d' "$SCRIPT"; then
    echo "o caminho Salem precisa transportar o corpo sem stdin interativo" >&2
    exit 1
fi

if ! grep -A1 -F 'send -- "$env(POM_SALEM_PASS)' "$SCRIPT" | grep -Fq 'exp_continue'; then
    echo "o caminho Salem não pode fechar o spawn dentro do prompt de senha" >&2
    exit 1
fi

FAKE_BIN="$TMP_DIR/bin"
mkdir -p "$FAKE_BIN"
cat > "$FAKE_BIN/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
set -euo pipefail

if [[ "${POM_FAKE_FAIL_KSIA:-0}" == 1 && "$*" == *"ksia"* ]]; then
    echo "fake ksia indisponível" >&2
    exit 7
fi

if [[ "$*" != */api/generate* ]]; then
    echo "rota /api/generate ausente" >&2
    exit 90
fi

if [[ "$*" != *"X-API-Key: sk-test-bench"* ]]; then
    echo "API key de inferencia ausente no request" >&2
    exit 91
fi

REQUEST_BODY="$(cat)"
if [[ "$REQUEST_BODY" == *"Gemma4-26b-NVFP4.gguf"* ]]; then
    echo "modelo fixo indevido no corpo" >&2
    exit 89
fi

printf '%s\n__POM_HTTP_STATUS__:200\n' "$POM_FAKE_RESPONSE"
FAKE_CURL
chmod +x "$FAKE_BIN/curl"

FAKE_RESPONSE='{"response":"ok","source":"pipeline:test","tokens_generated":5,"tokens_per_sec":4.0,"duration_ms":1250,"timings":{"ttft_ms":300,"prefill_ms":200,"prefill_tokens":20,"prefill_tokens_per_sec":100.0,"generation_ms":1000,"generation_tokens":5,"generation_tokens_per_sec":5.0,"total_ms":1250,"total_tokens_per_sec":4.0},"trace":{"input_tokens":20,"cached_tokens":3,"prefix_match_tokens":3,"prefix_reuse_status":"partial","output_tokens":5,"mtp_draft_tokens_proposed":10,"mtp_draft_tokens_accepted":8,"mtp_draft_tokens_rejected":2,"mtp_acceptance_rate":0.8,"total_bytes_sent":100,"total_bytes_received":200,"hops":[{"decode":{"calls":2,"processing_ms":40,"round_trip_ms":50,"network_ms":10,"node_p50_ms":19.5,"node_p95_ms":20.5,"node_decode_ms":19.0,"node_lock_wait_ms":0.5,"node_backend_completion_ms":18.0,"transport_summary":{"transports":[{"transport":"direct","calls":2}]}}, "prefill":{"calls":1,"processing_ms":200,"round_trip_ms":210,"network_ms":10,"node_p50_ms":190,"node_p95_ms":195,"node_decode_ms":190,"node_lock_wait_ms":0,"node_backend_completion_ms":180,"transport_summary":{"transports":[{"transport":"direct","calls":1}]}}}]}}'

CSV_ONE="$TMP_DIR/one.csv"
PATH="$FAKE_BIN:$PATH" POM_FAKE_RESPONSE="$FAKE_RESPONSE" POM_BENCH_API_KEY='sk-test-bench' \
    "$SCRIPT" test-one --origens mac-local --repeticoes 1 --csv "$CSV_ONE" \
    > "$TMP_DIR/one.summary"

[[ "$(wc -l < "$CSV_ONE" | tr -d ' ')" == 7 ]]
grep -Fq 'model_requested' "$CSV_ONE"
grep -Fq 'fichas_por_chamada' "$CSV_ONE"
grep -Fq 'trace_mtp_draft_tokens_proposed' "$CSV_ONE"
grep -Fq 'trace_mtp_draft_tokens_accepted' "$CSV_ONE"
grep -Fq 'trace_mtp_draft_tokens_rejected' "$CSV_ONE"
grep -Fq 'trace_mtp_acceptance_rate' "$CSV_ONE"
grep -Fq '2.5' "$CSV_ONE"
grep -Fq '0.8' "$CSV_ONE"
grep -Fq 'fichas/chamada' "$TMP_DIR/one.summary"
grep -Fq 'generation_tok/s' "$TMP_DIR/one.summary"
grep -Fq 'mtp_acceptance_rate' "$TMP_DIR/one.summary"
grep -Fq '0.8' "$TMP_DIR/one.summary"
grep -Fq 'p-prefill-aleatorio' "$CSV_ONE"

CSV_TWO="$TMP_DIR/two.csv"
PATH="$FAKE_BIN:$PATH" POM_FAKE_RESPONSE="$FAKE_RESPONSE" POM_BENCH_API_KEY='sk-test-bench' \
    POM_FAKE_FAIL_KSIA=1 "$SCRIPT" test-two \
    --origens mac-local,mac-ksia --repeticoes 1 --csv "$CSV_TWO" \
    > "$TMP_DIR/two.summary"

grep -Fq 'mac-ksia' "$CSV_TWO"
grep -Fq 'unavailable' "$CSV_TWO"
grep -Fq '"auto","","200","false"' "$CSV_TWO"
grep -Fq 'mac-local' "$CSV_TWO"

CSV_TWO="$CSV_TWO" python3 - <<'PY'
import csv
import os

with open(os.environ["CSV_TWO"], newline="") as handle:
    rows = list(csv.DictReader(handle))

header = rows[0].keys()
assert list(header).index("trace_mtp_draft_tokens_proposed") > list(header).index("fichas_por_chamada")
assert list(header).index("trace_mtp_acceptance_rate") > list(header).index("fichas_por_chamada")

assert rows[0]["model_requested"] == "auto"
assert rows[0]["served_model"] == ""
assert rows[0]["max_tokens"] == "200"
assert rows[0]["stream"] == "false"
prefill = next(row for row in rows if row["prompt_id"] == "p-prefill-aleatorio")
assert prefill["max_tokens"] == "8"
assert prefill["trace_mtp_acceptance_rate"] == "0.8"
PY

echo "bench-rotas.sh: testes de contrato passaram"
