import { describe, expect, it } from "vitest";
import type { PhaseTrace, RequestTrace } from "./chatStream";
import { analyzeTelemetry, buildTelemetryAnalysisPrompt, serializeTelemetryTables } from "./telemetryAnalysis";

function phase(overrides: Partial<PhaseTrace> = {}): PhaseTrace {
  return {
    processing_ms: 0,
    network_ms: 0,
    bytes_sent: 0,
    bytes_received: 0,
    calls: 0,
    client_encode_ms: 0,
    response_headers_ms: 0,
    response_body_ms: 0,
    client_decode_ms: 0,
    tls_calls: 0,
    tls_handshake_ms: 0,
    node_frame_decode_ms: 0,
    node_input_prepare_ms: 0,
    node_queue_ms: 0,
    node_lock_wait_ms: 0,
    node_setup_ms: 0,
    node_decode_ms: 0,
    node_backend_enqueue_ms: 0,
    node_backend_completion_ms: 0,
    node_residual_capture_d2h_sync_ms: 0,
    node_residual_ms: 0,
    node_logits_access_ms: 0,
    node_logits_clone_ms: 0,
    node_repetition_penalty_ms: 0,
    node_softmax_ms: 0,
    node_top_k_ms: 0,
    node_top_p_ms: 0,
    node_selection_ms: 0,
    node_token_text_ms: 0,
    node_sampling_ms: 0,
    node_encode_ms: 0,
    node_p50_ms: 0,
    node_p95_ms: 0,
    ...overrides,
  };
}

const trace = (decode: PhaseTrace): RequestTrace => ({
  model: "qwen",
  input_tokens: 2,
  output_tokens: 2,
  prefill_ms: 10,
  total_bytes_sent: 10,
  total_bytes_received: 20,
  hops: [{
    node_id: "node-a",
    start_layer: 0,
    end_layer: 31,
    prefill: phase({ calls: 1 }),
    decode,
  }],
});

describe("telemetryAnalysis", () => {
  it("identifies TP wait as the dominant bottleneck", () => {
    const result = analyzeTelemetry(trace(phase({
      processing_ms: 1_000,
      node_decode_ms: 1_000,
      tp: {
        enabled: true,
        rank: 0,
        size: 2,
        expected_reduces: 64,
        reduces: 64,
        wait_ms: 900,
        reduce_p50_ms: 14,
        reduce_p95_ms: 20,
        request_bytes: 128,
        response_bytes: 256,
        direct_calls: 64,
        tunnel_calls: 0,
        fallback_calls: 0,
        broadcast_calls: 1,
        broadcast_bytes: 128,
      },
    })));

    expect(result.kind).toBe("tp-network");
    expect(result.evidence).toContain("900 ms");
  });

  it("serializes both phase and TP tables", () => {
    const prompt = serializeTelemetryTables(trace(phase({
      tp: {
        enabled: true,
        rank: 0,
        size: 2,
        expected_reduces: 64,
        reduces: 64,
        wait_ms: 900,
        reduce_p50_ms: 14,
        reduce_p95_ms: 20,
        request_bytes: 128,
        response_bytes: 256,
        direct_calls: 64,
        tunnel_calls: 0,
        fallback_calls: 0,
        broadcast_calls: 1,
        broadcast_bytes: 128,
      },
    })));

    expect(prompt).toContain("Tabela 1: fases e hops");
    expect(prompt).toContain("Tabela 2: coletivas TP");
    expect(prompt).toContain('"reduces": 64');
  });

  it("builds an AI prompt that includes both telemetry tables", () => {
    const prompt = buildTelemetryAnalysisPrompt(trace(phase()), true);

    expect(prompt).toContain("Tabela 1: fases e hops");
    expect(prompt).toContain("Tabela 2: coletivas TP");
    expect(prompt).toContain("Analise esta execução distribuída em português");
  });

  it("identifies pipeline transport when TP is not dominant", () => {
    const result = analyzeTelemetry(trace(phase({
      processing_ms: 1_000,
      network_ms: 800,
      node_decode_ms: 100,
      node_residual_ms: 50,
    })));

    expect(result.kind).toBe("pipeline-network");
    expect(result.tpObserved).toBe(false);
  });

  it("identifies engine queue wait", () => {
    const result = analyzeTelemetry(trace(phase({
      processing_ms: 1_000,
      node_decode_ms: 300,
      node_lock_wait_ms: 700,
    })));

    expect(result.kind).toBe("queue");
    expect(result.evidence).toContain("700 ms");
  });

  it("does not blame the backend when total time is outside node measurements", () => {
    const result = analyzeTelemetry(
      trace(phase({ processing_ms: 593, node_decode_ms: 588 })),
      {
        ttft_ms: 6_040,
        prefill_ms: 153,
        prefill_tokens: 28,
        prefill_tokens_per_sec: 183,
        generation_ms: 24_960,
        generation_tokens: 56,
        generation_tokens_per_sec: 2.2,
        total_ms: 31_000,
        total_tokens_per_sec: 2.2,
      },
    );

    expect(result.kind).toBe("outside-node");
    expect(result.evidence).toContain("outside 30407 ms");
  });

  it("does not invent TP data when the trace has no TP phase", () => {
    const result = analyzeTelemetry(trace(phase({ processing_ms: 100, node_decode_ms: 20 })));

    expect(result.tpObserved).toBe(false);
    expect(result.evidence).toContain("not recorded");
  });
});
