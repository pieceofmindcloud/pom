#!/bin/bash
# Um ciclo do laço: sobe o nó como o Docker subiria, espera, e reporta.
# $1 = home, $2 = numero do ciclo, $3 = "aplicar" no primeiro ciclo
set -u
H="$1"; N="$2"; MODO="${3:-restaurar}"
BIN="$PWD/target/debug/rdma-p2p"
LOG="$H/ciclo-$N.log"

RDMA_P2P_HOME="$H" RUST_LOG=info "$BIN" --no-discovery --refresh 3600 > "$LOG" 2>&1 &
PID=$!

# Espera o gateway atender — é isso que prova "o nó está de pé".
for _ in $(seq 1 60); do
  curl -sf -m 2 http://127.0.0.1:18080/health >/dev/null 2>&1 && break
  sleep 0.5
done

if [ "$MODO" = "aplicar" ]; then
  curl -s -m 10 -X POST http://127.0.0.1:18080/api/ui/apply \
    -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer chave-de-teste' \
    -d "{\"source_model_path\":\"$H/models/fatia_0_0.gguf\",\"base_name\":\"fatia\",
         \"start_layer\":0,\"end_layer\":0,\"total_layers\":2,
         \"peer_count\":2,\"peer_index\":0,\"skip_split\":true}" > "$H/apply-$N.json"
  echo "  apply -> $(cat "$H/apply-$N.json")"
fi

sleep 4

ANOT="$H/rdma-p2p.json.last-slice.json"
echo "  [ciclo $N] pid=$PID  gateway=$(curl -sf -m 2 -o /dev/null -w '%{http_code}' http://127.0.0.1:18080/health || echo MORTO)  anotacao=$([ -f "$ANOT" ] && echo PRESENTE || echo ausente)"
grep -c "restaurada após reinício" "$LOG" 2>/dev/null | sed 's/^/  restauracoes no log: /'

# Como o OOM killer do cgroup mata: SIGKILL, sem executar código nenhum.
kill -9 $PID 2>/dev/null
wait $PID 2>/dev/null
echo "  [ciclo $N] processo morto com SIGKILL (simulando o OOM killer)"
