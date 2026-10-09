#!/usr/bin/env python3
"""Gera a config de cada no do cluster do docker-compose.

As portas de um no vem da sua config, nao da linha de comando: com o diretorio
de dados vazio o binario sobe em modo setup e serve so a tela de configuracao,
sem carregar modelo. Como os cinco nos compartilham a pilha de rede da maquina
(network_mode: host), gerar as configs e o que evita colisao de porta.

A config de referencia (--base, por padrao pom.json na raiz do repo) e a
fonte da connection_key, das api_keys, do advertise_addr e das pastas de
modelos. A connection_key e a mesma em todo o cluster: e ela que autentica as
rotas /admin, /api/ui/{plan,apply,launch} e /v1. Nunca e gerada aqui, para nao
partir um cluster existente ao meio.

Uso:
    scripts/gen-node-configs.py [--base pom.json] [--nodes 5] [--force]
        [--advertise-addr HOST[:PORT]]
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# O no 1 herda as portas historicas da config principal (inferencia 8092); do 2
# em diante seguem a serie 818x, que e a que os arquivos pom-cpu*.json ja
# usavam. Gateway e p2p sao sequenciais a partir do no 1.
GATEWAY_BASE = 8080
P2P_BASE = 42777
INFERENCE = {1: 8092}
INFERENCE_BASE = 8181


def inference_port(n: int) -> int:
    return INFERENCE.get(n, INFERENCE_BASE + n - 1)


def _advertise_host_with_node_port(config: dict, advertise_addr: str) -> str:
    """Use the config's p2p port when the override carries an explicit port.

    ``advertise_addr`` is also the UDP endpoint from which libp2p derives its
    TCP endpoint. A full override is therefore a host base: each generated
    process keeps its own already assigned port. Applying the same full
    endpoint to every process makes all peers announce one socket and hides
    the other nodes behind it.
    """
    node_port = config.get("p2p_port")
    if not isinstance(node_port, int) or not 1 <= node_port <= 65535:
        return advertise_addr

    raw = advertise_addr.strip()
    if raw.startswith("["):
        close = raw.find("]")
        if close <= 1 or not raw[close + 1 :].startswith(":"):
            return advertise_addr
        host = raw[: close + 1]
        port = raw[close + 2 :]
    elif raw.count(":") == 1:
        host, port = raw.rsplit(":", 1)
    else:
        return advertise_addr

    if not host or not port.isdigit() or not 1 <= int(port) <= 65535:
        return advertise_addr
    return f"{host}:{node_port}"


def config_with_advertise_addr(config: dict, advertise_addr: str) -> dict:
    """Atualiza só o endpoint anunciado, preservando o restante da config.

    When a full endpoint is supplied, its host is shared but its port is
    specific to the process described by this config.
    """
    updated = dict(config)
    updated["advertise_addr"] = _advertise_host_with_node_port(config, advertise_addr)
    return updated


def node_config_for(
    base: dict,
    n: int,
    total: int,
    advertise_addr: str | None = None,
) -> dict:
    cfg = dict(base)
    cfg["role"] = "principal" if n == 1 else "node"
    cfg["node_name"] = None if n == 1 else f"ksia-{n}"
    cfg["gateway_port"] = GATEWAY_BASE + n - 1
    cfg["p2p_port"] = P2P_BASE + n - 1
    cfg["inference_port"] = inference_port(n)
    # So o no 1 escuta na porta de descoberta 42777, entao o broadcast da LAN
    # nao alcanca os outros quatro: sem bootstrap explicito eles ficariam mudos
    # entre si. Todos apontam para o principal, que devolve a lista completa.
    cfg["bootstrap_nodes"] = [] if n == 1 else [f"127.0.0.1:{P2P_BASE}"]
    # As chaves de API sao credenciais de harness e vivem no principal; copia-las
    # para os outros quatro so multiplicaria o segredo sem necessidade.
    cfg["api_keys"] = base.get("api_keys", []) if n == 1 else []
    if advertise_addr is not None:
        cfg = config_with_advertise_addr(cfg, advertise_addr)
    return cfg


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    p = argparse.ArgumentParser()
    p.add_argument("--base", default=str(root / "pom.json"))
    p.add_argument("--nodes", type=int, default=5)
    p.add_argument("--force", action="store_true",
                   help="sobrescreve configs que ja existem")
    p.add_argument(
        "--advertise-addr",
        help="atualiza advertise_addr sem sobrescrever os demais campos",
    )
    args = p.parse_args()

    base_path = Path(args.base)
    if not base_path.is_file():
        print(f"config de referencia nao encontrada: {base_path}", file=sys.stderr)
        return 1
    base = json.loads(base_path.read_text())

    if not base.get("connection_key"):
        print(f"{base_path} nao tem connection_key; sem ela os nos nao se "
              f"autenticam entre si", file=sys.stderr)
        return 1

    for n in range(1, args.nodes + 1):
        destination = root / "tests" / "fixtures" / f"node-{n}"
        destination.mkdir(parents=True, exist_ok=True)
        target = destination / "pom.json"
        if target.exists() and not args.force:
            if args.advertise_addr is None:
                print(f"node-{n}: ja existe, mantido (use --force para sobrescrever)")
                continue
            existing = json.loads(target.read_text())
            updated = config_with_advertise_addr(existing, args.advertise_addr)
            if updated != existing:
                target.write_text(json.dumps(updated, indent=2) + "\n")
                print(f"node-{n}: advertise_addr atualizado")
            else:
                print(f"node-{n}: advertise_addr ja estava atualizado")
            continue
        cfg = node_config_for(base, n, args.nodes, args.advertise_addr)
        target.write_text(json.dumps(cfg, indent=2) + "\n")
        role = cfg["role"]
        print(f"node-{n}: {role:9} gateway {cfg['gateway_port']} "
              f"p2p {cfg['p2p_port']} inferencia {cfg['inference_port']}")

    print("\nA chave de identidade (.pom-node.key) de cada no e criada no "
          "primeiro arranque, uma por diretorio.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
