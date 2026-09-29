#!/usr/bin/env bash
# Sobe o compose da ksia. `--wizard` força o wizard de primeiro acesso nesta
# subida: a configuração atual vai para /data/setup-backups/<data> dentro do
# volume e o nó sobe em modo setup (ver `--setup` em crates/node/src/main.rs).
set -euo pipefail

if [[ "${1:-}" == "--wizard" ]]; then
  # Um token novo por chamada: o nó força o wizard uma vez por token, e o
  # reinício depois de salvar o wizard não força de novo.
  export POM_FORCE_SETUP="wizard-$(date +%Y%m%d-%H%M%S)"
fi

docker compose -f docker-compose.ksia.yml down
docker compose -f docker-compose.ksia.yml up -d
