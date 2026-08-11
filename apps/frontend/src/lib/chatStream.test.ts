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
});
