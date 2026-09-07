#!/usr/bin/env python3
import json
import unittest

from scripts.bench_llamacpp import (
    ConfigError,
    RemoteResult,
    _cleanup_server,
    _prebuilt_cache_is_compatible,
    _resolve_server,
    _start_server,
    build_bench_plan,
    incomplete_prompts,
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

    def test_serving_is_preferred_over_non_serving_default(self):
        self.snapshot["/api/ui/deployments"]["deployments"][0]["is_default"] = False
        self.snapshot["/api/ui/deployments"]["deployments"][1]["serving"] = False
        self.snapshot["/api/ui/deployments"]["deployments"][1]["is_default"] = True
        self.assertEqual(select_deployment(self.snapshot)["deploy_id"], "d-1")

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

    def test_context_total_preserves_n_ctx_per_session(self):
        self.snapshot["/api/ui/deployments"]["deployments"][0]["deployment"]["nodes"][0]["config"]["max_sessions"] = 2
        plan = build_bench_plan(select_deployment(self.snapshot), n_ctx_override=4096)
        self.assertEqual(plan["llama_flags"]["ctx_size"], 8192)
        self.assertEqual(plan["llama_flags"]["ctx_per_session"], 4096)

    def test_plan_maps_pom_mtp_to_llama_server_draft_mtp_flags(self):
        config = self.snapshot["/api/ui/deployments"]["deployments"][0]["deployment"]["nodes"][0]["config"]
        config.update(
            {
                "mtp_enabled": True,
                "mtp_draft_tokens": 3,
                "mtp_min_probability": 0.75,
                "mtp_cpu_fallback_enabled": True,
                "speculative_decoding_enabled": True,
                "speculative_k": 4,
                "speculative_lookup_window": 3,
            }
        )
        plan = build_bench_plan(select_deployment(self.snapshot))
        self.assertEqual(plan["pom_config"]["mtp_draft_tokens"], 3)
        self.assertEqual(plan["llama_flags"]["spec_type"], "draft-mtp")
        self.assertEqual(plan["llama_flags"]["spec_draft_n_max"], 3)
        self.assertEqual(plan["llama_flags"]["spec_draft_p_min"], 0.75)
        self.assertTrue(plan["llama_flags"]["mtp_cpu_fallback"])
        self.assertEqual(plan["llama_flags"]["speculation_source"], "mtp")

    def test_server_command_enables_mtp_with_same_draft_limits(self):
        config = self.snapshot["/api/ui/deployments"]["deployments"][0]["deployment"]["nodes"][0]["config"]
        config.update(
            {
                "mtp_enabled": True,
                "mtp_draft_tokens": 3,
                "mtp_min_probability": 0.75,
                "speculative_decoding_enabled": True,
            }
        )

        class StubRemote:
            def __init__(self):
                self.commands = []

            def run(self, command, timeout=None):
                self.commands.append(command)
                return RemoteResult(0, "4242\n", "")

        remote = StubRemote()
        _start_server(
            remote,
            "/tmp/llama-server",
            "/data/model.gguf",
            build_bench_plan(select_deployment(self.snapshot)),
            8090,
            "/tmp/bench.log",
        )
        command = remote.commands[0]
        self.assertIn("--spec-type draft-mtp", command)
        self.assertIn("--spec-draft-n-max 3", command)
        self.assertIn("--spec-draft-p-min 0.75", command)

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
        self.assertNotIn(" -kvo", command)
        self.assertNotIn("8080", command)

    def test_explicit_deploy_id_does_not_depend_on_active_model(self):
        self.snapshot["/api/ui/node"]["active_model"] = "outro.gguf"
        selected = select_deployment(self.snapshot, deploy_id="d-1")
        self.assertEqual(selected["deploy_id"], "d-1")

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

    def test_parse_stream_result_uses_partial_text_when_final_content_is_empty(self):
        lines = [
            "data: {\"content\":\"Oi\",\"stop\":false}\n",
            "data: {\"content\":\"\",\"stop\":true,\"timings\":{\"prompt_ms\":1,\"prompt_per_second\":1,\"predicted_ms\":1,\"predicted_per_second\":1,\"prompt_n\":1,\"predicted_n\":1}}\n",
        ]
        result = parse_sse_result(lines, first_token_ms=1, total_ms=2, http_status=200)
        self.assertEqual(result["response_text"], "Oi")

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

    def test_incomplete_measurements_are_not_publishable_as_success(self):
        rows = [
            {"phase": "measurement", "prompt_id": "p-curto-repetitivo", "status": "ok"},
            {"phase": "measurement", "prompt_id": "p-curto-repetitivo", "status": "ok"},
        ]
        self.assertEqual(incomplete_prompts(rows, expected_repetitions=3), ["p-curto-repetitivo"])

    def test_prebuilt_cache_must_prove_cuda_server_build(self):
        cache = """\nCMAKE_HOME_DIRECTORY:INTERNAL=/data/fm-prefixo-src/llama.cpp\nGGML_CUDA:BOOL=ON\nLLAMA_BUILD_SERVER:BOOL=ON\n"""
        self.assertTrue(_prebuilt_cache_is_compatible(cache, "/data/fm-prefixo-src/llama.cpp"))
        self.assertFalse(_prebuilt_cache_is_compatible(cache.replace("GGML_CUDA:BOOL=ON", "GGML_CUDA:BOOL=OFF"), "/data/fm-prefixo-src/llama.cpp"))

    def test_cuda_compiler_path_is_passed_to_external_build(self):
        class StubRemote:
            def __init__(self):
                self.commands = []

            def run(self, command, timeout=None):
                self.commands.append(command)
                if len(self.commands) == 1:
                    return RemoteResult(
                        0,
                        "origin git@github.com:pereirajair/llama.cpp.git (fetch)\n"
                        "COMMIT=abc123\nBRANCH=\n",
                        "",
                    )
                if len(self.commands) == 2:
                    return RemoteResult(1, "", "")
                if len(self.commands) == 3:
                    return RemoteResult(0, "/usr/local/cuda-13.0/bin/nvcc\n", "")
                return RemoteResult(0, "", "")

        remote = StubRemote()
        _resolve_server(remote, "/data/fm-prefixo-src/llama.cpp", None, 2, "120")
        self.assertIn(
            "-DCMAKE_CUDA_COMPILER=/usr/local/cuda-13.0/bin/nvcc",
            remote.commands[-1],
        )

    def test_cleanup_reports_remote_failure(self):
        class FailedRemote:
            def run(self, command, timeout=None):
                return RemoteResult(255, "", "ssh failed")

        self.assertFalse(_cleanup_server(FailedRemote(), 4242, "/tmp/bench.log"))


if __name__ == "__main__":
    unittest.main()
