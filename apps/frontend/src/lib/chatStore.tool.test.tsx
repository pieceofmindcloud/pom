// @vitest-environment jsdom
/**
 * Generate image in Automatic mode: the Chat offers the generate_image tool,
 * reassembles the call the model streams in fragments, and runs it on the
 * same reply before the conversation is saved.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useEffect, useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ChatSessionProvider, emptySession, useChatSessionController, type ChatMessage } from "./chatStore";
import { generateImageTool, imageCallOf, type ToolCall } from "./chatImageTool";

const sent = vi.hoisted(() => ({ bodies: [] as Array<Record<string, unknown>> }));

vi.mock("./api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./api")>();
  return {
    ...actual,
    streamSse: async function* (_path: string, body: Record<string, unknown>) {
      sent.bodies.push(body);
      yield JSON.stringify({ choices: [{ delta: { content: "Here it is." } }] });
      yield JSON.stringify({ choices: [{ delta: { tool_calls: [{ index: 0, function: { name: "generate_image", arguments: "{\"prompt\":\"a fox" } }] } }] });
      yield JSON.stringify({ choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: " in snow\",\"aspect\":\"wide\"}" } }] } }] });
      yield "[DONE]";
    },
  };
});

function Probe({ offerTool, onCalls }: { offerTool: boolean; onCalls: (calls: ToolCall[]) => void }) {
  const { session, setSession, startStream } = useChatSessionController();
  const user: ChatMessage = { id: "u", role: "user", content: "draw a fox", at: "" };
  const reply: ChatMessage = { id: "r", role: "assistant", content: "", at: "" };
  const [initial] = useState(() => ({ ...emptySession("qwen"), messages: [user, reply] }));
  useEffect(() => setSession(initial), []); // eslint-disable-line react-hooks/exhaustive-deps
  const last = session.messages.at(-1);
  return (
    <>
      <button
        type="button"
        onClick={() => startStream({
          session: initial,
          history: [user],
          replyId: "r",
          model: "qwen",
          thinkingEnabled: false,
          emptyResponseMessage: "empty",
          interruptedMessage: "interrupted",
          cluster: null,
          ...(offerTool ? {
            tools: [generateImageTool],
            onToolCalls: async (calls: ToolCall[], patch: (change: Partial<ChatMessage>) => void) => {
              onCalls(calls);
              patch({ imagePrompt: imageCallOf(calls)?.prompt, images: [{ hash: "a".repeat(64), ext: "png", width: 8, height: 8 }] });
            },
          } : {}),
        })}
      >
        start
      </button>
      <output aria-label="reply">{JSON.stringify({ content: last?.content, images: last?.images?.length ?? 0, prompt: last?.imagePrompt, error: last?.error })}</output>
    </>
  );
}

afterEach(() => {
  cleanup();
  sent.bodies.length = 0;
});

describe("generate_image in the Chat stream", () => {
  it("offers the tool and runs a call streamed in fragments on the same reply", async () => {
    const onCalls = vi.fn();
    render(<ChatSessionProvider><Probe offerTool onCalls={onCalls} /></ChatSessionProvider>);
    fireEvent.click(screen.getByText("start"));
    await waitFor(() => expect(onCalls).toHaveBeenCalled());
    await waitFor(() => expect(screen.getByLabelText("reply").textContent).toContain("\"images\":1"));
    expect((sent.bodies[0].tools as unknown[])[0]).toEqual(generateImageTool);
    expect(imageCallOf(onCalls.mock.calls[0][0])).toEqual({ prompt: "a fox in snow", negative: undefined, aspect: "wide", useAttachedImage: false });
    const reply = JSON.parse(screen.getByLabelText("reply").textContent!);
    expect(reply).toMatchObject({ content: "Here it is.", prompt: "a fox in snow" });
    expect(reply.error).toBeUndefined();
  });

  it("sends no tool when none is offered (Text only)", async () => {
    render(<ChatSessionProvider><Probe offerTool={false} onCalls={vi.fn()} /></ChatSessionProvider>);
    fireEvent.click(screen.getByText("start"));
    await waitFor(() => expect(sent.bodies).toHaveLength(1));
    expect(sent.bodies[0].tools).toBeUndefined();
  });
});
