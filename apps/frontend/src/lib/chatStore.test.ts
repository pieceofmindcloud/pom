// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { emptySession, turnsOnly, type ChatMessage } from "./chatStore";

describe("chatStore conversation metadata", () => {
  it("records the selected model at the start of a new conversation", () => {
    const session = emptySession("qwen");

    expect(session.modelAtStart).toBe("qwen");
    expect(turnsOnly(session.messages)).toEqual([]);
  });

  it("uses private request content for telemetry analysis", () => {
    const messages = [{
      id: "1",
      role: "user",
      content: "Analise a execução",
      requestContent: "Tabela 1... Tabela 2...",
      at: new Date().toISOString(),
    }] as ChatMessage[];

    expect(turnsOnly(messages)[0].content).toBe("Tabela 1... Tabela 2...");
  });
});
