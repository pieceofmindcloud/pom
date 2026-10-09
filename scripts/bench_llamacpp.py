#!/usr/bin/env python3
"""Implementacao do benchmark remoto de llama.cpp puro.

O launcher publico e ``bench-llamacpp.sh``. Este modulo concentra as partes
deterministicas para que selecao de deployment, payloads, parsing de SSE e
medianas tenham testes locais e continuem independentes do host remoto.
"""

from __future__ import annotations

import argparse
import base64
import csv
import copy
import json
import os
import secrets
import shlex
import subprocess
import sys
import time
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Sequence


class ConfigError(ValueError):
    """Snapshot ou override nao contem uma configuracao utilizavel."""


class SafetyError(RuntimeError):
    """A politica de seguranca impediu iniciar o servidor."""


PROMPT_IDS = (
    "p-curto-repetitivo",
    "p-curto-variado",
    "p-longo-repetitivo",
    "p-longo-variado",
    "p-prefill-aleatorio",
)
# Estes IDs e textos espelham intencionalmente a copia fixa em
# ~/scripts/bench-routes.sh usada pelo pom-bench. Se a bateria mudar, atualize
# os dois lados no mesmo trabalho e registre a nova referencia no relatorio.
HEATING_PROMPT = (
    "Aquecimento da medicao. Responda brevemente com uma frase curta sobre o POM."
)
SHORT_REPETITIVE = (
    "Escreva exatamente 30 vezes a palavra POM, separadas por um unico espaco, "
    "em uma unica linha. Nao escreva preambulo, numeracao, pontuacao, "
    "explicacao ou quebra de linha."
)
SHORT_VARIED = (
    "Explique em ate 120 palavras por que a separacao entre prefill e decode "
    "ajuda a medir inferencia distribuida. Escreva prosa natural, variada e "
    "informativa, sem listas e sem repetir frases."
)

CSV_FIELDS = (
    "backend",
    "label",
    "timestamp_utc",
    "phase",
    "prompt_id",
    "repetition",
    "status",
    "http_status",
    "prompt_tokens",
    "output_tokens",
    "generation_tok_s",
    "prefill_ms",
    "prefill_tok_s",
    "ttft_ms",
    "total_ms",
    "prompt_ms",
    "predicted_ms",
    "prompt_per_second",
    "predicted_per_second",
    "tokens_cached",
    "response_text",
    "error",
)
SUMMARY_FIELDS = (
    "backend",
    "prompt_id",
    "repetitions_ok",
    "generation_tok_s",
    "prefill_ms",
    "prefill_tok_s",
    "ttft_ms",
    "total_ms",
    "prompt_tokens",
    "output_tokens",
    "tokens_cached",
)
DEFAULT_OUTPUT_DIR = "/Users/jairpereira/scripts/reports"


def _as_label(value: Any) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        for key in ("label", "model", "name"):
            if isinstance(value.get(key), str):
                return value[key]
    return ""


def _model_stem(value: str) -> str:
    return value.strip().removesuffix(".gguf").casefold()


def _same_model(left: str, right: str) -> bool:
    return bool(left and right and _model_stem(left) == _model_stem(right))


def _normalise_deployment(entry: dict[str, Any]) -> dict[str, Any]:
    deployment = entry.get("deployment", entry)
    if not isinstance(deployment, dict):
        raise ConfigError("entrada de deployment invalida: campo deployment nao e objeto")
    model = deployment.get("model", {})
    if not isinstance(model, dict):
        model = {"label": _as_label(model)}
    deploy_id = deployment.get("deploy_id") or deployment.get("id")
    label = _as_label(model)
    nodes = deployment.get("nodes", [])
    if not isinstance(nodes, list):
        raise ConfigError(f"deployment {deploy_id or '<sem-id>'} tem nodes invalido")
    return {
        "deploy_id": str(deploy_id or ""),
        "kind": str(deployment.get("kind") or ""),
        "label": label,
        "model": model,
        "total_layers": model.get("total_layers"),
        "nodes": nodes,
        "serving": bool(entry.get("serving", deployment.get("serving", False))),
        "is_default": bool(entry.get("is_default", deployment.get("is_default", False))),
        "activated_at": deployment.get("activated_at"),
    }


def _active_model(snapshot: dict[str, Any]) -> str:
    node = snapshot.get("/api/ui/node", {})
    if not isinstance(node, dict):
        node = {}
    for key in ("default_model", "active_model", "model"):
        value = _as_label(node.get(key))
        if value:
            return value
    for key in ("default_model", "active_model", "model"):
        value = _as_label(snapshot.get(key))
        if value:
            return value
    return ""


def select_deployment(
    snapshot: dict[str, Any], model: str | None = None, deploy_id: str | None = None
) -> dict[str, Any]:
    """Seleciona um deployment do snapshot, respeitando serving/default."""

    endpoint = snapshot.get("/api/ui/deployments", {})
    entries = endpoint.get("deployments", []) if isinstance(endpoint, dict) else []
    if not isinstance(entries, list) or not entries:
        raise ConfigError("snapshot nao contem /api/ui/deployments.deployments")
    deployments = [_normalise_deployment(entry) for entry in entries if isinstance(entry, dict)]
    if deploy_id:
        selected = [item for item in deployments if item["deploy_id"] == deploy_id]
        if not selected:
            raise ConfigError(f"deploy-id nao encontrado no snapshot: {deploy_id}")
        if len(selected) > 1:
            raise ConfigError(f"deploy-id repetido no snapshot: {deploy_id}")
        candidate = selected[0]
        if model and not _same_model(model, candidate["label"]):
            raise ConfigError(
                f"deployment {deploy_id} serve {candidate['label']!r}, "
                f"mas --model pediu {model!r}"
            )
        return candidate

    wanted_model = model or _active_model(snapshot)
    if not wanted_model:
        raise ConfigError(
            "nao foi possivel inferir o modelo ativo; use --model LABEL ou --deploy-id ID"
        )
    candidates = [item for item in deployments if _same_model(wanted_model, item["label"])]
    if not candidates:
        raise ConfigError(f"nenhum deployment corresponde ao modelo {wanted_model!r}")
    serving = [item for item in candidates if item["serving"]]
    if len(serving) == 1:
        return serving[0]
    preferred = [item for item in candidates if item["is_default"]]
    if len(preferred) == 1:
        return preferred[0]
    if len(candidates) == 1:
        return candidates[0]
    ids = ", ".join(item["deploy_id"] or "<sem-id>" for item in candidates)
    raise ConfigError(
        f"seleção ambígua para o modelo {wanted_model!r}: {ids}; use --deploy-id ID"
    )


def merge_runtime_node_config(
    snapshot: dict[str, Any], deployment: dict[str, Any]
) -> dict[str, Any]:
    """Completa campos runtime ausentes com o config do nó ativo.

    O snapshot do pom-bench mistura o registro de deployments com o estado
    runtime de ``/api/ui/node``. Alguns campos do deployment podem ficar
    nulos, embora o nó ativo os publique preenchidos. Só fazemos o merge
    quando ``deploy_id`` coincide exatamente e nunca substituímos um valor
    já presente no deployment.
    """

    node_endpoint = snapshot.get("/api/ui/node", {})
    runtime_config = node_endpoint.get("config") if isinstance(node_endpoint, dict) else None
    if not isinstance(runtime_config, dict):
        return deployment
    runtime_deploy_id = runtime_config.get("deploy_id")
    if str(runtime_deploy_id or "") != str(deployment.get("deploy_id") or ""):
        return deployment
    nodes = deployment.get("nodes")
    if not isinstance(nodes, list) or len(nodes) != 1 or not isinstance(nodes[0], dict):
        return deployment
    node_config = nodes[0].get("config")
    if not isinstance(node_config, dict):
        return deployment

    merged = copy.deepcopy(deployment)
    merged_config = merged["nodes"][0]["config"]
    runtime_fields = (
        "mtp_enabled",
        "mtp_draft_tokens",
        "mtp_min_probability",
        "mtp_cpu_fallback_enabled",
        "speculative_decoding_enabled",
        "speculative_k",
        "speculative_lookup_window",
        "cache_type_k",
        "cache_type_v",
        "repeat_last_n",
        "no_thinking",
    )
    for key in runtime_fields:
        if merged_config.get(key) is None and runtime_config.get(key) is not None:
            merged_config[key] = runtime_config[key]
    return merged


def _required(config: dict[str, Any], key: str, deploy_id: str) -> Any:
    value = config.get(key)
    if value is None:
        raise ConfigError(f"deployment {deploy_id}: config.{key} ausente")
    return value


def _layer_range(node: dict[str, Any]) -> dict[str, Any]:
    start = node.get("start_layer")
    end = node.get("end_layer")
    layers = node.get("layers")
    if (start is None or end is None) and isinstance(layers, dict):
        start = layers.get("start", layers.get("start_layer"))
        end = layers.get("end", layers.get("end_layer"))
    if (start is None or end is None) and isinstance(layers, list) and len(layers) == 2:
        start, end = layers
    return {"start_layer": start, "end_layer": end}


def build_bench_plan(
    deployment: dict[str, Any], n_ctx_override: int | None = None
) -> dict[str, Any]:
    """Converte o deployment selecionado em plano serializavel de benchmark."""

    kind = deployment.get("kind") or ""
    if kind != "whole":
        raise ConfigError(
            f"deployment {deployment.get('deploy_id') or '<sem-id>'} tem kind={kind!r}; "
            "multi-node nao e reproduzido pelo llama-server simples. "
            "Use --deploy-id apontando para um deployment kind=whole."
        )
    nodes = deployment.get("nodes") or []
    if len(nodes) != 1 or not isinstance(nodes[0], dict):
        raise ConfigError(
            f"deployment {deployment.get('deploy_id') or '<sem-id>'} kind=whole "
            "precisa ter exatamente um no"
        )
    node = nodes[0]
    if not deployment.get("label"):
        raise ConfigError(f"deployment {deployment.get('deploy_id')} sem model.label")
    config = node.get("config")
    if not isinstance(config, dict):
        raise ConfigError(f"deployment {deployment.get('deploy_id')} sem nodes[0].config")
    deploy_id = deployment.get("deploy_id") or "<sem-id>"
    core = {
        key: _required(config, key, deploy_id)
        for key in (
            "n_ctx",
            "n_batch",
            "n_gpu_layers",
            "offload_kqv",
            "max_sessions",
            "hidden_state_dtype",
            "inference_mode",
            "temperature",
            "top_k",
            "top_p",
            "repeat_penalty",
        )
    }
    core.update(
        {
            "mtp_enabled": bool(config.get("mtp_enabled", False)),
            "mtp_draft_tokens": config.get("mtp_draft_tokens", 3),
            "mtp_min_probability": config.get("mtp_min_probability", 0.0),
            "mtp_cpu_fallback_enabled": bool(config.get("mtp_cpu_fallback_enabled", True)),
            "speculative_decoding_enabled": bool(
                config.get("speculative_decoding_enabled", False)
            ),
            "speculative_k": config.get("speculative_k", 4),
            "speculative_lookup_window": config.get("speculative_lookup_window", 3),
            "cache_type_k": config.get("cache_type_k"),
            "cache_type_v": config.get("cache_type_v"),
            "repeat_last_n": config.get("repeat_last_n"),
            "no_thinking": bool(config.get("no_thinking", False)),
        }
    )
    if n_ctx_override is not None:
        if n_ctx_override <= 0:
            raise ConfigError("--n-ctx deve ser inteiro positivo")
        effective_n_ctx = n_ctx_override
    else:
        effective_n_ctx = core["n_ctx"]
    if not isinstance(effective_n_ctx, int) or effective_n_ctx <= 0:
        raise ConfigError("config.n_ctx deve ser inteiro positivo")
    for key in ("n_batch", "n_gpu_layers", "max_sessions"):
        if not isinstance(core[key], int) or core[key] <= 0:
            raise ConfigError(f"config.{key} deve ser inteiro positivo")
    if not isinstance(core["mtp_draft_tokens"], int) or core["mtp_draft_tokens"] <= 0:
        raise ConfigError("config.mtp_draft_tokens deve ser inteiro positivo")
    if not isinstance(core["speculative_k"], int) or core["speculative_k"] <= 0:
        raise ConfigError("config.speculative_k deve ser inteiro positivo")
    if not isinstance(core["speculative_lookup_window"], int) or core["speculative_lookup_window"] <= 0:
        raise ConfigError("config.speculative_lookup_window deve ser inteiro positivo")
    if not isinstance(core["mtp_min_probability"], (int, float)) or not 0 <= core["mtp_min_probability"] <= 1:
        raise ConfigError("config.mtp_min_probability deve estar entre 0 e 1")
    if core["repeat_last_n"] is not None and (
        not isinstance(core["repeat_last_n"], int) or core["repeat_last_n"] < 0
    ):
        raise ConfigError("config.repeat_last_n deve ser inteiro nao negativo")
    for key in ("cache_type_k", "cache_type_v"):
        if core[key] is not None and not isinstance(core[key], str):
            raise ConfigError(f"config.{key} deve ser texto")
    mtp_enabled = core["mtp_enabled"]
    return {
        "deployment_id": deployment.get("deploy_id"),
        "model_label": deployment.get("label"),
        "kind": kind,
        "total_layers": deployment.get("total_layers"),
        "node": {
            "node_id": node.get("node_id"),
            "hostname": node.get("hostname"),
            **_layer_range(node),
        },
        "pom_config": core,
        "effective_n_ctx": effective_n_ctx,
        "llama_flags": {
            "ctx_size": effective_n_ctx,
            "ctx_per_session": effective_n_ctx,
            "batch_size": core["n_batch"],
            "gpu_layers": core["n_gpu_layers"],
            "parallel": core["max_sessions"] * 2,
            "kv_offload": bool(core["offload_kqv"]),
            "tensor_split": "automatic (sem --tensor-split)",
            "spec_type": "draft-mtp" if mtp_enabled else "none",
            "spec_draft_n_max": core["mtp_draft_tokens"] if mtp_enabled else 0,
            "spec_draft_p_min": core["mtp_min_probability"] if mtp_enabled else 0.0,
            "mtp_cpu_fallback": core["mtp_cpu_fallback_enabled"],
            "speculation_source": "mtp" if mtp_enabled else "none",
            "n_seq_max": core["max_sessions"] * 2,
            "kv_unified": True,
            "cache_type_k": core["cache_type_k"],
            "cache_type_v": core["cache_type_v"],
            "repeat_last_n": core["repeat_last_n"],
            "reasoning": "off" if core["no_thinking"] else None,
        },
        "not_replicated": [
            "speculative_decoding_enabled/speculative_k/speculative_lookup_window "
            "(quando mtp_enabled=true, o POM escolhe MTP antes do lookup)",
            "hidden_state_dtype (servidor local nao troca estado oculto)",
            "fit_memory (llama-server recebe n_ctx fixo; nao usa o ajuste do POM)",
            "prefill_max_inflight_chunks e a topologia distribuida",
        ],
    }


def make_completion_payload(
    prompt: str, max_tokens: int, sampling: dict[str, Any]
) -> dict[str, Any]:
    payload = {
        "prompt": prompt,
        "n_predict": max_tokens,
        "temperature": sampling["temperature"],
        "top_k": sampling["top_k"],
        "top_p": sampling["top_p"],
        "repeat_penalty": sampling["repeat_penalty"],
        "stream": True,
    }
    if sampling.get("repeat_last_n") is not None:
        payload["repeat_last_n"] = sampling["repeat_last_n"]
    return payload


def parse_sse_result(
    lines: Iterable[str],
    first_token_ms: float | None,
    total_ms: float,
    http_status: int | None,
    process_error: str = "",
) -> dict[str, Any]:
    events: list[dict[str, Any]] = []
    for raw_line in lines:
        line = raw_line.strip()
        if not line.startswith("data: "):
            continue
        payload = line[6:]
        if payload == "[DONE]":
            continue
        try:
            event = json.loads(payload)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            events.append(event)

    final_event = next((event for event in reversed(events) if event.get("stop") is True), None)
    timings_event = next(
        (event for event in reversed(events) if isinstance(event.get("timings"), dict)), None
    )
    timing = (timings_event or {}).get("timings", {})
    if not isinstance(timing, dict):
        timing = {}
    prompt_tokens = timing.get("prompt_n")
    if prompt_tokens is None and final_event:
        prompt_tokens = final_event.get("tokens_evaluated")
    output_tokens = timing.get("predicted_n")
    if output_tokens is None and final_event:
        output_tokens = final_event.get("tokens_predicted")
    response_text = "".join(
        event.get("content", "")
        for event in events
        if isinstance(event.get("content"), str)
    )
    errors: list[str] = []
    if process_error:
        errors.append(process_error)
    if http_status is not None and not 200 <= http_status < 300:
        errors.append(f"HTTP {http_status}")
    if not events:
        errors.append("resposta SSE sem eventos JSON")
    if not timing:
        errors.append("resposta sem timings")
    if first_token_ms is None:
        errors.append("primeiro token nao observado no stream")
    return {
        "status": "ok" if not errors else "error",
        "http_status": http_status,
        "prompt_tokens": prompt_tokens,
        "output_tokens": output_tokens,
        "generation_tok_s": timing.get("predicted_per_second"),
        "prefill_ms": timing.get("prompt_ms"),
        "prefill_tok_s": timing.get("prompt_per_second"),
        "ttft_ms": first_token_ms,
        "total_ms": total_ms,
        "prompt_ms": timing.get("prompt_ms"),
        "predicted_ms": timing.get("predicted_ms"),
        "prompt_per_second": timing.get("prompt_per_second"),
        "predicted_per_second": timing.get("predicted_per_second"),
        "tokens_cached": timing.get("cache_n"),
        "response_text": response_text,
        "error": "; ".join(errors),
    }


def median(values: Sequence[float | int | None]) -> float | int | None:
    clean = sorted(float(value) for value in values if value is not None)
    if not clean:
        return None
    middle = len(clean) // 2
    if len(clean) % 2:
        result = clean[middle]
    else:
        result = (clean[middle - 1] + clean[middle]) / 2
    return int(result) if result.is_integer() else result


def summary_rows(rows: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        if row.get("phase") == "measurement" and row.get("status") == "ok":
            grouped[str(row.get("prompt_id"))].append(row)
    result = []
    for prompt_id in PROMPT_IDS:
        group = grouped.get(prompt_id, [])
        result.append(
            {
                "backend": "llama.cpp puro",
                "prompt_id": prompt_id,
                "repetitions_ok": len(group),
                "generation_tok_s": median([row.get("generation_tok_s") for row in group]),
                "prefill_ms": median([row.get("prefill_ms") for row in group]),
                "prefill_tok_s": median([row.get("prefill_tok_s") for row in group]),
                "ttft_ms": median([row.get("ttft_ms") for row in group]),
                "total_ms": median([row.get("total_ms") for row in group]),
                "prompt_tokens": median([row.get("prompt_tokens") for row in group]),
                "output_tokens": median([row.get("output_tokens") for row in group]),
                "tokens_cached": median([row.get("tokens_cached") for row in group]),
            }
        )
    return result


def incomplete_prompts(rows: Iterable[dict[str, Any]], expected_repetitions: int) -> list[str]:
    """Retorna prompts que nao completaram todas as repeticoes medidas."""

    counts: dict[str, int] = defaultdict(int)
    for row in rows:
        if row.get("phase") == "measurement" and row.get("status") == "ok":
            counts[str(row.get("prompt_id"))] += 1
    return [
        prompt_id
        for prompt_id in PROMPT_IDS
        if prompt_id in counts and counts[prompt_id] < expected_repetitions
    ]


def _build_long_context(kind: str) -> str:
    return "".join(
        f"Contexto deterministico {kind} linha {index:03d}: este bloco fixo controla "
        "o tamanho do prompt e nao contem instrucoes variaveis; use-o apenas como "
        "contexto neutro para a medicao.\n"
        for index in range(1, 181)
    )


def _random_prefill_prompt(words: int) -> str:
    blob = " ".join(secrets.token_hex(3) for _ in range(words))
    nonce = f"{time.time_ns()}-{secrets.randbelow(1_000_000)}"
    return (
        f"Contexto aleatorio unico (nonce {nonce}) - use apenas como enchimento "
        "neutro para medir o prefill, nao contem instrucoes:\n"
        f"{blob}\n\nResponda apenas com a palavra OK."
    )


def build_prompts(prefill_words: int) -> dict[str, str]:
    repetitive = _build_long_context("R")
    varied = _build_long_context("V")
    return {
        "p-curto-repetitivo": SHORT_REPETITIVE,
        "p-curto-variado": SHORT_VARIED,
        "p-longo-repetitivo": (
            repetitive
            + "\nAgora escreva exatamente 30 vezes a palavra POM, separadas por um unico "
            "espaco, em uma unica linha. Nao escreva preambulo, numeracao, pontuacao, "
            "explicacao ou quebra de linha. O contexto acima e apenas enchimento deterministico."
        ),
        "p-longo-variado": (
            varied
            + "\nAgora explique em ate 120 palavras por que a separacao entre prefill e "
            "decode ajuda a medir inferencia distribuida. Escreva prosa natural, variada "
            "e informativa, sem listas e sem repetir frases. O contexto acima e apenas "
            "enchimento deterministico."
        ),
        "p-prefill-aleatorio": "",
    }


@dataclass
class RemoteResult:
    returncode: int
    stdout: str
    stderr: str


class Remote:
    def __init__(self, host: str, connect_timeout: int = 15):
        self.host = host
        self.connect_timeout = connect_timeout

    def run(self, command: str, timeout: float | None = None) -> RemoteResult:
        completed = subprocess.run(
            [
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                f"ConnectTimeout={self.connect_timeout}",
                "-o",
                "StrictHostKeyChecking=accept-new",
                self.host,
                command,
            ],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return RemoteResult(completed.returncode, completed.stdout, completed.stderr)


def _require_local_commands() -> None:
    for command in ("ssh", "python3"):
        if not shutil_which(command):
            raise ConfigError(f"comando obrigatorio ausente localmente: {command}")


def shutil_which(command: str) -> str | None:
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        candidate = Path(directory) / command
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    return None


def _parse_vram(output: str) -> list[dict[str, int]]:
    cards = []
    for line in output.splitlines():
        fields = [field.strip() for field in line.split(",")]
        if len(fields) != 4:
            continue
        try:
            cards.append(
                {
                    "index": int(fields[0]),
                    "total_mb": int(fields[1]),
                    "used_mb": int(fields[2]),
                    "free_mb": int(fields[3]),
                }
            )
        except ValueError:
            continue
    return cards


def _shell_path(value: str) -> str:
    return shlex.quote(value)


def _remote_vram(remote: Remote, minimum_free_mb: int) -> list[dict[str, int]]:
    result = remote.run(
        "nvidia-smi --query-gpu=index,memory.total,memory.used,memory.free "
        "--format=csv,noheader,nounits"
    )
    cards = _parse_vram(result.stdout)
    if result.returncode != 0 or not cards:
        detail = (result.stderr or result.stdout).strip()
        raise SafetyError(f"nao foi possivel consultar VRAM com nvidia-smi: {detail}")
    low = [card for card in cards if card["free_mb"] < minimum_free_mb]
    if low:
        details = ", ".join(
            f"GPU {card['index']}: livre={card['free_mb']} MiB "
            f"(usado={card['used_mb']} MiB, total={card['total_mb']} MiB)"
            for card in cards
        )
        raise SafetyError(
            f"VRAM insuficiente para iniciar com seguranca ({details}); "
            f"politica minima={minimum_free_mb} MiB livres por GPU. "
            "O POM pode estar ocupando as GPUs; liberte o modelo do POM e "
            "execute novamente. O harness nao mata nem reinicia o POM."
        )
    return cards


def _check_port(remote: Remote, port: int) -> None:
    command = (
        "if command -v ss >/dev/null 2>&1; then "
        f"ss -H -ltn 2>/dev/null | awk '$4 ~ /:{port}$/ {{found=1}} END {{exit found ? 0 : 1}}'; "
        "else "
        f"status=$(curl -sS --connect-timeout 1 --max-time 2 -o /dev/null "
        f"-w '%{{http_code}}' http://127.0.0.1:{port}/health 2>/dev/null || true); "
        "test \"$status\" != 000; "
        "fi"
    )
    result = remote.run(command)
    if result.returncode == 0:
        raise SafetyError(
            f"porta remota {port} ja esta ocupada; escolha outra com --port. "
            "A porta 8080 do POM nao e usada pelo harness."
        )


def _remote_source_info(remote: Remote, source: str) -> dict[str, str]:
    source_q = _shell_path(source)
    result = remote.run(
        f"test -d {source_q} && git -C {source_q} remote -v && "
        f"printf '\nCOMMIT=' && git -C {source_q} rev-parse HEAD && "
        f"printf '\nBRANCH=' && git -C {source_q} branch --show-current"
    )
    if result.returncode != 0:
        raise ConfigError(
            f"arvore llama.cpp indisponivel em {source}: {(result.stderr or result.stdout).strip()}"
        )
    if "pereirajair/llama.cpp" not in result.stdout:
        raise ConfigError(
            f"a arvore {source} nao parece ser o fork do POM (remote pereirajair/llama.cpp ausente)"
        )
    commit = ""
    branch = ""
    for line in result.stdout.splitlines():
        if line.startswith("COMMIT="):
            commit = line.removeprefix("COMMIT=").strip()
        if line.startswith("BRANCH="):
            branch = line.removeprefix("BRANCH=").strip()
    return {"source": source, "commit": commit, "branch": branch}


def _prebuilt_cache_is_compatible(cache_text: str, source: str) -> bool:
    """Exige prova no CMakeCache de que o binario e CUDA/server deste fork."""

    values: dict[str, str] = {}
    for line in cache_text.splitlines():
        if ":" not in line or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.split(":", 1)[0]
        values[key] = value.strip()
    configured_source = values.get("CMAKE_HOME_DIRECTORY", "").rstrip("/")
    expected_source = source.rstrip("/")
    return (
        configured_source == expected_source
        and values.get("GGML_CUDA") == "ON"
        and values.get("LLAMA_BUILD_SERVER") == "ON"
    )


def _resolve_cuda_compiler(remote: Remote, override: str | None) -> str:
    if override:
        result = remote.run(f"test -x {_shell_path(override)}")
        if result.returncode != 0:
            raise RuntimeError(f"CUDA compiler nao e executavel na ksia: {override}")
        return override
    result = remote.run(
        "for candidate in /usr/local/cuda/bin/nvcc /usr/local/cuda-13.0/bin/nvcc "
        "/usr/local/cuda-12.9/bin/nvcc; do "
        "if test -x \"$candidate\"; then printf '%s\\n' \"$candidate\"; exit 0; fi; "
        "done; command -v nvcc"
    )
    compiler = next((line.strip() for line in result.stdout.splitlines() if line.strip()), "")
    if result.returncode != 0 or not compiler:
        raise RuntimeError(
            "CMake nao encontrou nvcc na ksia; use --cuda-compiler PATH ou "
            "LLAMACPP_CUDA_COMPILER"
        )
    return compiler


def _resolve_server(
    remote: Remote,
    source: str,
    build_dir: str | None,
    build_jobs: int,
    cuda_architectures: str | None,
    cuda_compiler: str | None = None,
) -> tuple[str, dict[str, str]]:
    info = _remote_source_info(remote, source)
    candidate = f"{source.rstrip('/')}/build/bin/llama-server"
    cache_path = f"{source.rstrip('/')}/build/CMakeCache.txt"
    existing = remote.run(
        f"test -x {_shell_path(candidate)} && test -f {_shell_path(cache_path)} && "
        f"cat {_shell_path(cache_path)}"
    )
    if existing.returncode == 0 and _prebuilt_cache_is_compatible(existing.stdout, source):
        return candidate, info | {"build": "prebuilt"}

    external_build = build_dir or f"/tmp/rdma-bench-llamacpp-{info['commit'][:12]}"
    compiler = _resolve_cuda_compiler(remote, cuda_compiler)
    cmake_options = [
        "cmake",
        "-S",
        source,
        "-B",
        external_build,
        "-DGGML_CUDA=ON",
        "-DLLAMA_BUILD_SERVER=ON",
        "-DCMAKE_BUILD_TYPE=Release",
        f"-DCMAKE_CUDA_COMPILER={compiler}",
    ]
    if cuda_architectures:
        cmake_options.append(f"-DCMAKE_CUDA_ARCHITECTURES={cuda_architectures}")
    command = (
        "set -eu; "
        "case "
        + _shell_path(external_build)
        + " in "
        + _shell_path(source.rstrip("/"))
        + "|"
        + _shell_path(source.rstrip("/") + "/")
        + "*"
        + ") echo 'LLAMACPP_BUILD_DIR nao pode ficar dentro da arvore llama.cpp' >&2; exit 2;; esac; "
        + shlex.join(cmake_options)
        + "; cmake --build "
        + _shell_path(external_build)
        + " --target llama-server --parallel "
        + str(build_jobs)
        + "; test -x "
        + _shell_path(f"{external_build}/bin/llama-server")
    )
    result = remote.run("bash -c " + _shell_path(command), timeout=None)
    if result.returncode != 0:
        raise RuntimeError(
            "compilacao CUDA do llama-server falhou em build externo "
            f"{external_build}: {(result.stderr or result.stdout).strip()}"
        )
    return f"{external_build}/bin/llama-server", info | {
        "build": "compiled",
        "build_dir": external_build,
        "cuda_compiler": compiler,
    }


def _start_server(
    remote: Remote,
    server: str,
    model_path: str,
    plan: dict[str, Any],
    port: int,
    log_path: str,
) -> int:
    flags = plan["llama_flags"]
    command_args = [
        server,
        "--host",
        "127.0.0.1",
        "--port",
        str(port),
        "-m",
        model_path,
        "-c",
        str(flags["ctx_size"]),
        "-b",
        str(flags["batch_size"]),
        "-ngl",
        str(flags["gpu_layers"]),
        "-np",
        str(flags["parallel"]),
    ]
    if flags["kv_unified"]:
        command_args.append("--kv-unified")
    if not flags["kv_offload"]:
        command_args.append("-nkvo")
    if flags["cache_type_k"] is not None:
        command_args.extend(["--cache-type-k", str(flags["cache_type_k"])])
    if flags["cache_type_v"] is not None:
        command_args.extend(["--cache-type-v", str(flags["cache_type_v"])])
    if flags["reasoning"] is not None:
        command_args.extend(["--reasoning", str(flags["reasoning"])])
    if flags["spec_type"] != "none":
        command_args.extend(
            [
                "--spec-type",
                flags["spec_type"],
                "--spec-draft-n-max",
                str(flags["spec_draft_n_max"]),
                "--spec-draft-p-min",
                str(flags["spec_draft_p_min"]),
            ]
        )
    command = (
        "nohup "
        + shlex.join(command_args)
        + " > "
        + _shell_path(log_path)
        + " 2>&1 < /dev/null & printf '%s\\n' $!"
    )
    result = remote.run(command)
    if result.returncode != 0:
        raise RuntimeError(f"nao foi possivel iniciar llama-server: {result.stderr.strip()}")
    try:
        pid = int(result.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError) as exc:
        raise RuntimeError(f"PID invalido devolvido ao iniciar llama-server: {result.stdout!r}") from exc
    return pid


def _server_alive(remote: Remote, pid: int) -> bool:
    return remote.run(f"kill -0 {pid}").returncode == 0


def _tail_log(remote: Remote, path: str, lines: int = 120) -> str:
    """Preserva o arranque e o fim do log para auditar flags e timings."""

    try:
        result = remote.run(
            f"if test -f {_shell_path(path)}; then "
            f"head -n {lines} {_shell_path(path)}; "
            f"printf '\\n--- llama-server log tail ---\\n'; "
            f"tail -n {lines} {_shell_path(path)}; fi",
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired):
        return ""
    return result.stdout.strip()


def _wait_health(remote: Remote, pid: int, port: int, timeout: int) -> str:
    deadline = time.monotonic() + timeout
    last_error = ""
    while time.monotonic() < deadline:
        result = remote.run(
            f"curl -fsS --connect-timeout 3 --max-time 8 http://127.0.0.1:{port}/health"
        )
        if result.returncode == 0:
            try:
                health = json.loads(result.stdout)
            except json.JSONDecodeError:
                health = {}
            if isinstance(health, dict):
                return result.stdout.strip()
        last_error = (result.stderr or result.stdout).strip()
        if not _server_alive(remote, pid):
            raise RuntimeError(
                "llama-server terminou antes de ficar pronto. "
                f"Ultimo erro de health: {last_error}"
            )
        time.sleep(2)
    raise TimeoutError(
        f"timeout esperando /health do llama-server em {timeout}s: {last_error}"
    )


def _request_stream(
    remote: Remote,
    port: int,
    payload: dict[str, Any],
    request_timeout: int,
) -> dict[str, Any]:
    encoded = base64.b64encode(json.dumps(payload, ensure_ascii=False).encode()).decode()
    curl_args = [
        "curl",
        "-sS",
        "-N",
        "--connect-timeout",
        "10",
        "--max-time",
        str(request_timeout),
        "-H",
        "Content-Type: application/json",
        "--data-binary",
        "@-",
        "-w",
        "\n__LLAMA_HTTP_STATUS__%{http_code}\n",
        f"http://127.0.0.1:{port}/completion",
    ]
    script = (
        "set -o pipefail; "
        f"printf '%s' {_shell_path(encoded)} | base64 -d | "
        + shlex.join(curl_args)
        + " | { first=0; while IFS= read -r line; do "
        "case \"$line\" in "
        "data:\\ *) if [ \"$first\" -eq 0 ]; then "
        "printf '%s\\n' '__LLAMA_FIRST_TOKEN__'; first=1; fi; "
        "printf '%s\\n' \"$line\" ;; "
        "__LLAMA_HTTP_STATUS__*) printf '%s\\n' \"$line\" ;; "
        "esac; done; };"
    )
    start = time.monotonic()
    process = subprocess.Popen(
        [
            "ssh",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            f"ConnectTimeout={remote.connect_timeout}",
            "-o",
            "StrictHostKeyChecking=accept-new",
            remote.host,
            "bash -c " + shlex.quote(script),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    lines: list[str] = []
    first_token_ms: float | None = None
    assert process.stdout is not None
    for line in process.stdout:
        if line.strip() == "__LLAMA_FIRST_TOKEN__":
            if first_token_ms is None:
                first_token_ms = (time.monotonic() - start) * 1000
            continue
        lines.append(line)
    returncode = process.wait()
    stderr = process.stderr.read().strip() if process.stderr is not None else ""
    total_ms = (time.monotonic() - start) * 1000
    status = None
    data_lines: list[str] = []
    for line in lines:
        if line.startswith("__LLAMA_HTTP_STATUS__"):
            try:
                status = int(line.removeprefix("__LLAMA_HTTP_STATUS__").strip())
            except ValueError:
                pass
        else:
            data_lines.append(line)
    return parse_sse_result(
        data_lines,
        first_token_ms,
        total_ms,
        status,
        process_error=(f"ssh/curl rc={returncode}: {stderr}" if returncode else ""),
    )


def _timestamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _safe_slug(label: str) -> str:
    stem = label.removesuffix(".gguf")
    safe = "".join(char if char.isalnum() or char in "._-" else "-" for char in stem)
    while "--" in safe:
        safe = safe.replace("--", "-")
    return safe.strip("-") or "modelo"


def _write_csv(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=CSV_FIELDS, extrasaction="ignore")
        writer.writeheader()
        for row in rows:
            writer.writerow({field: "" if row.get(field) is None else row.get(field, "") for field in CSV_FIELDS})


def _write_summary(
    path: Path,
    plan: dict[str, Any],
    metadata: dict[str, Any],
    rows: list[dict[str, Any]],
    status: str,
    reason: str = "",
) -> None:
    summaries = summary_rows(rows)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        handle.write("# llama.cpp puro - resumo do benchmark\n\n")
        handle.write("Este arquivo mede o llama.cpp puro para comparacao direta com o relatorio POM do mesmo modelo/config.\n\n")
        handle.write(f"- status: `{status}`\n")
        handle.write(f"- modelo: `{plan.get('model_label')}`\n")
        handle.write(f"- deployment: `{plan.get('deployment_id')}`\n")
        handle.write(f"- n_ctx efetivo: `{plan.get('effective_n_ctx')}`\n")
        handle.write(f"- repeticoes medidas: `{metadata.get('repetitions')}`; warmup: `1` por execucao\n")
        if reason:
            handle.write(f"- motivo: {reason}\n")
        handle.write("\n## Medianas por prompt\n\n")
        handle.write("| backend | prompt_id | reps ok | generation_tok/s | prefill_ms | prefill_tok/s | ttft_ms | total_ms | prompt_tokens | output_tokens | tokens_cached |\n")
        handle.write("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n")
        for row in summaries:
            values = [row.get(field, "") if row.get(field) is not None else "-" for field in SUMMARY_FIELDS]
            handle.write("| " + " | ".join(str(value) for value in values) + " |\n")
        handle.write("\nO CSV contem uma linha por warmup/chamada medida/erro. O cabeçalho identifica `backend=llama.cpp puro`; `ttft_ms` e o tempo observado ate o primeiro evento SSE de token, e `total_ms` e o tempo de parede da chamada SSH+curl local na ksia.\n")


def _write_run_report(
    path: Path,
    plan: dict[str, Any],
    metadata: dict[str, Any],
    rows: list[dict[str, Any]],
    status: str,
    reason: str = "",
) -> None:
    summaries = summary_rows(rows)
    flags = metadata.get("server_args") or plan.get("llama_flags", {})
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        handle.write("# llama.cpp puro - relatorio da corrida\n\n")
        handle.write("Este relatorio mede somente o backend llama.cpp puro para comparacao com o POM do mesmo modelo.\n\n")
        handle.write(f"- status: `{status}`\n")
        handle.write(f"- modelo: `{plan.get('model_label')}`\n")
        handle.write(f"- deployment: `{plan.get('deployment_id')}`\n")
        handle.write(f"- n_ctx efetivo: `{plan.get('effective_n_ctx')}`\n")
        handle.write(f"- geometria: `n_seq_max={flags.get('n_seq_max')}`, `kv_unified={flags.get('kv_unified')}`, `-c={flags.get('ctx_size')}`, `-np={flags.get('parallel')}`\n")
        handle.write(f"- MTP/especulacao efetiva: `{flags.get('speculation_source', 'none')}`, tipo `{flags.get('spec_type', 'none')}`, n_max `{flags.get('spec_draft_n_max', 0)}`, p_min `{flags.get('spec_draft_p_min', 0.0)}`\n")
        if flags.get("cache_type_k") or flags.get("cache_type_v"):
            handle.write(f"- cache KV: K `{flags.get('cache_type_k')}`, V `{flags.get('cache_type_v')}`\n")
        if reason:
            handle.write(f"- motivo: {reason}\n")
        handle.write("\n## Medianas por prompt\n\n")
        handle.write("| backend | prompt_id | reps ok | generation_tok/s | prefill_ms | prefill_tok/s | ttft_ms | total_ms | prompt_tokens | output_tokens | tokens_cached |\n")
        handle.write("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n")
        for row in summaries:
            values = [row.get(field, "") if row.get(field) is not None else "-" for field in SUMMARY_FIELDS]
            handle.write("| " + " | ".join(str(value) for value in values) + " |\n")
        handle.write("\nO CSV e o summary.md com o mesmo timestamp ficam ao lado deste relatorio.\n")


def _write_metadata(path: Path, metadata: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(metadata, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def _parse_args(argv: Sequence[str]) -> argparse.Namespace:
    env = os.environ
    parser = argparse.ArgumentParser(
        description="Sobe llama-server do fork POM na ksia e mede a bateria bench-routes."
    )
    parser.add_argument("config_json", help="snapshot *.config.json produzido pelo pom-bench")
    parser.add_argument("--model", help="label do modelo a selecionar")
    parser.add_argument("--deploy-id", help="ID exato da implantacao a selecionar")
    parser.add_argument("--n-ctx", type=int, help="override do n_ctx efetivo")
    parser.add_argument("--port", type=int, default=8090, help="porta remota dedicada (padrao: 8090)")
    parser.add_argument(
        "--remote-host",
        default=env.get("LLAMACPP_SSH_HOST", "ksia"),
        help="host SSH (padrao: LLAMACPP_SSH_HOST ou ksia)",
    )
    parser.add_argument(
        "--llama-src",
        default=env.get("LLAMACPP_SRC", "/data/fm-prefixo-src/llama.cpp"),
        help="arvore remota do fork llama.cpp",
    )
    parser.add_argument(
        "--model-root",
        default=env.get("LLAMACPP_MODEL_ROOT", "/data/models"),
        help="raiz remota para resolver label em GGUF",
    )
    parser.add_argument("--model-path", default=env.get("LLAMACPP_MODEL_PATH"))
    parser.add_argument("--build-dir", default=env.get("LLAMACPP_BUILD_DIR"))
    parser.add_argument("--build-jobs", type=int, default=int(env.get("LLAMACPP_BUILD_JOBS", "2")))
    parser.add_argument("--cuda-architectures", default=env.get("LLAMACPP_CUDA_ARCHITECTURES"))
    parser.add_argument("--cuda-compiler", default=env.get("LLAMACPP_CUDA_COMPILER"))
    parser.add_argument(
        "--min-free-mb",
        type=int,
        default=int(env.get("LLAMACPP_MIN_FREE_MB", "14000")),
        help="VRAM livre minima por GPU antes do launch (padrao: 14000)",
    )
    parser.add_argument(
        "--output-dir",
        default=env.get("LLAMACPP_OUTPUT_DIR", DEFAULT_OUTPUT_DIR),
        help="diretorio local dos artefatos (padrao: /Users/jairpereira/scripts/reports)",
    )
    parser.add_argument("--csv")
    parser.add_argument("--summary")
    parser.add_argument("--metadata")
    parser.add_argument("--report", help="relatorio markdown da corrida")
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--max-tokens", type=int, default=200)
    parser.add_argument(
        "--prefill-words",
        type=int,
        default=int(env.get("POM_BENCH_PREFILL_WORDS", "1200")),
    )
    parser.add_argument(
        "--prefill-max-tokens",
        type=int,
        default=int(env.get("POM_BENCH_PREFILL_MAX_TOKENS", "8")),
    )
    parser.add_argument("--startup-timeout", type=int, default=900)
    parser.add_argument("--request-timeout", type=int, default=600)
    parser.add_argument("--dry-run", action="store_true", help="seleciona e imprime o plano sem SSH/launch")
    return parser.parse_args(argv)


def _resolve_model_path(remote: Remote, root: str, label: str, override: str | None) -> str:
    if override:
        result = remote.run(f"test -f {_shell_path(override)} && test -r {_shell_path(override)}")
        if result.returncode != 0:
            raise ConfigError(f"--model-path nao e legivel na ksia: {override}")
        return override
    filename = label if label.casefold().endswith(".gguf") else f"{label}.gguf"
    result = remote.run(
        f"find {_shell_path(root)} -type f -iname {_shell_path(filename)} -print"
    )
    matches = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    if result.returncode != 0 or not matches:
        raise ConfigError(
            f"nao encontrei GGUF com nome {filename!r} sob {root}; "
            "use --model-path para um caminho explicito"
        )
    if len(matches) > 1:
        raise ConfigError(
            f"GGUF ambiguo para {filename!r}: {', '.join(matches)}; use --model-path"
        )
    return matches[0]


def _cleanup_server(remote: Remote, pid: int | None, log_path: str | None) -> bool:
    """Encerra apenas o PID criado e retorna se o cleanup remoto foi confirmado."""

    cleanup_ok = True
    if pid is not None:
        try:
            result = remote.run(
                "pid="
                + str(pid)
                + "; if kill -0 \"$pid\" 2>/dev/null; then "
                + "command_name=$(ps -p \"$pid\" -o comm= 2>/dev/null || true); "
                + "case \"$command_name\" in llama-server) ;; *) "
                + "echo \"PID $pid nao e llama-server ($command_name)\" >&2; exit 42;; esac; "
                + "kill -TERM \"$pid\" 2>/dev/null || true; "
                + "for i in $(seq 1 15); do kill -0 \"$pid\" 2>/dev/null || break; sleep 1; done; "
                + "kill -KILL \"$pid\" 2>/dev/null || true; fi",
                timeout=30,
            )
            if result.returncode != 0:
                cleanup_ok = False
                print(
                    f"aviso: cleanup do PID {pid} falhou: {(result.stderr or result.stdout).strip()}",
                    file=sys.stderr,
                )
        except (OSError, subprocess.TimeoutExpired) as exc:
            cleanup_ok = False
            print(f"aviso: cleanup remoto falhou: {exc}", file=sys.stderr)
    if log_path and cleanup_ok:
        try:
            result = remote.run(f"rm -f {_shell_path(log_path)}", timeout=30)
            if result.returncode != 0:
                cleanup_ok = False
                print(
                    f"aviso: remocao do log remoto falhou: {(result.stderr or result.stdout).strip()}",
                    file=sys.stderr,
                )
        except (OSError, subprocess.TimeoutExpired) as exc:
            cleanup_ok = False
            print(f"aviso: remocao do log remoto falhou: {exc}", file=sys.stderr)
    return cleanup_ok


def _row(label: str, phase: str, prompt_id: str, repetition: int, result: dict[str, Any]) -> dict[str, Any]:
    return {
        "backend": "llama.cpp puro",
        "label": label,
        "timestamp_utc": _timestamp(),
        "phase": phase,
        "prompt_id": prompt_id,
        "repetition": repetition,
        **result,
    }


def main(argv: Sequence[str] | None = None) -> int:
    args = _parse_args(argv if argv is not None else sys.argv[1:])
    try:
        _require_local_commands()
        snapshot_path = Path(args.config_json)
        snapshot = json.loads(snapshot_path.read_text(encoding="utf-8"))
        if not isinstance(snapshot, dict):
            raise ConfigError("config_json precisa conter um objeto JSON")
        deployment = merge_runtime_node_config(
            snapshot, select_deployment(snapshot, args.model, args.deploy_id)
        )
        plan = build_bench_plan(deployment, args.n_ctx)
        plan["remote_host"] = args.remote_host
        plan["llama_source"] = args.llama_src
        plan["model_root"] = args.model_root
        plan["port"] = args.port
        if args.dry_run:
            print(json.dumps(plan, indent=2, ensure_ascii=False))
            return 0
    except (OSError, json.JSONDecodeError, ConfigError) as exc:
        print(f"erro: {exc}", file=sys.stderr)
        return 2

    if args.port < 1 or args.port > 65535:
        print("erro: --port fora do intervalo 1..65535", file=sys.stderr)
        return 2
    if args.repetitions <= 0 or args.max_tokens <= 0 or args.prefill_words <= 0 or args.prefill_max_tokens <= 0:
        print("erro: repeticoes e limites de tokens/palavras devem ser positivos", file=sys.stderr)
        return 2
    if args.min_free_mb <= 0 or args.build_jobs <= 0:
        print("erro: --min-free-mb e --build-jobs devem ser positivos", file=sys.stderr)
        return 2

    now = datetime.now(timezone.utc).strftime("%Y-%m-%d_%H%M%S")
    slug = _safe_slug(plan["model_label"] or "modelo")
    output_dir = Path(args.output_dir)
    csv_path = Path(args.csv) if args.csv else output_dir / f"{now}-{slug}.llamacpp.csv"
    summary_path = Path(args.summary) if args.summary else output_dir / f"{now}-{slug}.llamacpp.summary.md"
    metadata_path = Path(args.metadata) if args.metadata else output_dir / f"{now}-{slug}.llamacpp.metadata.json"
    report_path = Path(args.report) if args.report else output_dir / f"{now}-{slug}-llama.md"
    rows: list[dict[str, Any]] = []
    metadata: dict[str, Any] = {
        "backend": "llama.cpp puro",
        "status": "starting",
        "config_snapshot": str(snapshot_path),
        "deployment": plan,
        "repetitions": args.repetitions,
        "max_tokens": args.max_tokens,
        "prefill_words": args.prefill_words,
        "prefill_max_tokens": args.prefill_max_tokens,
        "csv": str(csv_path),
        "summary": str(summary_path),
        "metadata": str(metadata_path),
        "report": str(report_path),
        "safety_policy": f"cada GPU precisa de pelo menos {args.min_free_mb} MiB livres antes do launch",
    }
    _write_csv(csv_path, rows)
    _write_metadata(metadata_path, metadata)

    remote = Remote(args.remote_host)
    server_pid: int | None = None
    remote_log: str | None = None
    local_log_path = summary_path.with_suffix(".server.log")
    server_log = ""
    status = "error"
    reason = ""
    try:
        vram = _remote_vram(remote, args.min_free_mb)
        metadata["vram_before_launch"] = vram
        _write_metadata(metadata_path, metadata)
        _check_port(remote, args.port)
        source_info = _remote_source_info(remote, args.llama_src)
        server, build_info = _resolve_server(
            remote,
            args.llama_src,
            args.build_dir,
            args.build_jobs,
            args.cuda_architectures,
            args.cuda_compiler,
        )
        model_path = _resolve_model_path(remote, args.model_root, plan["model_label"], args.model_path)
        metadata["llama_source"] = source_info | build_info
        metadata["server_binary"] = server
        metadata["model_path"] = model_path
        metadata["server_args"] = {
            "host": "127.0.0.1",
            "port": args.port,
            "model": model_path,
            **plan["llama_flags"],
        }
        # O build externo e a resolucao do GGUF podem demorar. Reconfirma o
        # estado imediatamente antes do launch para nunca competir com o POM
        # caso ele tenha voltado a ocupar VRAM nesse intervalo.
        metadata["vram_pre_launch"] = _remote_vram(remote, args.min_free_mb)
        _check_port(remote, args.port)
        remote_log = f"/tmp/rdma-bench-llamacpp-{os.getpid()}-{args.port}.log"
        server_pid = _start_server(remote, server, model_path, plan, args.port, remote_log)
        metadata["server_pid"] = server_pid
        metadata["status"] = "starting-server"
        _write_metadata(metadata_path, metadata)
        metadata["health"] = json.loads(_wait_health(remote, server_pid, args.port, args.startup_timeout))
        metadata["status"] = "measuring"
        _write_metadata(metadata_path, metadata)

        prompts = build_prompts(args.prefill_words)
        sampling = plan["pom_config"]
        warmup = _request_stream(
            remote,
            args.port,
            make_completion_payload(HEATING_PROMPT, args.max_tokens, sampling),
            args.request_timeout,
        )
        warmup_row = _row(plan["model_label"], "warmup", "warmup", 0, warmup)
        rows.append(warmup_row)
        _write_csv(csv_path, rows)
        if warmup["status"] != "ok":
            raise RuntimeError(f"warmup falhou: {warmup.get('error')}")

        for prompt_id in PROMPT_IDS:
            for repetition in range(1, args.repetitions + 1):
                prompt = (
                    _random_prefill_prompt(args.prefill_words)
                    if prompt_id == "p-prefill-aleatorio"
                    else prompts[prompt_id]
                )
                max_tokens = args.prefill_max_tokens if prompt_id == "p-prefill-aleatorio" else args.max_tokens
                result = _request_stream(
                    remote,
                    args.port,
                    make_completion_payload(prompt, max_tokens, sampling),
                    args.request_timeout,
                )
                row = _row(plan["model_label"], "measurement", prompt_id, repetition, result)
                rows.append(row)
                _write_csv(csv_path, rows)
                if result["status"] != "ok":
                    print(
                        f"aviso: {prompt_id} repeticao {repetition} falhou: {result.get('error')}",
                        file=sys.stderr,
                    )
        incomplete = incomplete_prompts(rows, args.repetitions)
        measured_prompt_ids = {
            str(row.get("prompt_id"))
            for row in rows
            if row.get("phase") == "measurement"
        }
        incomplete.extend(
            prompt_id for prompt_id in PROMPT_IDS if prompt_id not in measured_prompt_ids
        )
        incomplete = list(dict.fromkeys(incomplete))
        if incomplete:
            raise RuntimeError(
                "medicao incompleta; prompts sem todas as repeticoes validas: "
                + ", ".join(incomplete)
            )
        status = "ok"
        metadata["status"] = status
    except SafetyError as exc:
        status = "blocked_vram"
        reason = str(exc)
        metadata["status"] = status
        metadata["safety_failure"] = reason
        print(f"parada de seguranca: {reason}", file=sys.stderr)
        return_code = 3
    except Exception as exc:  # lifecycle cleanup is handled below
        status = "error"
        reason = str(exc)
        metadata["status"] = status
        metadata["error"] = reason
        print(f"erro: {reason}", file=sys.stderr)
        return_code = 1
    else:
        return_code = 0
    finally:
        if remote_log:
            server_log = _tail_log(remote, remote_log)
        cleanup_ok = _cleanup_server(remote, server_pid, remote_log)
        metadata["cleanup_ok"] = cleanup_ok
        if not cleanup_ok:
            cleanup_reason = "nao foi possivel confirmar o encerramento do llama-server remoto"
            if reason:
                reason += "; " + cleanup_reason
            else:
                reason = cleanup_reason
            status = "cleanup_error"
            return_code = 1
        if server_log:
            local_log_path.write_text(server_log + "\n", encoding="utf-8")
            metadata["server_log"] = str(local_log_path)
        metadata["status"] = status
        _write_csv(csv_path, rows)
        _write_summary(summary_path, plan, metadata, rows, status, reason)
        _write_run_report(report_path, plan, metadata, rows, status, reason)
        _write_metadata(metadata_path, metadata)

    print(f"backend=llama.cpp puro status={status}")
    print(f"modelo={plan['model_label']} deployment={plan['deployment_id']} n_ctx={plan['effective_n_ctx']}")
    print(f"csv={csv_path}")
    print(f"summary={summary_path}")
    print(f"metadata={metadata_path}")
    print(f"report={report_path}")
    if server_log:
        print(f"server_log={local_log_path}")
    if status == "ok":
        for item in summary_rows(rows):
            print(
                f"{item['prompt_id']}: generation_tok/s={item['generation_tok_s'] or '-'} "
                f"prefill_ms={item['prefill_ms'] or '-'} ttft_ms={item['ttft_ms'] or '-'} "
                f"total_ms={item['total_ms'] or '-'} reps={item['repetitions_ok']}"
            )
    elif reason:
        print(f"motivo={reason}", file=sys.stderr)
    return return_code


if __name__ == "__main__":
    raise SystemExit(main())
