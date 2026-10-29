# Published POM image

The image `.github/workflows/docker-hub.yml` pushes to Docker Hub: the Linux
AppImage of a GitHub release, unpacked onto Ubuntu 24.04 with the tools plugins
and the agents in plugin terminals use (git, ssh client, curl, jq, python3,
ripgrep, tmux...). See `docs/subsystems/build-and-run.md`, "Published image".

## Run

On a server (a VPS) the host network is the simplest: P2P discovery and the
iroh endpoint use UDP, and the admin UI only answers on loopback by default, so
it is reached through an SSH tunnel to the server.

```sh
docker run -d --name pom --restart unless-stopped --network host \
  -v pom-data:/data <repository>:latest
# from your machine:
ssh -L 8080:127.0.0.1:8080 <user>@<server>   # then open http://localhost:8080/
```

With published ports instead: `-p 8080:8080 -p 8181:8181 -p 42777:42777/udp
-p 52777:52777/udp`. With an NVIDIA GPU add `--gpus all` (the host needs the
NVIDIA Container Toolkit); without it the node runs on the CPU.

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
