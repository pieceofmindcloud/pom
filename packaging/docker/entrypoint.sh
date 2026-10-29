#!/bin/sh
# Entrypoint of the published image (packaging/docker/Dockerfile).
set -eu

# The image carries no NVIDIA library and the executable links none: with `--gpus all`
# the NVIDIA Container Toolkit mounts the host driver's libcuda and POM uses CUDA;
# without it POM runs on the CPU (or Vulkan).

if [ -e /opt/pom/needs-xvfb ]; then
    # A release without the headless mode: give its window a virtual display.
    exec xvfb-run -a -s "-screen 0 1280x1024x24 -nolisten tcp" /usr/local/bin/pom "$@"
fi
exec /usr/local/bin/pom "$@"
