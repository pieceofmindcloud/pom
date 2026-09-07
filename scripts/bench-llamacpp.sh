#!/usr/bin/env bash
# bench-llamacpp.sh - benchmark do llama.cpp puro para comparar com o POM.
#
# Recebe o snapshot *.config.json do pom-bench, seleciona a implantacao servida,
# verifica VRAM na ksia, sobe o llama-server do fork remoto do POM em 127.0.0.1
# numa porta dedicada, mede os cinco prompts de bench-rotas e sempre derruba
# apenas o processo que criou. CSV, summary.md, metadata.json e log local sao
# produzidos no diretorio de saida. Veja --help e o report.md desta tarefa para
# a CLI completa, as diferencas inevitaveis e como comparar as medianas.

set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
exec python3 "$SELF_DIR/bench_llamacpp.py" "$@"
