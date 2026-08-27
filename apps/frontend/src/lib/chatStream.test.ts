import { describe, expect, it } from "vitest";
import {
  consumeChatDelta,
  consumeChatError,
  emptyChatStream,
  finishChatStream,
  streamErrorMessage,
} from "./chatStream";

describe("chat stream parsing", () => {
  it("separates a think block split across streaming chunks", () => {
    let state = emptyChatStream();
    state = consumeChatDelta(state, "<think>cal");
    state = consumeChatDelta(state, "culo</think>Resposta");
    state = finishChatStream(state);

    expect(state.reasoning).toBe("calculo");
    expect(state.content).toBe("Resposta");
  });

  it("keeps ordinary text that only resembles a thinking delimiter", () => {
    const state = finishChatStream(consumeChatDelta(emptyChatStream(), "Use <thing> como exemplo."));

    expect(state.content).toBe("Use <thing> como exemplo.");
    expect(state.reasoning).toBe("");
  });

  it("reads the real message from an OpenAI error frame in the stream", () => {
    expect(
      streamErrorMessage({
        error: { message: "pipeline indisponível: camada 17 não respondeu", type: "server_error" },
      }),
    ).toBe("pipeline indisponível: camada 17 não respondeu");
  });

  it("keeps rendered text when a later stream frame reports an error", () => {
    let state = consumeChatDelta(emptyChatStream(), "Parte já renderizada");
    state = consumeChatError(state, "o peer caiu durante a geração");

    expect(state.content).toBe("Parte já renderizada");
    expect(state.error).toBe("o peer caiu durante a geração");
  });

  it("stores metrics emitted on the final SSE chunk", () => {
    const state = finishChatStream(emptyChatStream(), {
      tokens_per_sec: 18.4,
      tokens_generated: 312,
      duration_ms: 16956,
    });

    expect(state.tokensPerSecond).toBe(18.4);
    expect(state.tokensGenerated).toBe(312);
    expect(state.durationMs).toBe(16956);
  });

  it("stores split request timings without treating total throughput as generation speed", () => {
    const state = finishChatStream(emptyChatStream(), {
      tokens_per_sec: 2,
      tokens_generated: 20,
      duration_ms: 10000,
      timings: {
        ttft_ms: 2400,
        prefill_ms: 2000,
        prefill_tokens: 100,
        prefill_tokens_per_sec: 50,
        generation_ms: 7600,
        generation_tokens: 20,
        generation_tokens_per_sec: 20 / 7.6,
        total_ms: 10000,
        total_tokens_per_sec: 2,
      },
    });

    expect(state.timings?.ttft_ms).toBe(2400);
    expect(state.timings?.prefill_tokens_per_sec).toBe(50);
    expect(state.timings?.generation_tokens_per_sec).toBe(20 / 7.6);
    expect(state.timings?.total_tokens_per_sec).toBe(2);
  });

  it("keeps unavailable rates unavailable for zero prefill and one output token", () => {
    const state = finishChatStream(emptyChatStream(), {
      timings: {
        ttft_ms: 1200,
        prefill_ms: 800,
        prefill_tokens: 0,
        prefill_tokens_per_sec: null,
        generation_ms: 0,
        generation_tokens: 1,
        generation_tokens_per_sec: null,
        total_ms: 1200,
        total_tokens_per_sec: 1 / 1.2,
      },
    });

    expect(state.timings?.prefill_tokens_per_sec).toBeNull();
    expect(state.timings?.generation_tokens_per_sec).toBeNull();
  });

  it("keeps the final distributed trace for the debug view", () => {
    const state = finishChatStream(emptyChatStream(), {
      trace: { model: "qwen", input_tokens: 12, output_tokens: 4, prefill_ms: 90, total_bytes_sent: 10, total_bytes_received: 20, hops: [] },
    });

    expect(state.trace?.model).toBe("qwen");
    expect(state.trace?.input_tokens).toBe(12);
  });

  it("keeps the reused prefix count, which is what the chat shows as savings", () => {
    const state = finishChatStream(emptyChatStream(), {
      trace: { model: "qwen", input_tokens: 40, cached_tokens: 33, output_tokens: 4, prefill_ms: 90, total_bytes_sent: 10, total_bytes_received: 20, hops: [] },
    });

    expect(state.trace?.cached_tokens).toBe(33);
  });

  it("treats a node that does not report reuse as zero, never as savings", () => {
    const state = finishChatStream(emptyChatStream(), {
      trace: { model: "qwen", input_tokens: 40, output_tokens: 4, prefill_ms: 90, total_bytes_sent: 10, total_bytes_received: 20, hops: [] },
    });

    expect(state.trace?.cached_tokens).toBeUndefined();
  });
});
