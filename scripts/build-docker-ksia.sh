#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "$0")" && pwd -P)
exec env COMPOSE_FILE=docker-compose.ksia.yml "$script_dir/build-docker.sh"
