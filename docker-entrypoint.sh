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

# The host needs the Enterprise artifact downloaded after activation. That is why
# pom starts it as a child, passing the plugin path and connecting stdin/stdout
# to the IPC frames. A host started empty at boot would have no plugin to load
# and would not be the process pom uses.
export POM_PLUGIN_HOST="$plugin_host"
exec "$pom_bin" "$@"
