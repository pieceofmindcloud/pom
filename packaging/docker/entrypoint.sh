#!/bin/sh
# Entrypoint of the published image (packaging/docker/Dockerfile).
set -eu

# A container started with `--gpus` has the host's libcuda in the linker cache; without
# it, the stub lets the CUDA build load and run on the CPU.
if ! ldconfig -p | grep -q 'libcuda\.so\.1 '; then
    export LD_LIBRARY_PATH="/opt/pom/cuda-stub${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi

if [ -e /opt/pom/needs-xvfb ]; then
    # A release without the headless mode: give its window a virtual display.
    exec xvfb-run -a -s "-screen 0 1280x1024x24 -nolisten tcp" /usr/local/bin/pom "$@"
fi
exec /usr/local/bin/pom "$@"
