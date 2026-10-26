# syntax=docker/dockerfile:1
#
# The published POM image (`pereirajair/pom` on Docker Hub): the Linux AppImage of a
# GitHub release, unpacked onto a clean Ubuntu with the everyday tools the plugins use
# (git, ssh, curl, jq, python3...). Nothing is compiled here; `.github/workflows/docker-hub.yml`
# downloads the AppImage into the build context as `pom.AppImage`. See `README.md`.

# The AppImage's glibc, WebKitGTK and ICU are Ubuntu 24.04's; the runtime must match.
ARG UBUNTU=24.04

# ── The AppImage, unpacked ────────────────────────────────────────────────────
FROM ubuntu:${UBUNTU} AS appimage
COPY pom.AppImage /tmp/pom.AppImage
# `--appimage-extract` needs no FUSE, which a build container does not have.
RUN cd /tmp && chmod 755 pom.AppImage && ./pom.AppImage --appimage-extract >/dev/null \
    && install -D -m 755 squashfs-root/usr/bin/pom /out/pom \
    && install -D -m 755 squashfs-root/usr/bin/pom-plugin-host /out/pom-plugin-host

# ── libcuda stub ──────────────────────────────────────────────────────────────
# The CUDA AppImage links `libcuda.so.1`, which belongs to the host's driver and only
# exists in a container started with `--gpus`. On a machine without a GPU (a VPS) the
# NVIDIA stub lets the binary load; CUDA then finds no device and POM runs on the CPU.
# The entrypoint puts the stub on the library path only when no real driver is mounted.
FROM ubuntu:${UBUNTU} AS cuda-stub
ARG CUDA_STUB_PACKAGE=cuda-driver-dev-12-9
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -fsSL -o /tmp/keyring.deb \
       https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb \
    && dpkg -i /tmp/keyring.deb && apt-get update \
    && apt-get install -y --no-install-recommends "$CUDA_STUB_PACKAGE" \
    && stub="$(find /usr/local/cuda*/ -path '*/stubs/libcuda.so' | head -1)" \
    && test -n "$stub" && install -D -m 644 "$stub" /out/libcuda.so.1

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM ubuntu:${UBUNTU}
ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# Two groups of packages:
# - what the AppImage links (it is the desktop build: GTK 3, WebKitGTK 4.1, the Vulkan
#   loader) plus `iproute2`, which the node runs to list its own addresses, and `tini`,
#   PID 1 so orphans of plugin processes are reaped;
# - the tools a plugin or an agent in a plugin terminal expects on a Linux box.
# No Mesa Vulkan drivers on purpose: on a server their software rasterizer would show up
# as a Vulkan "GPU". An NVIDIA GPU is used through CUDA with `--gpus all`.
RUN apt-get update && apt-get install -y --no-install-recommends \
      libgtk-3-0t64 libwebkit2gtk-4.1-0 libsoup-3.0-0 libvulkan1 libssl3t64 libgomp1 \
      iproute2 tini \
      bash bzip2 ca-certificates curl dnsutils file git git-lfs gnupg htop iputils-ping jq \
      less lsof nano netcat-openbsd openssh-client procps python3 python3-pip python3-venv \
      ripgrep rsync sudo tar tmux unzip vim-tiny wget xz-utils zip zstd \
    && rm -rf /var/lib/apt/lists/*

COPY --from=appimage /out/ /usr/local/bin/
COPY --from=cuda-stub /out/libcuda.so.1 /opt/pom/cuda-stub/libcuda.so.1
COPY entrypoint.sh /usr/local/bin/pom-entrypoint

# A release before the headless mode always opens its GTK window and dies without a
# display. Probe the binary once here: if it still needs a display, install Xvfb and mark
# it, and the entrypoint runs it under a virtual display. A release with the headless mode
# keeps the image without Xvfb.
RUN set -eu; \
    probe="$(mktemp -d)"; \
    out="$(cd "$probe" && env -u DISPLAY -u WAYLAND_DISPLAY POM_HOME="$probe" POM_HEADLESS=1 \
      LD_LIBRARY_PATH=/opt/pom/cuda-stub timeout 20 /usr/local/bin/pom \
      --no-discovery --no-pkarr --gateway-port 18080 --port 43777 2>&1 || true)"; \
    rm -rf "$probe"; \
    if printf '%s' "$out" | grep -q 'Failed to initialize gtk'; then \
      echo "this release needs a display; installing Xvfb"; \
      apt-get update && apt-get install -y --no-install-recommends xvfb xauth \
        && rm -rf /var/lib/apt/lists/*; \
      touch /opt/pom/needs-xvfb; \
    elif printf '%s' "$out" | grep -qi 'panicked\|error while loading shared libraries'; then \
      printf '%s\n' "$out" >&2; exit 1; \
    fi

ARG POM_RELEASE_VERSION=""
ENV POM_RELEASE_VERSION=${POM_RELEASE_VERSION} \
    POM_HOME=/data \
    POM_HEADLESS=1 \
    POM_PLUGIN_HOST=/usr/local/bin/pom-plugin-host \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility
LABEL org.opencontainers.image.title="POM" \
      org.opencontainers.image.version="${POM_RELEASE_VERSION}"

WORKDIR /data
VOLUME /data
# 8080 interface and API, 8181 inference server, 42777 discovery, 52777 iroh endpoint.
EXPOSE 8080/tcp 8181/tcp 42777/udp 52777/udp
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS -o /dev/null http://127.0.0.1:${POM_GATEWAY_PORT:-8080}/ || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/pom-entrypoint"]
