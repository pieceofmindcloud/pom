#!/usr/bin/env python3
"""Check the host Compose files keep one image and select backend binaries."""

import json
import os
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
EXPECTED_SERVICES = {"node-gpu", "node-gpu2", "node-gpu-3", "node-cpu"}
EXPECTED_VOLUME_TARGETS = {"/data", "/data/models", "/data/source-models"}


def fail(message: str) -> None:
    raise AssertionError(message)


def compose_config(filename: str) -> dict:
    environment = os.environ.copy()
    environment.pop("COMPOSE_FILE", None)
    environment["COMPOSE_FILE"] = filename
    result = subprocess.run(
        ["docker", "compose", "config", "--format", "json"],
        cwd=ROOT,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        fail(f"docker compose config falhou para {filename}: {result.stderr.strip()}")
    return json.loads(result.stdout)


def reserved_devices(service: dict) -> list[dict]:
    deploy = service.get("deploy") or {}
    resources = deploy.get("resources") or {}
    reservations = resources.get("reservations") or {}
    return reservations.get("devices") or []


def nvidia_ids(service: dict) -> list[str]:
    return [
        device_id
        for device in reserved_devices(service)
        if device.get("driver") == "nvidia"
        for device_id in device.get("device_ids", [])
    ]


def device_sources(service: dict) -> set[str]:
    return {device["source"] for device in service.get("devices", [])}


def check_config(
    filename: str,
    expected_bins: dict[str, str],
    gpu_ids: dict[str, list[str]],
    radeon_services: set[str],
) -> None:
    config = compose_config(filename)
    services = config["services"]
    if set(services) != EXPECTED_SERVICES:
        fail(f"{filename}: servicos encontrados {sorted(services)}")

    builds = {json.dumps(service["build"], sort_keys=True) for service in services.values()}
    images = {service["image"] for service in services.values()}
    if len(builds) != 1 or images != {"pom:latest"}:
        fail(f"{filename}: servicos nao compartilham imagem e build")
    if any(
        service["build"].get("context") != str(ROOT)
        or service["build"].get("dockerfile") != "Dockerfile"
        for service in services.values()
    ):
        fail(f"{filename}: contexto ou Dockerfile incorreto")

    for name, service in services.items():
        if service.get("network_mode") != "host":
            fail(f"{filename}: {name} perdeu network_mode: host")
        environment = service.get("environment", {})
        if environment.get("POM_HOME") != "/data":
            fail(f"{filename}: {name} perdeu POM_HOME=/data")
        if environment.get("NVIDIA_DRIVER_CAPABILITIES") != "compute,utility":
            fail(f"{filename}: {name} alterou as capacidades NVIDIA")
        targets = {volume["target"] for volume in service.get("volumes", [])}
        if targets != EXPECTED_VOLUME_TARGETS:
            fail(f"{filename}: {name} perdeu os volumes persistentes: {sorted(targets)}")
        actual_bin = environment.get("POM_BIN")
        if actual_bin != expected_bins[name]:
            fail(
                f"{filename}: {name} usa POM_BIN={actual_bin!r}, "
                f"esperado {expected_bins[name]!r}"
            )
        actual_ids = nvidia_ids(service)
        if actual_ids != gpu_ids.get(name, []):
            fail(
                f"{filename}: {name} tem NVIDIA {actual_ids}, "
                f"esperado {gpu_ids.get(name, [])}"
            )
        has_radeon = "/dev/dri" in device_sources(service)
        if has_radeon != (name in radeon_services):
            fail(f"{filename}: visibilidade Radeon incorreta em {name}; atual {has_radeon}")
        if name == "node-cpu" and (service.get("devices") or reserved_devices(service)):
            fail(f"{filename}: node-cpu recebeu dispositivos de GPU")


CUDA_BIN = "/usr/local/bin/pom"
VULKAN_BIN = "/usr/local/bin/pom-vulkan"
CPU_BIN = "/usr/local/bin/pom-cpu"

SALEM_BINS = {
    "node-gpu": CUDA_BIN,
    "node-gpu2": CUDA_BIN,
    "node-gpu-3": VULKAN_BIN,
    "node-cpu": CPU_BIN,
}
SALEM_GPUS = {"node-gpu": ["0"], "node-gpu2": ["0"]}
SALEM_RADEON = {"node-gpu", "node-gpu-3"}

def check_ksia_config() -> None:
    filename = "docker-compose.ksia.yml"
    services = compose_config(filename)["services"]
    if set(services) != {"node-1"}:
        fail(f"{filename}: servicos encontrados {sorted(services)}")

    service = services["node-1"]
    if service["image"] != "pom:ksia":
        fail(f"{filename}: imagem inesperada {service['image']!r}")
    if service["build"].get("dockerfile") != "Dockerfile":
        fail(f"{filename}: Dockerfile de build incorreto")
    if service.get("network_mode") != "host":
        fail(f"{filename}: node-1 perdeu network_mode: host")
    if nvidia_ids(service) != ["0", "1"]:
        fail(f"{filename}: node-1 deve reservar as NVIDIA 0 e 1")
    if service.get("environment", {}).get("POM_BIN", CUDA_BIN) != CUDA_BIN:
        fail(f"{filename}: node-1 deve usar o binário CUDA+Vulkan compartilhado")
    args = service["build"].get("args", {})
    if args.get("POM_CUDA_ARCHITECTURES") != "86;120" or args.get(
        "POM_CUDA_ARCH_CACHE_KEY"
    ) != "86-120":
        fail(f"{filename}: arquitetura CUDA e cache key devem corresponder")


check_config("docker-compose.yml", SALEM_BINS, SALEM_GPUS, SALEM_RADEON)
check_config("docker-compose.salem.yml", SALEM_BINS, SALEM_GPUS, SALEM_RADEON)
check_ksia_config()
print("Docker Compose backend and GPU configuration passed")
