# syntax=docker/dockerfile:1

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    libclang-dev \
    clang \
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
    sudo \
    openssh-server \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce-cli docker-compose-plugin \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile default --default-toolchain stable --no-modify-path \
    && rustup component add rustfmt clippy

RUN usermod -aG sudo ubuntu
RUN echo "ubuntu ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers
RUN groupadd -g 125 docker -U ubuntu
RUN echo 'ubuntu:123' | chpasswd
RUN sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config \
    && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
RUN echo 'export SSH_AUTH_SOCK=/ssh-agent.sock' >> /home/ubuntu/.bash_profile

# BuildKit / buildx, from the gist linked below:
# https://gist.github.com/jniltinho/bcb28a99aef33dcb5f35c297bf71e4ae
#
# The script downloads the `linux-amd64` binary from the buildx release, so this
# image only builds on x86_64. On an arm64 host (Apple Silicon) replace the script
# with `docker-buildx-plugin`, which comes from the same apt repository already configured
# above and is resolved per architecture.
USER ubuntu
#COPY install-buildkit.sh /tmp/install-buildkit.sh
#RUN bash /tmp/install-buildkit.sh
RUN <<EOF
VERSION=$(curl -Ls -o /dev/null -w %{url_effective} https://github.com/docker/buildx/releases/latest); VERSION=${VERSION##*/}
mkdir -p $HOME/.docker/cli-plugins
wget https://github.com/docker/buildx/releases/download/$VERSION/buildx-$VERSION.linux-amd64 -O $HOME/.docker/cli-plugins/docker-buildx
chmod +x $HOME/.docker/cli-plugins/docker-buildx
EOF


# The script exports these two in `.profile`, which only a login shell reads. Here
# they apply to every process in the container, which is what makes the everyday `docker build`
# and `docker compose` actually use BuildKit.
ENV DOCKER_BUILDKIT=1 \
    COMPOSE_DOCKER_CLI_BUILD=1

CMD ["sleep", "infinity"]
