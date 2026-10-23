#!/usr/bin/env python3
"""Drives an engine plugin through the real pom-plugin-host the way a POM node
does: ready, host.services, engine.load, engine.status until ready,
engine.upstream, POST /pom/generate, engine.unload, shutdown. It checks the
NDJSON lines and that no secret appears in a status or a service.

    verify-engine-plugin.py --host <pom-plugin-host> --lib <libpom_plugin_x.so> --code <plugin_code>
        --model <model file> [--runtime <dir with bin/server>] [--prompt "..."] [--timeout 600]

Without --runtime the plugin finds its runtime itself (build/runtime of its
source). To check the plugin alone, give a runtime whose bin/server is this
skill's mock-openai-server.py.
"""
import argparse
import http.client
import json
import os
import struct
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", required=True)
    parser.add_argument("--lib", required=True)
    parser.add_argument("--code", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--runtime")
    parser.add_argument("--prompt", default="Say hello in one sentence.")
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args()

    data = tempfile.mkdtemp(prefix="pom-engine-verify-")
    env = {"POM_PLUGIN_DB": os.path.join(data, "pom.db"), "PATH": os.environ.get("PATH", ""),
           "HOME": os.environ.get("HOME", data)}
    if args.runtime:
        env[f"POM_{args.code.upper()}_RUNTIME_DIR"] = os.path.abspath(args.runtime)
    host = subprocess.Popen([args.host, os.path.abspath(args.lib), args.code],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env)
    ids = iter(range(1, 1 << 30))

    def call(method, payload=None):
        body = json.dumps({"id": next(ids), "method": method, "payload": payload}).encode()
        host.stdin.write(struct.pack(">I", len(body)) + body)
        host.stdin.flush()
        size = struct.unpack(">I", host.stdout.read(4))[0]
        answer = json.loads(host.stdout.read(size))
        return answer

    def query(operation, **fields):
        answer = call("query", {"operation": operation, **fields})
        if not answer.get("ok"):
            raise SystemExit(f"{operation}: {answer.get('error')}")
        return answer["result"]

    def check(condition, message):
        print(("ok   " if condition else "FAIL ") + message, flush=True)
        if not condition:
            raise SystemExit(1)

    call("ready")
    services = query("host.services")["services"]
    check(len(services) == 1 and services[0]["kind"] == "inference", f"host.services: {services}")
    query("engine.load", params={"model_path": os.path.abspath(args.model), "model_label": "verify",
                                 "n_ctx": 4096, "max_sessions": 1, "kv": "f16", "gpus": [],
                                 "gpu_devices": [], "settings": {}})
    deadline = time.time() + args.timeout
    status = {}
    while time.time() < deadline:
        status = query("engine.status")
        if status["state"] in ("ready", "error"):
            break
        time.sleep(2)
    check(status.get("state") == "ready", f"engine.status: {status.get('state')} {status.get('detail') or ''}")
    upstream = query("engine.upstream")
    check(upstream["status"] == "ready" and upstream.get("port"), "engine.upstream is ready")
    token = upstream["token"]
    for answer in (query("engine.status"), query("host.services"),
                   query("rpc.status", params=None, profile="admin", node_id="n")):
        check(token not in json.dumps(answer), "no secret in a status or service")

    connection = http.client.HTTPConnection("127.0.0.1", upstream["port"], timeout=args.timeout)
    started = time.time()
    connection.request("POST", "/pom/generate", json.dumps({
        "prompt": args.prompt, "chat_template": True, "thinking": True, "max_tokens": 64, "stop": [],
        "temperature": 0.0}), {"authorization": f"Bearer {token}", "content-type": "application/json"})
    response = connection.getresponse()
    lines = [json.loads(line) for line in response.read().decode().splitlines() if line.strip()]
    text = "".join(line.get("t", "") for line in lines)
    reasoning = "".join(line.get("r", "") for line in lines)
    print(f"     answer: {text!r}\n     reasoning: {reasoning[:120]!r}\n     {time.time() - started:.1f}s")
    check(not any("error" in line for line in lines), f"no error line: {[l for l in lines if 'error' in l]}")
    check(lines and "done" in lines[-1], "the stream ends with a done line")
    done = lines[-1]["done"]
    check(done["completion_tokens"] > 0 and "timings" in done, f"done: {done}")
    connection = http.client.HTTPConnection("127.0.0.1", upstream["port"], timeout=10)
    connection.request("POST", "/pom/generate", "{}", {"authorization": "Bearer wrong"})
    check(connection.getresponse().status == 401, "a wrong token is refused")

    query("engine.unload")
    while time.time() < deadline and query("engine.status")["state"] != "idle":
        time.sleep(1)
    check(query("engine.status")["state"] == "idle", "engine.unload returns to idle")
    call("shutdown")
    host.wait(timeout=60)
    check(host.returncode == 0, "the plugin host exits cleanly")


if __name__ == "__main__":
    main()
