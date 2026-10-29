# Published POM image

The image `.github/workflows/docker-hub.yml` pushes to Docker Hub: the Linux
AppImage of a GitHub release, unpacked onto Ubuntu 24.04 with the tools plugins
and the agents in plugin terminals use (git, ssh client, curl, jq, python3,
ripgrep, tmux...). See `docs/subsystems/build-and-run.md`, "Published image".

## Run

This image sets `POM_NO_TLS=1` so a reverse proxy can connect to the browser
UI and client API over HTTP. **Do not expose port 8080 as plain HTTP to the
internet**: credentials and sessions would cross it unencrypted. With a direct
Docker run, bind the UI to the host's loopback and reach it through an SSH
tunnel. Peer connections still use mutual TLS on the same gateway port.

```sh
docker run -d --name pom --restart unless-stopped \
  -p 127.0.0.1:8080:8080 -p 8181:8181 \
  -p 42777:42777/udp -p 52777:52777/udp -p 52777:52777/tcp \
  -v pom-data:/data <repository>:latest
# from your machine:
ssh -L 8080:127.0.0.1:8080 <user>@<server>   # then open http://localhost:8080/
```

Host networking simplifies P2P discovery but also exposes port 8080 on all
host interfaces. Use it only if a firewall restricts that port to trusted
clients or a local HTTPS proxy.

### HTTPS behind Coolify / Traefik

Use the Docker image as a Compose service in Coolify. Point its HTTP proxy
upstream to **port 8080 with scheme `http`** and enable HTTPS/Let's Encrypt on
the public domain. Keep port 8080 on the private Docker network, not as a
publicly published port. Do not set `insecureSkipVerify`: the proxy does not
connect to POM over HTTPS. The proxy must be the only public way to reach the
UI/API, and must pass the original `Host` header for same-origin checks and
session cookies. The image already sets `POM_NO_TLS=1`; outside the image,
pass `--no-tls` or `POM_NO_TLS=1`. To require HTTPS directly to POM again,
set `POM_NO_TLS=0` in the container environment. TLS and mutual certificate
verification for calls between POM nodes remain enabled on both ports.

### NVIDIA GPU

The image carries no NVIDIA library: the CUDA driver library (`libcuda.so.1`)
belongs to the host's driver and is mounted into the container when it starts.
On the host, install the NVIDIA driver and the
[NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)
(`nvidia-ctk runtime configure --runtime=docker`, then restart Docker), and
start the container with `--gpus all`:

```sh
docker run -d --name pom --restart unless-stopped --gpus all \
  -p 127.0.0.1:8080:8080 -p 8181:8181 \
  -p 42777:42777/udp -p 52777:52777/udp -p 52777:52777/tcp \
  -v pom-data:/data <repository>:latest
```

With Compose:

```yaml
services:
  pom:
    image: <repository>:latest
    # On a public host, do not use host networking without firewalling 8080.
    ports: ["127.0.0.1:8080:8080", "8181:8181", "42777:42777/udp", "52777:52777/udp", "52777:52777/tcp"]
    volumes: [pom-data:/data]
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: [gpu]
volumes:
  pom-data:
```

Without `--gpus` (or on a machine without NVIDIA) the node runs on the CPU, or
on another GPU through Vulkan. Check with `docker exec pom nvidia-smi`, which
the toolkit also mounts.

Command line options go after the image name (`pom --help`), or as `POM_*`
variables (`POM_GATEWAY_PORT`, `POM_PUBLIC_ADDR`, ...). `/data` is the POM home:
configuration, identity key, models and plugins.

## Licenses

The image carries POM's `LICENSE` and `THIRD-PARTY-NOTICES.txt` in
`/usr/share/doc/pom/`, taken from the AppImage it is built from; an AppImage
without them (a release from before they existed) stops the build.

## Build locally

```sh
gh release download <tag> -R <owner>/<repo> -p 'pom-*-x86_64.AppImage' -O packaging/docker/pom.AppImage
docker build -t pom:local --build-arg POM_RELEASE_VERSION=<tag> packaging/docker
```
