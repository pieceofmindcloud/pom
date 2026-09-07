#!/usr/bin/env python3
import json
import unittest

from scripts.bench_llamacpp import (
    ConfigError,
    RemoteResult,
    _start_server,
    build_bench_plan,
    make_completion_payload,
    median,
    parse_sse_result,
    select_deployment,
    summary_rows,
)


def wrapped_deployment(
    deploy_id="d-1",
    label="Qwen3.8-27B-UD-Q5_K_M.gguf",
    kind="whole",
    serving=True,
    is_default=True,
    nodes=None,
):
    if nodes is None:
        nodes = [
            {
                "node_id": "node-1",
                "hostname": "ksia",
                "start_layer": 0,
                "end_layer": 64,
                "config": {
                    "n_ctx": 250000,
                    "n_batch": 512,
                    "n_gpu_layers": 99,
                    "offload_kqv": True,
                    "max_sessions": 1,
                    "hidden_state_dtype": "f16",
                    "inference_mode": "dense",
                    "temperature": 0.7,
                    "top_k": 40,
                    "top_p": 0.9,
                    "repeat_penalty": 1.15,
                },
            }
        ]
    return {
        "is_default": is_default,
        "serving": serving,
        "deployment": {
            "deploy_id": deploy_id,
            "kind": kind,
            "model": {"label": label, "total_layers": 65},
            "nodes": nodes,
        },
    }


class BenchLlamaCppTests(unittest.TestCase):
    def setUp(self):
        self.snapshot = {
            "/api/ui/node": {"active_model": "Qwen3.8-27B-UD-Q5_K_M.gguf"},
            "/api/ui/deployments": {
                "deployments": [
                    wrapped_deployment(),
                    wrapped_deployment(
                        deploy_id="d-2", serving=False, is_default=False
                    ),
                ]
            },
        }

    def test_default_selection_uses_active_serving_deployment(self):
        selected = select_deployment(self.snapshot)
        self.assertEqual(selected["deploy_id"], "d-1")
        self.assertEqual(selected["nodes"][0]["config"]["n_batch"], 512)

    def test_ambiguous_model_requires_deploy_id(self):
        self.snapshot["/api/ui/deployments"]["deployments"][0]["serving"] = False
        self.snapshot["/api/ui/deployments"]["deployments"][0]["is_default"] = False
        with self.assertRaisesRegex(ConfigError, "ambígua"):
            select_deployment(self.snapshot, model="Qwen3.8-27B-UD-Q5_K_M.gguf")

    def test_multi_node_is_rejected_before_launch(self):
        deployment = wrapped_deployment(
            kind="shards",
            nodes=[wrapped_deployment()["deployment"]["nodes"][0], wrapped_deployment()["deployment"]["nodes"][0]],
        )
        with self.assertRaisesRegex(ConfigError, "multi-node"):
            build_bench_plan(deployment)

    def test_context_override_is_recorded_as_effective_value(self):
        plan = build_bench_plan(select_deployment(self.snapshot), n_ctx_override=4096)
        self.assertEqual(plan["pom_config"]["n_ctx"], 250000)
        self.assertEqual(plan["effective_n_ctx"], 4096)
        self.assertEqual(plan["llama_flags"]["ctx_size"], 4096)

    def test_completion_payload_maps_sampling_and_budget(self):
        payload = make_completion_payload(
            "prompt", 8, {"temperature": 0.7, "top_k": 40, "top_p": 0.9, "repeat_penalty": 1.15}
        )
        self.assertEqual(
            payload,
            {
                "prompt": "prompt",
                "n_predict": 8,
                "temperature": 0.7,
                "top_k": 40,
                "top_p": 0.9,
                "repeat_penalty": 1.15,
                "stream": True,
            },
        )

    def test_server_command_maps_effective_flags_without_touching_port_8080(self):
        class StubRemote:
            def __init__(self):
                self.commands = []

            def run(self, command, timeout=None):
                self.commands.append(command)
                return RemoteResult(0, "4242\n", "")

        remote = StubRemote()
        plan = build_bench_plan(select_deployment(self.snapshot), n_ctx_override=4096)
        pid = _start_server(remote, "/tmp/llama-server", "/data/model.gguf", plan, 8090, "/tmp/bench.log")
        self.assertEqual(pid, 4242)
        command = remote.commands[0]
        self.assertIn("--host 127.0.0.1", command)
        self.assertIn("--port 8090", command)
        self.assertIn(" -c 4096", command)
        self.assertIn(" -b 512", command)
        self.assertIn(" -ngl 99", command)
        self.assertIn(" -np 1", command)
        self.assertIn(" -kvo", command)
        self.assertNotIn("8080", command)

    def test_parse_stream_result_extracts_timings_and_first_token(self):
        lines = [
            "data: {\"content\":\"Oi\",\"stop\":false}\n",
            "data: {\"content\":\"!\",\"stop\":true,\"tokens_predicted\":2,\"tokens_evaluated\":5,\"timings\":{\"prompt_ms\":12.5,\"prompt_per_second\":400.0,\"predicted_ms\":25.0,\"predicted_per_second\":80.0,\"prompt_n\":5,\"predicted_n\":2,\"cache_n\":1}}\n",
        ]
        result = parse_sse_result(lines, first_token_ms=7.25, total_ms=41.5, http_status=200)
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["output_tokens"], 2)
        self.assertEqual(result["generation_tok_s"], 80.0)
        self.assertEqual(result["prefill_ms"], 12.5)
        self.assertEqual(result["ttft_ms"], 7.25)
        self.assertEqual(result["total_ms"], 41.5)

    def test_summary_uses_median_and_excludes_warmup(self):
        rows = [
            {"phase": "warmup", "prompt_id": "p-curto-repetitivo", "status": "ok", "generation_tok_s": 999},
            {"phase": "measurement", "prompt_id": "p-curto-repetitivo", "status": "ok", "generation_tok_s": 10, "prefill_ms": 30, "prefill_tok_s": 100, "ttft_ms": 40, "total_ms": 50, "prompt_tokens": 5, "output_tokens": 2, "tokens_cached": 0},
            {"phase": "measurement", "prompt_id": "p-curto-repetitivo", "status": "ok", "generation_tok_s": 30, "prefill_ms": 20, "prefill_tok_s": 200, "ttft_ms": 20, "total_ms": 40, "prompt_tokens": 5, "output_tokens": 2, "tokens_cached": 5},
            {"phase": "measurement", "prompt_id": "p-curto-repetitivo", "status": "ok", "generation_tok_s": 20, "prefill_ms": 10, "prefill_tok_s": 300, "ttft_ms": 30, "total_ms": 30, "prompt_tokens": 5, "output_tokens": 2, "tokens_cached": 5},
        ]
        summary = summary_rows(rows)
        self.assertEqual(len(summary), 5)
        self.assertEqual(summary[0]["generation_tok_s"], 20)
        self.assertEqual(summary[0]["prefill_ms"], 20)
        self.assertEqual(median([10, 30, 20]), 20)


if __name__ == "__main__":
    unittest.main()
