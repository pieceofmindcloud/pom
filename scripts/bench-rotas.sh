#!/usr/bin/env bash
# bench-rotas.sh - bateria reproduzivel para separar forma da saida, origem de
# rede e custo do no na geracao distribuida do POM.
#
# O que mede:
#   4 origens (mac-local, mac-ksia, ksia-local e salem-ksia) x 5 prompts
#   (curto/longo repetitivo/variado e prefill aleatorio). Cada celula recebe uma chamada
#   de aquecimento descartada e, por padrao, 3 repeticoes medidas.
#
# Uso:
#   ./scripts/bench-rotas.sh ipv4-42777
#   ./scripts/bench-rotas.sh ipv6-42777 --origens mac-ksia,ksia-local
#   ./scripts/bench-rotas.sh passagem-1 --repeticoes 5 --csv /tmp/p1.csv
#
# A saida principal e um CSV estavel, uma linha por aquecimento, pedido medido
# ou falha. O resumo de medianas por celula e impresso no fim. As colunas
# timings_* sao os tempos agregados da resposta; trace_* sao tokens/cache/
# bytes; decode_* e prefill_* sao as telemetrias do primeiro hop. As quatro
# colunas trace_mtp_* finais sao os contadores MTP somados por requisicao e
# trace_mtp_acceptance_rate e aceitos / propostos, ou vazio quando nao houve
# proposta MTP. model_requested
# e "auto" quando o campo foi omitido; served_model vem do trace retornado pelo
# gateway. A coluna fichas_por_chamada e sempre calculada como
# trace.output_tokens / trace.hops[0].decode.calls e e a metrica central.
#
# A API usada e somente POST /api/generate, sem autenticacao, sempre com o
# modelo atualmente servido (o campo model e omitido), max_tokens=200 por
# padrao (8 no prompt de prefill aleatorio) e stream=false. POM_BENCH_MODEL
# pode fixar um modelo apenas quando isso for desejado para uma comparacao
# especifica. Esta rota nao oferece uma opcao de
# seed fixa, portanto o script nao envia seed e registra essa limitacao no
# relatorio. A variacao estocastica deve ser tratada pelas 3 repeticoes e
# pelas medianas, nunca por trocar as constantes entre celulas.
#
# salem-ksia usa SSH como SALEM_SSH_USER (padrao ubuntu) em
# SALEM_SSH_HOST (padrao 100.85.81.27), autenticando com SALEM_PASS. Informe
# a senha no ambiente ao rodar, por exemplo:
#   SALEM_PASS='sua-senha' ./scripts/bench-rotas.sh passagem-1
# A senha nunca fica neste arquivo. SALEM_KSIA_URL pode sobrescrever o alvo
# visto de dentro da salem; por padrao e http://ksia:8080. KSIA_URL pode
# sobrescrever o alvo usado pelo Mac; por padrao e http://ksia:8080.

set -uo pipefail

MODEL="${POM_BENCH_MODEL:-}"
MODEL_REQUESTED_LABEL="${MODEL:-auto}"
readonly MAX_TOKENS=200
readonly PREFILL_MAX_TOKENS=8
readonly PREFILL_RANDOM_WORDS=1200
readonly STREAM=false
readonly MAC_LOCAL_URL='http://localhost:8080'
readonly MAC_KSIA_URL_DEFAULT='http://ksia:8080'
readonly KSIA_LOCAL_URL='http://localhost:8080'
readonly SALEM_KSIA_URL_DEFAULT='http://ksia:8080'
readonly CURL_CONNECT_TIMEOUT=15
readonly CURL_MAX_TIME=300
readonly SSH_CONNECT_TIMEOUT=15
readonly HTTP_MARKER=$'\n__POM_HTTP_STATUS__:'
readonly HEATING_PROMPT='Aquecimento da medicao. Responda brevemente com uma frase curta sobre o POM.'

KSIA_URL="${KSIA_URL:-$MAC_KSIA_URL_DEFAULT}"
SALEM_KSIA_URL="${SALEM_KSIA_URL:-$SALEM_KSIA_URL_DEFAULT}"
SALEM_SSH_USER="${SALEM_SSH_USER:-ubuntu}"
SALEM_SSH_HOST="${SALEM_SSH_HOST:-100.85.81.27}"

ORIGINS=(mac-local mac-ksia ksia-local salem-ksia)
PROMPT_IDS=(p-curto-repetitivo p-curto-variado p-longo-repetitivo p-longo-variado p-prefill-aleatorio)
SELECTED_ORIGINS=()
REPETITIONS=3
CSV_PATH=''
LABEL=''
TMP_DIR=''
CSV_COLUMN_COUNT=0
SUMMARY_RAW=''
REQUEST_MAX_TOKENS="$MAX_TOKENS"

usage() {
    sed -n '1,42p' "$0"
    cat <<'USAGE'

Opcoes:
  --origens LISTA       Subconjunto separado por virgulas.
  --repeticoes N        Repeticoes medidas por celula (padrao: 3).
  --csv CAMINHO         Arquivo de saida (padrao: bench-rotas-LABEL-UTC.csv).
  -h, --help            Mostra esta ajuda.

Dependencias: bash, curl, jq, openssl, ssh; expect tambem e necessario para salem-ksia.
USAGE
}

die() {
    echo "erro: $*" >&2
    exit 2
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "comando obrigatorio ausente: $1"
}

contains() {
    local needle="$1"
    shift
    local value
    for value in "$@"; do
        [[ "$value" == "$needle" ]] && return 0
    done
    return 1
}

parse_origins() {
    local raw="$1"
    local origin
    local IFS=','
    read -r -a SELECTED_ORIGINS <<< "$raw"
    [[ ${#SELECTED_ORIGINS[@]} -gt 0 ]] || die '--origens nao pode ser vazio'
    for origin in "${SELECTED_ORIGINS[@]}"; do
        contains "$origin" "${ORIGINS[@]}" || die "origem desconhecida: $origin"
    done
}

parse_args() {
    [[ $# -ge 1 ]] || {
        usage >&2
        exit 2
    }

    LABEL="$1"
    shift
    [[ -n "$LABEL" && "$LABEL" != -* ]] || die 'o primeiro argumento deve ser o rotulo da passagem'

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --origens)
                [[ $# -ge 2 ]] || die '--origens exige uma lista'
                parse_origins "$2"
                shift 2
                ;;
            --origens=*)
                parse_origins "${1#*=}"
                shift
                ;;
            --repeticoes)
                [[ $# -ge 2 ]] || die '--repeticoes exige um numero'
                REPETITIONS="$2"
                shift 2
                ;;
            --repeticoes=*)
                REPETITIONS="${1#*=}"
                shift
                ;;
            --csv)
                [[ $# -ge 2 ]] || die '--csv exige um caminho'
                CSV_PATH="$2"
                shift 2
                ;;
            --csv=*)
                CSV_PATH="${1#*=}"
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "opcao desconhecida: $1"
                ;;
        esac
    done

    [[ "$REPETITIONS" =~ ^[1-9][0-9]*$ ]] || die '--repeticoes deve ser um inteiro positivo'
    if [[ ${#SELECTED_ORIGINS[@]} -eq 0 ]]; then
        SELECTED_ORIGINS=("${ORIGINS[@]}")
    fi
}

build_long_context() {
    local kind="$1"
    local i
    for i in $(seq 1 180); do
        printf 'Contexto deterministico %s linha %03d: este bloco fixo controla o tamanho do prompt e nao contem instrucoes variaveis; use-o apenas como contexto neutro para a medicao.\n' "$kind" "$i"
    done
}

LONG_REPETITIVE_CONTEXT="$(build_long_context R)"
LONG_VARIADO_CONTEXT="$(build_long_context V)"

PROMPT_SHORT_REPETITIVE='Escreva exatamente 30 vezes a palavra POM, separadas por um unico espaco, em uma unica linha. Nao escreva preambulo, numeracao, pontuacao, explicacao ou quebra de linha.'
PROMPT_SHORT_VARIADO='Explique em ate 120 palavras por que a separacao entre prefill e decode ajuda a medir inferencia distribuida. Escreva prosa natural, variada e informativa, sem listas e sem repetir frases.'
PROMPT_LONG_REPETITIVE="$LONG_REPETITIVE_CONTEXT

Agora escreva exatamente 30 vezes a palavra POM, separadas por um unico espaco, em uma unica linha. Nao escreva preambulo, numeracao, pontuacao, explicacao ou quebra de linha. O contexto acima e apenas enchimento deterministico."
PROMPT_LONG_VARIADO="$LONG_VARIADO_CONTEXT

Agora explique em ate 120 palavras por que a separacao entre prefill e decode ajuda a medir inferencia distribuida. Escreva prosa natural, variada e informativa, sem listas e sem repetir frases. O contexto acima e apenas enchimento deterministico."

random_prefill_prompt() {
    local nonce
    local blob
    nonce="$(openssl rand -hex 8)"
    blob="$(openssl rand -hex $((PREFILL_RANDOM_WORDS * 3)) | fold -w 6 | tr '\n' ' ')"
    printf 'Contexto aleatorio unico (nonce %s) - use apenas como enchimento neutro para medir o prefill, nao contem instrucoes:\n%s\n\nResponda apenas com a palavra OK.' "$nonce" "$blob"
}

get_prompt() {
    case "$1" in
        p-curto-repetitivo) printf '%s' "$PROMPT_SHORT_REPETITIVE" ;;
        p-curto-variado) printf '%s' "$PROMPT_SHORT_VARIADO" ;;
        p-longo-repetitivo) printf '%s' "$PROMPT_LONG_REPETITIVE" ;;
        p-longo-variado) printf '%s' "$PROMPT_LONG_VARIADO" ;;
        p-prefill-aleatorio) random_prefill_prompt ;;
        *) return 1 ;;
    esac
}

request_json() {
    local prompt="$1"
    local max_tokens="${2:-$MAX_TOKENS}"
    if [[ -n "$MODEL" ]]; then
        jq -cn --arg model "$MODEL" --arg prompt "$prompt" --argjson max_tokens "$max_tokens" \
            '{model:$model,prompt:$prompt,max_tokens:$max_tokens,stream:false}'
    else
        jq -cn --arg prompt "$prompt" --argjson max_tokens "$max_tokens" \
            '{prompt:$prompt,max_tokens:$max_tokens,stream:false}'
    fi
}

generate_url() {
    case "$1" in
        */api/generate) printf '%s' "$1" ;;
        *) printf '%s/api/generate' "${1%/}" ;;
    esac
}

remote_curl_command() {
    local url="$1"
    url="$(generate_url "$url")"
    printf "curl -sS --connect-timeout %s --max-time %s -H 'Content-Type: application/json' --write-out '\\n__POM_HTTP_STATUS__:%%{http_code}' --data-binary @- %q" \
                "$CURL_CONNECT_TIMEOUT" "$CURL_MAX_TIME" "$url"
}

remote_curl_command_with_body() {
    local url="$1"
    local body="$2"
    local body_b64
    body_b64="$(printf '%s' "$body" | base64 | tr -d '\\r\\n')"
    printf "printf '%%s' %q | base64 -d | %s" "$body_b64" "$(remote_curl_command "$url")"
}

REQUEST_RAW=''
REQUEST_RC=0

run_local_request() {
    local url="$1"
    local body="$2"
    local raw
    url="$(generate_url "$url")"
    if raw="$(printf '%s' "$body" | curl -sS --connect-timeout "$CURL_CONNECT_TIMEOUT" \
                --max-time "$CURL_MAX_TIME" -H 'Content-Type: application/json' \
                --write-out "$HTTP_MARKER%{http_code}" --data-binary @- "$url")"; then
        REQUEST_RC=0
    else
        REQUEST_RC=$?
    fi
    REQUEST_RAW="$raw"
}

run_ksia_ssh_request() {
    local url="$1"
    local body="$2"
    local command
    local raw
    command="$(remote_curl_command "$url")"
    if raw="$(printf '%s' "$body" | ssh -T -o BatchMode=yes \
                -o ConnectTimeout="$SSH_CONNECT_TIMEOUT" -o StrictHostKeyChecking=accept-new \
                ksia "$command")"; then
        REQUEST_RC=0
    else
        REQUEST_RC=$?
    fi
    REQUEST_RAW="$raw"
}

run_salem_ssh_request() {
    local url="$1"
    local body="$2"
    local command
    local raw
    if [[ -z "${SALEM_PASS:-}" ]]; then
        REQUEST_RC=125
        REQUEST_RAW='SALEM_PASS nao definido; a origem salem-ksia foi marcada como indisponivel'
        return
    fi
    command="$(remote_curl_command_with_body "$url" "$body")"
    if raw="$(POM_SALEM_PASS="$SALEM_PASS" \
                POM_SALEM_USER="$SALEM_SSH_USER" POM_SALEM_HOST="$SALEM_SSH_HOST" \
                POM_SALEM_COMMAND="$command" \
                expect -c '
            set timeout 60
            set command $env(POM_SALEM_COMMAND)
            spawn ssh -T -o ConnectTimeout=15 -o StrictHostKeyChecking=no \
                        -o PreferredAuthentications=password -o PubkeyAuthentication=no \
                        "$env(POM_SALEM_USER)@$env(POM_SALEM_HOST)" $command
            expect {
                -re {(?i)are you sure.*yes/no} {
                    send -- "yes\r"
                    exp_continue
                }
               -re {(?i)password:} {
                   send -- "$env(POM_SALEM_PASS)\r"
                   exp_continue
               }
                eof {}
                timeout {
                    puts "expect timeout while connecting to salem"
                    exit 124
                }
            }
        ' 2>&1)"; then
        REQUEST_RC=0
    else
        REQUEST_RC=$?
    fi
    REQUEST_RAW="$raw"
}

run_request() {
    local origin="$1"
    local body="$2"
    case "$origin" in
        mac-local) run_local_request "$MAC_LOCAL_URL" "$body" ;;
        mac-ksia) run_local_request "$KSIA_URL" "$body" ;;
        ksia-local) run_ksia_ssh_request "$KSIA_LOCAL_URL" "$body" ;;
        salem-ksia) run_salem_ssh_request "$SALEM_KSIA_URL" "$body" ;;
        *) REQUEST_RC=125; REQUEST_RAW="origem desconhecida: $origin" ;;
    esac
}

PARSED_BODY=''
PARSED_HTTP_STATUS=''

parse_request_output() {
    PARSED_BODY="$REQUEST_RAW"
    PARSED_HTTP_STATUS=''
    if [[ "$REQUEST_RAW" == *"$HTTP_MARKER"* ]]; then
        PARSED_BODY="${REQUEST_RAW%"$HTTP_MARKER"*}"
        PARSED_HTTP_STATUS="${REQUEST_RAW##*"$HTTP_MARKER"}"
        PARSED_HTTP_STATUS="$(printf '%s' "$PARSED_HTTP_STATUS" | tr -d '\r\n')"
    fi
}

compact_error() {
    printf '%s' "$1" | tr '\r\n' '  ' | cut -c1-2000
}

csv_row() {
    jq -Rrn --args '$ARGS.positional | @csv' -- "$@"
}

append_error_row() {
    local timestamp="$1"
    local phase="$2"
    local origin="$3"
    local prompt_id="$4"
    local repetition="$5"
    local status="$6"
    local error="$7"
    local http_status="$8"
    local row
    local -a fields
    fields=("$LABEL" "$timestamp" "$phase" "$origin" "$prompt_id" "$repetition" \
        "$status" "$(compact_error "$error")" "$http_status" "$MODEL_REQUESTED_LABEL" '' "$REQUEST_MAX_TOKENS" "$STREAM")
    while [[ ${#fields[@]} -lt $CSV_COLUMN_COUNT ]]; do
        fields+=( '')
    done
    row="$(csv_row "${fields[@]}")"
    printf '%s\n' "$row" >> "$CSV_PATH"
}

append_json_row() {
    local timestamp="$1"
    local phase="$2"
    local origin="$3"
    local prompt_id="$4"
    local repetition="$5"
    local http_status="$6"
    local row
    if ! jq -e 'type == "object"' >/dev/null 2>&1 <<< "$PARSED_BODY"; then
        append_error_row "$timestamp" "$phase" "$origin" "$prompt_id" "$repetition" \
                    error 'resposta nao e um objeto JSON' "$http_status"
        return 1
    fi
    if ! row="$(jq -r --arg label "$LABEL" --arg timestamp "$timestamp" \
                --arg phase "$phase" --arg origin "$origin" --arg prompt_id "$prompt_id" \
                --arg repetition "$repetition" --arg status ok --arg error '' \
                --arg http_status "$http_status" --arg model "$MODEL_REQUESTED_LABEL" \
                --arg max_tokens "$REQUEST_MAX_TOKENS" --arg stream "$STREAM" '
        [
          $label, $timestamp, $phase, $origin, $prompt_id, $repetition,
          $status, $error, $http_status, $model, (.trace.model // ""), $max_tokens, $stream,
          (.response // ""),
          (.source // ""),
          (.tokens_generated // ""),
          (.tokens_per_sec // ""),
          (.duration_ms // ""),
          (.timings.ttft_ms // ""),
          (.timings.prefill_ms // ""),
          (.timings.prefill_tokens // ""),
          (.timings.prefill_tokens_per_sec // ""),
          (.timings.generation_ms // ""),
          (.timings.generation_tokens // ""),
          (.timings.generation_tokens_per_sec // ""),
          (.timings.total_ms // ""),
          (.timings.total_tokens_per_sec // ""),
          (.trace.input_tokens // ""),
          (.trace.cached_tokens // ""),
          (.trace.prefix_match_tokens // ""),
          (.trace.prefix_reuse_status // ""),
          (.trace.output_tokens // ""),
          (.trace.total_bytes_sent // ""),
          (.trace.total_bytes_received // ""),
          (.trace.hops[0].decode.calls // ""),
          (.trace.hops[0].decode.processing_ms // ""),
          (.trace.hops[0].decode.round_trip_ms // ""),
          (.trace.hops[0].decode.network_ms // ""),
          (.trace.hops[0].decode.node_p50_ms // ""),
          (.trace.hops[0].decode.node_p95_ms // ""),
          (.trace.hops[0].decode.node_decode_ms // ""),
          (.trace.hops[0].decode.node_lock_wait_ms // ""),
          (.trace.hops[0].decode.node_backend_completion_ms // ""),
          ((.trace.hops[0].decode.transport_summary // {}) | tojson),
          (.trace.hops[0].prefill.calls // ""),
          (.trace.hops[0].prefill.processing_ms // ""),
          (.trace.hops[0].prefill.round_trip_ms // ""),
          (.trace.hops[0].prefill.network_ms // ""),
          (.trace.hops[0].prefill.node_p50_ms // ""),
          (.trace.hops[0].prefill.node_p95_ms // ""),
          (.trace.hops[0].prefill.node_decode_ms // ""),
          (.trace.hops[0].prefill.node_lock_wait_ms // ""),
          (.trace.hops[0].prefill.node_backend_completion_ms // ""),
          ((.trace.hops[0].prefill.transport_summary // {}) | tojson),
          (if ((.trace.hops[0].decode.calls // 0) > 0)
           then ((.trace.output_tokens // 0) / .trace.hops[0].decode.calls)
           else "" end),
          (.trace.mtp_draft_tokens_proposed // ""),
          (.trace.mtp_draft_tokens_accepted // ""),
          (.trace.mtp_draft_tokens_rejected // ""),
          (.trace.mtp_acceptance_rate // "")
        ] | @csv
        ' <<< "$PARSED_BODY")"; then
        append_error_row "$timestamp" "$phase" "$origin" "$prompt_id" "$repetition" \
                    error 'falha ao extrair metricas com jq' "$http_status"
        return 1
    fi
    printf '%s\n' "$row" >> "$CSV_PATH"
    if [[ "$phase" == measurement && -n "$SUMMARY_RAW" ]]; then
        jq -r --arg origin "$origin" --arg prompt_id "$prompt_id" '
          [
            $origin, $prompt_id, "measurement", "ok",
            (if ((.trace.hops[0].decode.calls // 0) > 0)
             then ((.trace.output_tokens // 0) / .trace.hops[0].decode.calls)
             else "" end),
            (.timings.generation_tokens_per_sec // ""),
            (.trace.mtp_acceptance_rate // ""),
            (.timings.ttft_ms // ""),
            (.trace.hops[0].decode.node_p50_ms // ""),
            (.timings.total_ms // "")
          ] | @tsv
        ' <<< "$PARSED_BODY" >> "$SUMMARY_RAW"
    fi
    return 0
}

CSV_HEADER='label,timestamp_utc,phase,origin,prompt_id,repetition,status,error,http_status,model_requested,served_model,max_tokens,stream,response_text,source,tokens_generated,tokens_per_sec,duration_ms,timings_ttft_ms,timings_prefill_ms,timings_prefill_tokens,timings_prefill_tokens_per_sec,timings_generation_ms,timings_generation_tokens,timings_generation_tokens_per_sec,timings_total_ms,timings_total_tokens_per_sec,trace_input_tokens,trace_cached_tokens,trace_prefix_match_tokens,trace_prefix_reuse_status,trace_output_tokens,trace_total_bytes_sent,trace_total_bytes_received,decode_calls,decode_processing_ms,decode_round_trip_ms,decode_network_ms,decode_node_p50_ms,decode_node_p95_ms,decode_node_decode_ms,decode_node_lock_wait_ms,decode_node_backend_completion_ms,decode_transport_summary,prefill_calls,prefill_processing_ms,prefill_round_trip_ms,prefill_network_ms,prefill_node_p50_ms,prefill_node_p95_ms,prefill_node_decode_ms,prefill_node_lock_wait_ms,prefill_node_backend_completion_ms,prefill_transport_summary,fichas_por_chamada,trace_mtp_draft_tokens_proposed,trace_mtp_draft_tokens_accepted,trace_mtp_draft_tokens_rejected,trace_mtp_acceptance_rate'

write_summary_tsv() {
    local input="$1"
    local output="$2"
    jq -Rr -s '
        def median:
          if length == 0 then ""
          elif (length % 2) == 1 then (sort | .[length / 2 | floor])
          else (sort | .[(length / 2) - 1] + .[length / 2]) / 2
          end;
        (split("\n")
         | map(select(length > 0) | split("\t")
             | {
                 origin: .[0], prompt: .[1], phase: .[2], status: .[3],
                 fichas: .[4], generation: .[5], mtp_acceptance: .[6], ttft: .[7],
                 node_p50: .[8], total: .[9]
               })) as $rows
        | ($rows
           | map(select(.phase == "measurement" and .status == "ok"))
           | group_by([.origin, .prompt])
           | .[]
           | . as $group
           | [
               $group[0].origin,
               $group[0].prompt,
               ($group | map(.fichas | select(length > 0) | tonumber) | median),
               ($group | map(.generation | select(length > 0) | tonumber) | median),
               ($group | map(.mtp_acceptance | select(length > 0) | tonumber) | median),
               ($group | map(.ttft | select(length > 0) | tonumber) | median),
               ($group | map(.node_p50 | select(length > 0) | tonumber) | median),
               ($group | map(.total | select(length > 0) | tonumber) | median)
             ]
           | @tsv)
    ' "$input" > "$output"
}

print_summary() {
    local summary_tsv="$1"
    local origin
    local prompt_id
    local row
    local summary_origin
    local summary_prompt
    local fichas
    local generation_rate
    local mtp_acceptance_rate
    local ttft
    local node_p50
    local total

    printf '\nTabela-resumo: medianas das chamadas medidas (warmup excluido)\n'
    printf '%-14s %-23s %14s %16s %18s %10s %14s %10s\n' \
                'origem' 'prompt' 'fichas/chamada' 'generation_tok/s' 'mtp_acceptance_rate' 'ttft_ms' 'node_p50_ms' 'total_ms'
    printf '%-14s %-23s %14s %16s %18s %10s %14s %10s\n' \
                '--------------' '-----------------------' '--------------' '----------------' '------------------' '----------' '--------------' '----------'

    for origin in "${SELECTED_ORIGINS[@]}"; do
        for prompt_id in "${PROMPT_IDS[@]}"; do
            row="$(awk -F '\t' -v wanted_origin="$origin" -v wanted_prompt="$prompt_id" \
                        '$1 == wanted_origin && $2 == wanted_prompt { print; exit }' "$summary_tsv")"
            if [[ -n "$row" ]]; then
                IFS=$'\t' read -r summary_origin summary_prompt fichas generation_rate mtp_acceptance_rate ttft node_p50 total <<< "$row"
                printf '%-14s %-23s %14s %16s %18s %10s %14s %10s\n' \
                            "$summary_origin" "$summary_prompt" "$fichas" "$generation_rate" \
                            "$mtp_acceptance_rate" "$ttft" "$node_p50" "$total"
            else
                printf '%-14s %-23s %14s %16s %18s %10s %14s %10s\n' \
                            "$origin" "$prompt_id" 'indisponivel' '-' '-' '-' '-' '-'
            fi
        done
    done
}

collect_one() {
    local phase="$1"
    local origin="$2"
    local prompt_id="$3"
    local repetition="$4"
    local prompt="$5"
    local request_max_tokens="${6:-$MAX_TOKENS}"
    local timestamp
    local body
    local row_status

    REQUEST_MAX_TOKENS="$request_max_tokens"
    timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    body="$(request_json "$prompt" "$request_max_tokens")"
    run_request "$origin" "$body"
    parse_request_output

    if [[ "$REQUEST_RC" -ne 0 || ! "$PARSED_HTTP_STATUS" =~ ^2[0-9][0-9]$ ]]; then
        row_status=error
        [[ "$phase" == warmup ]] && row_status=unavailable
        if [[ -n "$PARSED_HTTP_STATUS" ]]; then
            append_error_row "$timestamp" "$phase" "$origin" "$prompt_id" "$repetition" \
                        "$row_status" "HTTP $PARSED_HTTP_STATUS: $PARSED_BODY" "$PARSED_HTTP_STATUS"
        else
            append_error_row "$timestamp" "$phase" "$origin" "$prompt_id" "$repetition" \
                        "$row_status" "comando falhou (rc=$REQUEST_RC): $REQUEST_RAW" ''
        fi
        printf '[%s] %s %s rep=%s: indisponivel/erro\n' \
                    "$phase" "$origin" "$prompt_id" "$repetition" >&2
        return 1
    fi

    if ! append_json_row "$timestamp" "$phase" "$origin" "$prompt_id" \
                "$repetition" "$PARSED_HTTP_STATUS"; then
        printf '[%s] %s %s rep=%s: JSON invalido\n' \
                    "$phase" "$origin" "$prompt_id" "$repetition" >&2
        return 1
    fi
    printf '[%s] %s %s rep=%s: ok\n' "$phase" "$origin" "$prompt_id" "$repetition" >&2
    return 0
}

main() {
    local origin
    local prompt_id
    local repetition
    local prompt
    local summary_tsv
    local warmup_failed

    parse_args "$@"
    require_command curl
    require_command jq
    require_command ssh
    require_command openssl
   if contains salem-ksia "${SELECTED_ORIGINS[@]}"; then
       require_command expect
        require_command base64
   fi

    TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bench-rotas.XXXXXX")"
    trap 'rm -rf "$TMP_DIR"' EXIT

    if [[ -z "$CSV_PATH" ]]; then
        CSV_PATH="bench-rotas-$LABEL-$(date -u +%Y%m%dT%H%M%SZ).csv"
    fi
    printf '%s\n' "$CSV_HEADER" > "$CSV_PATH" || die "nao foi possivel criar CSV: $CSV_PATH"
    CSV_COLUMN_COUNT="$(awk -F, '{ print NF }' <<< "$CSV_HEADER")"
    SUMMARY_RAW="$TMP_DIR/summary-input.tsv"
    : > "$SUMMARY_RAW"

    echo "medicao=$LABEL csv=$CSV_PATH model_requested=$MODEL_REQUESTED_LABEL max_tokens=$MAX_TOKENS stream=$STREAM" >&2
    echo 'seed: nao enviada; /api/generate nao oferece semente fixa' >&2
    echo "prompts longos: 180 linhas de enchimento deterministico por prompt" >&2
    echo "prefill aleatorio: $PREFILL_RANDOM_WORDS palavras hexicas unicas por chamada, max_tokens=$PREFILL_MAX_TOKENS" >&2

    for origin in "${SELECTED_ORIGINS[@]}"; do
        warmup_failed=0
        if ! collect_one warmup "$origin" warmup 0 "$HEATING_PROMPT"; then
            warmup_failed=1
        fi
        if [[ "$warmup_failed" -ne 0 ]]; then
            echo "origem marcada como indisponivel apos warmup: $origin" >&2
            continue
        fi
        for prompt_id in "${PROMPT_IDS[@]}"; do
            for repetition in $(seq 1 "$REPETITIONS"); do
                prompt="$(get_prompt "$prompt_id")"
                request_max_tokens="$MAX_TOKENS"
                if [[ "$prompt_id" == p-prefill-aleatorio ]]; then
                    request_max_tokens="$PREFILL_MAX_TOKENS"
                fi
                collect_one measurement "$origin" "$prompt_id" "$repetition" "$prompt" "$request_max_tokens" || true
            done
        done
    done

    write_summary_tsv "$SUMMARY_RAW" "$TMP_DIR/summary.tsv"
    print_summary "$TMP_DIR/summary.tsv"
    printf '\nCSV completo: %s\n' "$CSV_PATH"
    printf 'Para P1, compare fichas/chamada e generation_tok/s entre prompts e origens; use prefix_reuse_status e cached_tokens para auditar cache.\n'
}

main "$@"
