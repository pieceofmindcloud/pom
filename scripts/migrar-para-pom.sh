#!/usr/bin/env bash
#
# Migra uma máquina do nome antigo do projeto para POM.
#
# The binary no longer knows the old name: it looks for `pom.json` and
# `.pom-node.key` (the node's only key), and reads the `POM_*` variables. This
# script carries the compatibility by renaming what is already on disk. Once it
# has run on every machine, it can be deleted.
#
# Rode a partir do diretório do projeto (ou passe o caminho como argumento):
#
#     ./scripts/migrar-para-pom.sh [diretorio]
#
# O que ele faz, e só isso:
#   - <dir>/rdma-p2p*.json      -> <dir>/pom*.json
#   - <dir>/.rdma-p2p-node.key  -> <dir>/.pom-node.key
#   - <dir>/*.last-slice.json   -> renomeado junto com a config a que pertence
#
# Não toca em modelo, fatia, nem em nada dentro dos GGUF. Uma fatia cortada
# antes do rename perde o `original_block_count` (o metadado agora se chama
# `pom.original_block_count`): o cluster volta a tratá-la como "total de camadas
# desconhecido", que é o mesmo caso das fatias antigas e já é suportado - a
# implantação continua funcionando, e refatiar o modelo restaura o metadado.
set -euo pipefail

# --quiet: so fala quando renomeou alguma coisa, e cala o lembrete do fim. E o
# modo que os scripts de deploy usam, porque eles rodam a migracao em toda
# subida e numa maquina ja migrada nao ha o que dizer.
QUIET=0
if [ "${1:-}" = "--quiet" ]; then
  QUIET=1
  shift
fi

dir="${1:-$(pwd)}"
cd "$dir"

renomeou=0

mover() {
    local de="$1" para="$2"
    [ -e "$de" ] || return 0
    if [ -e "$para" ]; then
        echo "  ! $para já existe - deixando $de como está"
        return 0
    fi
    mv "$de" "$para"
    echo "  ✓ $de -> $para"
    renomeou=$((renomeou + 1))
}

[ "$QUIET" = 1 ] || echo "Migrando $dir para POM"

# Config: o nome exato e as variantes (rdma-p2p-cpu.json e afins).
for antigo in rdma-p2p*.json; do
    [ -e "$antigo" ] || continue
    mover "$antigo" "pom${antigo#rdma-p2p}"
done

# A anotação da última fatia é derivada do nome da config.
for antigo in rdma-p2p*.last-slice.json; do
    [ -e "$antigo" ] || continue
    mover "$antigo" "pom${antigo#rdma-p2p}"
done

# Chaves de identidade, incluindo as variantes por papel
# (`.rdma-p2p-node-cpu.key` e afins - o proprio .gitignore do projeto sempre
# usou o padrao `.rdma-p2p-node*.key`, entao elas existem no campo).
for antigo in .rdma-p2p-node*.key; do
    [ -e "$antigo" ] || continue
    mover "$antigo" ".pom-node${antigo#.rdma-p2p-node}"
done

if [ "$renomeou" -eq 0 ]; then
    [ "$QUIET" = 1 ] || echo "Nada a migrar aqui."
    [ "$QUIET" = 1 ] && exit 0
else
    echo "$renomeou arquivo(s) renomeado(s) em $dir."
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
