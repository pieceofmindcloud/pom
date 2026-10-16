#!/usr/bin/env bash
# bench-llamacpp.sh - plain llama.cpp benchmark to compare against POM.
#
# Takes the *.config.json snapshot from pom-bench, selects the served deployment,
# checks VRAM on ksia, starts the llama-server of the POM remote fork on
# 127.0.0.1 on a dedicated port, measures the five bench-routes prompts and
# always tears down only the process it created. CSV, summary.md, metadata.json
# and the local log are produced in the output directory. See --help and this
# task's report.md for the full CLI, the unavoidable differences and how to
# compare the medians.

set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
exec python3 "$SELF_DIR/bench_llamacpp.py" "$@"
