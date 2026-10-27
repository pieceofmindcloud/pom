#!/usr/bin/env python3
"""Check the host Compose files share one universal multi-backend executable."""

import json
import os
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
EXPECTED_SERVICES = {"node-gpu", "node-gpu2", "node-gpu-3", "node-cpu"}
EXPECTED_VOLUME_TARGETS = {
    "/data",
    "/data/models",
    "/data/source-models",
    "/data/workspace",
    "/opt/pom/dev-plugins",
}


def fail(message: str) -> None:
    raise AssertionError(message)


def compose_config(filename: str, overrides: dict[str, str] | None = None) -> dict:
    environment = os.environ.copy()
    environment.pop("COMPOSE_FILE", None)
    for variable in (
        "POM_LLAMA_CPP_MODE",
        "POM_LLAMA_CPP_RELEASE_TAG",
        "POM_LLAMA_CPP_RELEASE_REPOSITORY",
        "POM_LLAMA_CPP_SOURCE_CONTEXT",
        "CUDA_RUNTIME_FLAVOR",
    ):
        environment.pop(variable, None)
    environment.update(overrides or {})
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
    shared_build = next(iter(services.values()))["build"]
    build_args = shared_build.get("args", {})
    if shared_build.get("target") != "dev-runtime":
        fail(f"{filename}: cluster local deve usar a imagem de desenvolvimento")
    if "POM_BUILD_VARIANTS" in build_args:
        fail(f"{filename}: obsolete per-variant build selector")
    if build_args.get("POM_LLAMA_CPP_MODE") != "release":
        fail(f"{filename}: build de desenvolvimento deve usar llama.cpp release por padrão")
    if build_args.get("CUDA_RUNTIME_FLAVOR") != "runtime":
        fail(f"{filename}: release prebuilt requer a imagem CUDA runtime")
    if build_args.get("POM_LLAMA_CPP_RELEASE_REPOSITORY") != "pieceofmindcloud/llama.cpp":
        fail(f"{filename}: repositório da release prebuilt incorreto")
    if any(
        service["build"].get("context") != str(ROOT)
        or service["build"].get("dockerfile") != "Dockerfile"
        or service["build"].get("additional_contexts", {}).get("llama-fork")
        != str(ROOT / "llama.cpp")
        for service in services.values()
    ):
        fail(f"{filename}: contexto ou Dockerfile incorreto")

    for name, service in services.items():
        if service.get("network_mode") != "host":
            fail(f"{filename}: {name} perdeu network_mode: host")
        environment = service.get("environment", {})
        if environment.get("POM_HOME") != "/data":
            fail(f"{filename}: {name} perdeu POM_HOME=/data")
        if environment.get("POM_DEV_MODE") != "1":
            fail(f"{filename}: {name} não habilita os controles de desenvolvimento")
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

LOCAL_BINS = {name: CUDA_BIN for name in EXPECTED_SERVICES}
LOCAL_GPUS = {"node-gpu": ["0"], "node-gpu2": ["0"]}
LOCAL_RADEON = {"node-gpu", "node-gpu-3"}

check_config("docker-compose.yml", LOCAL_BINS, LOCAL_GPUS, LOCAL_RADEON)

for filename, service_name in (("docker-compose.yml", "node-gpu"),):
    build = compose_config(
        filename,
        {
            "POM_LLAMA_CPP_MODE": "source",
            "POM_LLAMA_CPP_SOURCE_CONTEXT": "/tmp/local-llama-context",
            "CUDA_RUNTIME_FLAVOR": "base",
        },
    )["services"][service_name]["build"]
    args = build.get("args", {})
    if args.get("POM_LLAMA_CPP_MODE") != "source" or args.get("CUDA_RUNTIME_FLAVOR") != "base":
        fail(f"{filename}: nao permite selecionar o fork fonte local")
    if build.get("additional_contexts", {}).get("llama-fork") != "/tmp/local-llama-context":
        fail(f"{filename}: nao permite apontar o fork fonte local")

print("Docker Compose backend and GPU configuration passed")
