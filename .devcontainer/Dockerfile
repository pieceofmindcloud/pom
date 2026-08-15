# syntax=docker/dockerfile:1

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    libclang-dev \
    ca-certificates \
    curl \
    gnupg \
    pkg-config \
    libssl-dev \
    mold \
    nodejs \
    npm \
    git \
    wget \
    openssh-server \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce-cli docker-compose-plugin \
    && rm -rf /var/lib/apt/lists/*

# BuildKit / buildx, do gist do jniltinho:
# https://gist.github.com/jniltinho/bcb28a99aef33dcb5f35c297bf71e4ae
#
# O script baixa o binário `linux-amd64` do release do buildx, então esta
# imagem só constrói em x86_64. Num host arm64 (Apple Silicon) troque o script
# por `docker-buildx-plugin`, que vem do mesmo repositório apt já configurado
# acima e é resolvido por arquitetura.
COPY install-buildkit.sh /tmp/install-buildkit.sh
RUN chmod +x /tmp/install-buildkit.sh && /tmp/install-buildkit.sh && rm /tmp/install-buildkit.sh

# O script exporta estas duas no `.profile`, que só um shell de login lê. Aqui
# elas valem para todo processo do container, que é o que faz o `docker build`
# e o `docker compose` do dia a dia usarem BuildKit de verdade.
ENV DOCKER_BUILDKIT=1 \
    COMPOSE_DOCKER_CLI_BUILD=1

# Root login over SSH with a fixed password — this container is a local dev
# sandbox, not internet-exposed, so a plain password is an acceptable
# trade-off for not having to manage keys per machine.
RUN echo 'root:123' | chpasswd \
    && sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config \
    && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config \
    && mkdir -p /var/run/sshd

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile default --default-toolchain stable --no-modify-path \
    && rustup component add rustfmt clippy

CMD ["sleep", "infinity"]
