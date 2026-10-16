#!/usr/bin/env bash
#
# Migrates a machine from the project's old name to POM.
#
# The binary no longer knows the old name: it looks for `pom.json` and
# `.pom-node.key` (the node's only key), and reads the `POM_*` variables. This
# script carries the compatibility by renaming what is already on disk. Once it
# has run on every machine, it can be deleted.
#
# Run it from the project directory (or pass the path as an argument):
#
#     ./scripts/migrate-to-pom.sh [diretorio]
#
# What it does, and only that:
#   - <dir>/rdma-p2p*.json      -> <dir>/pom*.json
#   - <dir>/.rdma-p2p-node.key  -> <dir>/.pom-node.key
#   - <dir>/*.last-slice.json   -> renamed along with the config it belongs to
#
# It does not touch models, slices, or anything inside the GGUF files. A slice
# cut before the rename loses `original_block_count` (the metadata is now called
# `pom.original_block_count`): the cluster goes back to treating it as "total
# layers unknown", which is the same case as the old slices and is already
# supported - the deployment keeps working, and re-slicing the model restores
# the metadata.
set -euo pipefail

# --quiet: only speaks when it renamed something, and silences the reminder at
# the end. It is the mode the deploy scripts use, because they run the migration
# on every startup and on an already migrated machine there is nothing to say.
QUIET=0
if [ "${1:-}" = "--quiet" ]; then
  QUIET=1
  shift
fi

dir="${1:-$(pwd)}"
cd "$dir"

renamed=0

mover() {
    local from="$1" to="$2"
    [ -e "$from" ] || return 0
    if [ -e "$to" ]; then
        echo "  ! $to já existe - deixando $from como está"
        return 0
    fi
    mv "$from" "$to"
    echo "  ✓ $from -> $to"
    renamed=$((renamed + 1))
}

[ "$QUIET" = 1 ] || echo "Migrando $dir para POM"

# Config: the exact name and the variants (rdma-p2p-cpu.json and the like).
for old in rdma-p2p*.json; do
    [ -e "$old" ] || continue
    mover "$old" "pom${old#rdma-p2p}"
done

# The last slice annotation is derived from the config name.
for old in rdma-p2p*.last-slice.json; do
    [ -e "$old" ] || continue
    mover "$old" "pom${old#rdma-p2p}"
done

# Identity keys, including the per-role variants
# (`.rdma-p2p-node-cpu.key` and the like - the project's own .gitignore always
# used the `.rdma-p2p-node*.key` pattern, so they exist in the field).
for old in .rdma-p2p-node*.key; do
    [ -e "$old" ] || continue
    mover "$old" ".pom-node${old#.rdma-p2p-node}"
done

if [ "$renamed" -eq 0 ]; then
    [ "$QUIET" = 1 ] || echo "Nada a migrar aqui."
    [ "$QUIET" = 1 ] && exit 0
else
    echo "$renamed arquivo(s) renomeado(s) em $dir."
fi

[ "$QUIET" = 1 ] && exit 0

cat <<'AVISO'

Falta o que este script não pode fazer por você:

  - Variáveis de ambiente: RDMA_P2P_HOME, RDMA_P2P_CONFIG, RDMA_P2P_UI_DIST,
    RDMA_P2P_NODE_KEY, RDMA_P2P_DEV_UPDATE, RDMA_P2P_BUILD_FEATURES e
    RDMA_P2P_DEV_AUTO_ACCEPT_TRANSFERS viraram POM_*. Ajuste onde elas são
    definidas (systemd, docker compose, .zshrc, scripts de subida).
  - O binário mudou de nome: ./target/release/pom. Comandos, serviços e
    `pkill -f` que citam o nome antigo precisam ser atualizados.
  - A identidade do nó (node_id) é preservada pela renomeação da chave. Apagar
    .pom-node.key em vez de renomeá-la cria uma máquina nova aos olhos do
    cluster, e ela perde o que estiver em blocked_nodes.
AVISO
