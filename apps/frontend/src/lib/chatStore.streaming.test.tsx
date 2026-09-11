// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useEffect } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ChatSessionProvider,
  emptySession,
  useChatSessionController,
  type ChatMessage,
  type ChatSession,
} from "./chatStore";

const streamScenario = vi.hoisted(() => ({ chunks: 2000, delayMs: 0 }));

vi.mock("./api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./api")>();
  return {
    ...actual,
    // eslint-disable-next-line require-yield
    streamSse: async function* () {
      for (let index = 0; index < streamScenario.chunks; index += 1) {
        if (streamScenario.delayMs > 0) {
          await new Promise((resolve) => setTimeout(resolve, streamScenario.delayMs));
        }
        yield JSON.stringify({ choices: [{ delta: { content: "x" } }] });
      }
      yield "[DONE]";
    },
  };
});

function streamSession(): { session: ChatSession; replyId: string } {
  const session = emptySession("qwen");
  const user: ChatMessage = {
    id: "user",
    role: "user",
    content: "gere",
    at: new Date().toISOString(),
  };
  const reply: ChatMessage = {
    id: "reply",
    role: "assistant",
    content: "",
    at: new Date().toISOString(),
  };
  return { session: { ...session, messages: [user, reply] }, replyId: reply.id };
}

function StreamProbe({
  initial,
  onRender,
}: {
  initial: { session: ChatSession; replyId: string };
  onRender: (content: string) => void;
}) {
  const { session, setSession, startStream } = useChatSessionController();
  useEffect(() => {
    setSession(initial.session);
  }, [initial.session, setSession]);
  useEffect(() => {
    onRender(session.messages.at(-1)?.content ?? "");
  }, [onRender, session]);
  return (
    <>
      <button
        type="button"
        onClick={() => startStream({
          session: initial.session,
          history: initial.session.messages.slice(0, -1),
          replyId: initial.replyId,
          model: "qwen",
          thinkingEnabled: false,
          emptyResponseMessage: "empty",
          interruptedMessage: "interrupted",
          cluster: null,
        })}
      >
        start
      </button>
      <output aria-label="answer">{session.messages.at(-1)?.content ?? ""}</output>
    </>
  );
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  streamScenario.chunks = 2000;
  streamScenario.delayMs = 0;
  vi.restoreAllMocks();
});

describe("chat stream delivery", () => {
  it("batches rapid persistence while preserving every generated character", async () => {
    const initial = streamSession();
    const renders: string[] = [];
    const setItem = vi.spyOn(Storage.prototype, "setItem");

    render(
      <ChatSessionProvider>
        <StreamProbe initial={initial} onRender={(content) => renders.push(content)} />
      </ChatSessionProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "start" }));

    await waitFor(() => expect(screen.getByRole("status", { name: "answer" }).textContent).toHaveLength(streamScenario.chunks));

    expect(screen.getByRole("status", { name: "answer" }).textContent).toBe("x".repeat(streamScenario.chunks));
    expect(renders.filter((content) => content.length > 0).length).toBeLessThan(100);
    expect(setItem.mock.calls.length).toBeLessThan(100);
  });

  it("publishes visible progress during a sustained stream instead of waiting for completion", async () => {
    streamScenario.chunks = 40;
    streamScenario.delayMs = 10;
    const initial = streamSession();
    const renders: string[] = [];

    render(
      <ChatSessionProvider>
        <StreamProbe initial={initial} onRender={(content) => renders.push(content)} />
      </ChatSessionProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "start" }));

    await waitFor(
      () => expect(screen.getByRole("status", { name: "answer" }).textContent).toHaveLength(streamScenario.chunks),
      { timeout: 1500 },
    );

    const visibleUpdates = renders.filter((content) => content.length > 0);
    expect(visibleUpdates.length).toBeGreaterThan(1);
    expect(visibleUpdates.length).toBeLessThan(streamScenario.chunks);
    expect(visibleUpdates.at(-1)).toBe("x".repeat(streamScenario.chunks));
  });
});
