import { describe, expect, it } from "vitest";
import { consumeChatDelta, emptyChatStream, finishChatStream } from "./chatStream";

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
