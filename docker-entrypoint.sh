#!/bin/sh
set -eu

pom_bin=${POM_BIN:-/usr/local/bin/pom}
plugin_host=${POM_PLUGIN_HOST:-/usr/local/bin/pom-plugin-host}

if [ ! -x "$pom_bin" ]; then
    echo "pom não encontrado ou não executável: $pom_bin" >&2
    exit 127
fi
if [ ! -x "$plugin_host" ]; then
    echo "pom-plugin-host não encontrado ou não executável: $plugin_host" >&2
    exit 127
fi

# O host precisa do artefato Enterprise baixado depois da ativação. Por isso o
# pom o inicia como filho, passando o caminho do plugin e conectando stdin/stdout
# aos frames IPC. Um host vazio iniciado no boot não teria plugin para carregar
# e não seria o processo usado pelo pom.
export POM_PLUGIN_HOST="$plugin_host"
exec "$pom_bin" "$@"
