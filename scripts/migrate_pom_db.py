#!/usr/bin/env python3
"""Preserve pre-0.0.1 local state in an already-created ``pom.db``.

This is intentionally a one-off operator tool, not an application migration.
The node never imports or calls this file at startup. It copies the old
``deployments/*.json``, ``audit.sqlite3``, and credentials from ``pom.json``;
all source files remain untouched. Any ambiguous or malformed record aborts
the transaction instead of silently dropping it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import sqlite3
import sys
from pathlib import Path
from typing import Any, Iterable


DEPLOY_ID = re.compile(r"d-[0-9a-f]{8}\Z")
AUDIT_COLUMNS = (
    "id",
    "received_at",
    "completed_at",
    "route",
    "model",
    "api_key_id",
    "api_key_label",
    "api_key_node_id",
    "entry_node_id",
    "prompt",
    "request_details",
    "result",
    "telemetry",
    "timings",
    "hops",
    "source",
    "prompt_tokens",
    "completion_tokens",
    "duration_ms",
    "status",
    "error",
)
REQUIRED_TABLES = {
    "deployments",
    "api_keys",
    "api_key_revocations",
    "model_history",
    "model_slices",
    "local_settings",
    "local_secrets",
    "chat_conversations",
    "chat_messages",
    "audit_requests",
    "audit_request_hops",
    "audit_prompt_fts",
}


class MigrationError(RuntimeError):
    """An unsafe source record or incompatible target."""


def fail(message: str) -> None:
    raise MigrationError(message)


def table_names(connection: sqlite3.Connection) -> set[str]:
    rows = connection.execute(
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
    )
    return {row[0] for row in rows}


def check_target(connection: sqlite3.Connection) -> None:
    missing = REQUIRED_TABLES - table_names(connection)
    if missing:
        fail("o pom.db não tem o esquema da aplicação; inicie o nó uma vez antes")
    connection.execute("PRAGMA busy_timeout = 5000")
    connection.execute("PRAGMA journal_mode = WAL")


def owner_only(path: Path) -> None:
    try:
        path.chmod(0o600)
    except OSError as error:
        fail(f"não foi possível restringir as permissões do banco: {error}")


def canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def u32(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= 2**32 - 1


def u64(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= 2**64 - 1


def finite_number(value: Any) -> bool:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def validate_deployment_shape(value: dict[str, Any], name: str) -> None:
    if not u32(value.get("deployment_version")) or value["deployment_version"] > 1:
        fail(f"registro de implantação com versão inválida: {name}")
    if value.get("kind") not in {"shards", "whole", "tensor_parallel"}:
        fail(f"registro de implantação com tipo inválido: {name}")
    for field in ("created_at", "activated_at"):
        if not isinstance(value.get(field), str):
            fail(f"registro de implantação sem {field}: {name}")

    model = value.get("model")
    if not isinstance(model, dict) or not isinstance(model.get("label"), str):
        fail(f"modelo inválido no registro de implantação: {name}")
    if not model["label"].strip() or not u32(model.get("total_layers")):
        fail(f"metadados do modelo inválidos no registro de implantação: {name}")
    if model.get("pom_id") is not None and not isinstance(model["pom_id"], str):
        fail(f"pom_id inválido no registro de implantação: {name}")
    identity = model.get("identity")
    if identity is not None:
        if not isinstance(identity, dict) or identity.get("kind") not in {"sha256", "pom_id"}:
            fail(f"identidade do modelo inválida no registro de implantação: {name}")
        identity_field = "sha256" if identity["kind"] == "sha256" else "pom_id"
        if not isinstance(identity.get(identity_field), str) or not identity[identity_field]:
            fail(f"identidade do modelo sem conteúdo: {name}")

    nodes = value.get("nodes")
    if not isinstance(nodes, list) or not nodes:
        fail(f"registro de implantação sem nós: {name}")
    unsigned = {
        "start_layer",
        "end_layer",
        "peer_index",
        "peer_count",
    }
    booleans = {"primary"}
    config_u32 = {
        "n_gpu_layers",
        "n_ctx",
        "fit_memory_margin_mb",
        "n_batch",
        "max_new_tokens",
        "max_sessions",
        "max_queue_requests",
        "batch_window_ms",
        "max_batch_sequences",
        "max_batch_tokens",
        "prefill_chunk_tokens",
        "prefill_max_inflight_chunks",
        "top_k",
        "repeat_last_n",
        "cycle_detection_min_window_tokens",
        "cycle_detection_max_window_tokens",
        "cycle_detection_repetitions",
        "speculative_k",
        "speculative_lookup_window",
        "mtp_draft_tokens",
    }
    config_bool = {
        "offload_kqv",
        "fit_memory",
        "batching_enabled",
        "cycle_detection_enabled",
        "no_thinking",
        "no_embeddings",
        "speculative_decoding_enabled",
        "mtp_enabled",
        "mtp_cpu_fallback_enabled",
    }
    config_float = {"temperature", "top_p", "repeat_penalty", "mtp_min_probability"}
    for node in nodes:
        if not isinstance(node, dict) or not isinstance(node.get("node_id"), str):
            fail(f"nó inválido no registro de implantação: {name}")
        for field in unsigned:
            if not u32(node.get(field)):
                fail(f"campo {field} inválido no registro de implantação: {name}")
        if "tp_rank" in node and not u32(node["tp_rank"]):
            fail(f"campo tp_rank inválido no registro de implantação: {name}")
        if node["start_layer"] > node["end_layer"]:
            fail(f"faixa invertida no registro de implantação: {name}")
        for field in ("hostname", "part_sha256"):
            if node.get(field) is not None and not isinstance(node[field], str):
                fail(f"campo {field} inválido no registro de implantação: {name}")
        for field in booleans:
            if field in node and not isinstance(node[field], bool):
                fail(f"campo {field} inválido no registro de implantação: {name}")
        config = node.get("config")
        if not isinstance(config, dict):
            fail(f"configuração ausente no registro de implantação: {name}")
        for field in config_u32:
            if field in config and not u32(config[field]):
                fail(f"configuração {field} inválida no registro de implantação: {name}")
        for field in config_bool:
            if field in config and not isinstance(config[field], bool):
                fail(f"configuração {field} inválida no registro de implantação: {name}")
        for field in config_float:
            if field in config and not finite_number(config[field]):
                fail(f"configuração {field} inválida no registro de implantação: {name}")


def deployment_record(path: Path) -> tuple[str, str, str, str, str, str]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"registro de implantação ilegível: {path.name}: {error}")
    if not isinstance(value, dict):
        fail(f"registro de implantação não é um objeto: {path.name}")
    validate_deployment_shape(value, path.name)
    deploy_id = value.get("deploy_id")
    model = value.get("model")
    kind = value.get("kind")
    created_at = value.get("created_at")
    activated_at = value.get("activated_at")
    if (
        not isinstance(deploy_id, str)
        or not DEPLOY_ID.fullmatch(deploy_id)
        or not isinstance(model, dict)
        or not isinstance(model.get("label"), str)
        or not model["label"].strip()
        or not isinstance(kind, str)
        or not isinstance(created_at, str)
        or not isinstance(activated_at, str)
    ):
        fail(f"registro de implantação sem campos obrigatórios: {path.name}")
    return (
        deploy_id,
        canonical_json(value),
        kind,
        model["label"],
        created_at,
        activated_at,
    )


def copy_deployments(
    connection: sqlite3.Connection, source: Path
) -> int:
    if not source.exists():
        return 0
    if not source.is_dir():
        fail("a fonte de implantações não é uma pasta")
    copied = 0
    entries = sorted(source.iterdir())
    unknown = [path.name for path in entries if not path.is_file() or path.suffix != ".json"]
    if unknown:
        fail("a pasta de implantações contém arquivos que não são registros JSON: " + ", ".join(unknown))
    for path in entries:
        deploy_id, payload, kind, label, created_at, activated_at = deployment_record(path)
        old = connection.execute(
            "SELECT payload_json FROM deployments WHERE deploy_id = ?", (deploy_id,)
        ).fetchone()
        if old is not None:
            try:
                same_payload = canonical_json(json.loads(old[0])) == payload
            except (TypeError, json.JSONDecodeError):
                same_payload = False
            if not same_payload:
                fail(f"implantação {deploy_id} já existe no destino com outro conteúdo")
            continue
        connection.execute(
            """INSERT INTO deployments (
                deploy_id, payload_json, kind, model_label, created_at, activated_at,
                active, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, 1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))""",
            (deploy_id, payload, kind, label, created_at, activated_at),
        )
        copied += 1
    return copied


def stable_legacy_id(key: dict[str, Any]) -> str:
    metadata = "|".join(
        str(key.get(field, ""))
        for field in ("label", "node_id", "created_at")
    )
    return "legacy-" + hashlib.sha256(metadata.encode()).hexdigest()[:12]


def copy_api_keys(connection: sqlite3.Connection, config: dict[str, Any]) -> int:
    keys = config.get("api_keys", [])
    if keys is None:
        return 0
    if not isinstance(keys, list):
        fail("api_keys no pom.json não é uma lista")
    copied = 0
    for key in keys:
        if not isinstance(key, dict):
            fail("uma API key antiga não é um objeto")
        secret = key.get("key")
        label = key.get("label")
        node_id = key.get("node_id", "")
        created_at = key.get("created_at")
        active_value = key.get("active", False)
        if (
            not isinstance(secret, str)
            or not secret
            or not isinstance(label, str)
            or not isinstance(node_id, str)
            or not isinstance(created_at, str)
            or not isinstance(active_value, bool)
        ):
            fail("API key antiga sem campos obrigatórios")
        key_id = key.get("id") or stable_legacy_id(key)
        if not isinstance(key_id, str) or not key_id:
            fail("API key antiga sem identidade estável")
        active = active_value
        old = connection.execute(
            "SELECT key, label, node_id, active, created_at FROM api_keys WHERE id = ?",
            (key_id,),
        ).fetchone()
        if old is not None:
            if old != (secret, label, node_id, int(active), created_at):
                fail(f"API key {key_id} já existe no destino com outro conteúdo")
            continue
        collision = connection.execute(
            "SELECT id FROM api_keys WHERE key = ?", (secret,)
        ).fetchone()
        if collision is not None:
            fail("duas API keys antigas têm o mesmo segredo")
        connection.execute(
            """INSERT INTO api_keys (id, key, label, node_id, active, created_at)
             VALUES (?, ?, ?, ?, ?, ?)""",
            (key_id, secret, label, node_id, int(active), created_at),
        )
        copied += 1
    return copied


def copy_preferences(connection: sqlite3.Connection, config: dict[str, Any]) -> tuple[int, int]:
    thermal = config.get("thermal")
    settings = 0
    secrets = 0
    if thermal is not None:
        if not isinstance(thermal, dict):
            fail("a política térmica antiga não é um objeto")
        required = {"limit_c", "hysteresis_c", "cooldown_secs"}
        if not required <= thermal.keys():
            fail("a política térmica antiga não tem todos os campos")
        if (
            not finite_number(thermal["limit_c"])
            or thermal["limit_c"] <= 0
            or thermal["limit_c"] > 150
            or not finite_number(thermal["hysteresis_c"])
            or thermal["hysteresis_c"] <= 0
            or thermal["hysteresis_c"] >= thermal["limit_c"]
            or not u64(thermal["cooldown_secs"])
            or thermal["cooldown_secs"] == 0
        ):
            fail("a política térmica antiga não passa pela validação do runtime")
        value = canonical_json(thermal)
        old = connection.execute(
            "SELECT value_json FROM local_settings WHERE setting_key = 'thermal_policy'"
        ).fetchone()
        if old is not None and canonical_json(json.loads(old[0])) != value:
            fail("a política térmica do destino tem outro conteúdo")
        if old is None:
            connection.execute(
                "INSERT INTO local_settings (setting_key, value_json) VALUES (?, ?)",
                ("thermal_policy", value),
            )
            settings = 1

    token = config.get("huggingface_token")
    if token is not None:
        if not isinstance(token, str) or not token.strip():
            fail("o token antigo do Hugging Face não é uma credencial válida")
        old = connection.execute(
            "SELECT value_text FROM local_secrets WHERE secret_key = 'huggingface_token'"
        ).fetchone()
        if old is not None and old[0] != token.strip():
            fail("o token do Hugging Face já existe no destino com outro conteúdo")
        if old is None:
            connection.execute(
                "INSERT INTO local_secrets (secret_key, value_text) VALUES (?, ?)",
                ("huggingface_token", token.strip()),
            )
            secrets = 1
    return settings, secrets


def source_columns(connection: sqlite3.Connection, table: str) -> set[str]:
    return {row[1] for row in connection.execute(f"PRAGMA table_info({table})")}


def copy_audit(connection: sqlite3.Connection, source: Path) -> int:
    if not source.exists():
        return 0
    if not source.is_file():
        fail("a fonte da auditoria não é um arquivo")
    try:
        old_connection = sqlite3.connect(source)
        old_connection.row_factory = sqlite3.Row
    except sqlite3.Error as error:
        fail(f"não foi possível abrir a auditoria antiga: {error}")
    try:
        columns = source_columns(old_connection, "audit_requests")
        missing = set(AUDIT_COLUMNS) - columns
        if missing:
            fail("a auditoria antiga não tem o esquema esperado")
        copied = 0
        quoted = ", ".join(AUDIT_COLUMNS)
        rows = old_connection.execute(f"SELECT {quoted} FROM audit_requests ORDER BY rowid")
        for row in rows:
            values = tuple(row[column] for column in AUDIT_COLUMNS)
            old = connection.execute(
                "SELECT " + quoted + " FROM audit_requests WHERE id = ?", (row["id"],)
            ).fetchone()
            if old is not None:
                if tuple(old) != values:
                    fail("um registro de auditoria já existe no destino com outro conteúdo")
            else:
                placeholders = ", ".join("?" for _ in AUDIT_COLUMNS)
                connection.execute(
                    f"INSERT INTO audit_requests ({quoted}) VALUES ({placeholders})", values
                )
                copied += 1
            connection.execute("DELETE FROM audit_prompt_fts WHERE request_id = ?", (row["id"],))
            connection.execute(
                "INSERT INTO audit_prompt_fts (request_id, prompt) VALUES (?, ?)",
                (row["id"], row["prompt"]),
            )

        hop_columns = source_columns(old_connection, "audit_request_hops")
        if {"request_id", "position", "node_id"} - hop_columns:
            fail("a auditoria antiga não tem o esquema de saltos esperado")
        for row in old_connection.execute(
            "SELECT request_id, position, node_id FROM audit_request_hops ORDER BY request_id, position"
        ):
            old = connection.execute(
                "SELECT node_id FROM audit_request_hops WHERE request_id = ? AND position = ?",
                (row[0], row[1]),
            ).fetchone()
            if old is not None and old[0] != row[2]:
                fail("um salto de auditoria já existe no destino com outro conteúdo")
            if old is None:
                connection.execute(
                    "INSERT INTO audit_request_hops (request_id, position, node_id) VALUES (?, ?, ?)",
                    (row[0], row[1], row[2]),
                )
        return copied
    finally:
        old_connection.close()


def load_config(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"pom.json ilegível: {error}")
    if not isinstance(value, dict):
        fail("pom.json não é um objeto")
    return value


def migrate(database: Path, deployments: Path, audit: Path, config_path: Path) -> None:
    if not database.exists():
        fail("pom.db não existe; deixe a aplicação criar o esquema antes de migrar")
    owner_only(database)
    connection = sqlite3.connect(database)
    try:
        check_target(connection)
        config = load_config(config_path)
        deployment_count = copy_deployments(connection, deployments)
        audit_count = copy_audit(connection, audit)
        api_key_count = copy_api_keys(connection, config)
        settings_count, secret_count = copy_preferences(connection, config)
        connection.commit()
    except Exception:
        connection.rollback()
        raise
    finally:
        connection.close()
        owner_only(database)
        for sidecar in (Path(str(database) + "-wal"), Path(str(database) + "-shm")):
            if sidecar.exists():
                owner_only(sidecar)
    print(
        "migração concluída: "
        f"{deployment_count} implantações, {audit_count} pedidos, "
        f"{api_key_count} API keys, {settings_count} preferências e "
        f"{secret_count} credenciais"
    )


def arguments(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    configured_dir = os.environ.get("POM_DB_DIR", "").strip()
    default_database = Path(configured_dir) / "pom.db" if configured_dir else Path("pom.db")
    parser.add_argument("--database", type=Path, default=default_database)
    parser.add_argument("--deployments", type=Path, default=Path("deployments"))
    parser.add_argument("--audit", type=Path, default=Path("audit.sqlite3"))
    parser.add_argument("--config", type=Path, default=Path("pom.json"))
    return parser.parse_args(list(argv))


def main(argv: Iterable[str] | None = None) -> int:
    try:
        args = arguments(sys.argv[1:] if argv is None else argv)
        migrate(args.database, args.deployments, args.audit, args.config)
    except MigrationError as error:
        print(f"migração interrompida: {error}", file=sys.stderr)
        return 2
    except (OSError, sqlite3.Error) as error:
        print(f"migração interrompida por erro de armazenamento: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
