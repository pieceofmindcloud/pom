// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { createElement } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import {
  ChatSessionProvider,
  emptySession,
  turnsOnly,
  type ChatMessage,
  useChatSessionController,
} from "./chatStore";

function SessionProbe() {
  const { session, selectSession, startNew } = useChatSessionController();
  return createElement(
    "div",
    null,
    createElement("output", { "data-testid": "session-title" }, session.title),
    createElement("button", { onClick: () => selectSession({ ...emptySession("qwen"), title: "Selecionada" }) }, "select"),
    createElement("button", { onClick: () => startNew("qwen") }, "new"),
  );
}

describe("chatStore conversation metadata", () => {
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
  });

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

  it("shares active conversation actions between the chat and its menu", () => {
    render(createElement(
      LocaleProvider,
      { initialLocale: "pt-BR" },
      createElement(ChatSessionProvider, null, createElement(SessionProbe)),
    ));

    expect(screen.getByTestId("session-title").textContent).toBe("Conversa nova");
    fireEvent.click(screen.getByRole("button", { name: "select" }));
    expect(screen.getByTestId("session-title").textContent).toBe("Selecionada");
    fireEvent.click(screen.getByRole("button", { name: "new" }));
    expect(screen.getByTestId("session-title").textContent).toBe("Conversa nova");
  });
});
