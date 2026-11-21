import { describe, expect, it } from "vitest";
import { answerPermission, applyUpdate, emptyConversation, pushPermission, pushUser } from "./session";

describe("ACP conversation state", () => {
  it("accumulates agent_message_chunk deltas into one agent message", () => {
    let state = pushUser(emptyConversation(), "list the files");
    state = applyUpdate(state, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Here " } });
    state = applyUpdate(state, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "they are." } });
    expect(state.items).toEqual([
      { kind: "user", text: "list the files" },
      { kind: "agent", text: "Here they are." },
    ]);
  });

  it("upserts tool calls by toolCallId with status transitions", () => {
    let state = applyUpdate(emptyConversation(), {
      sessionUpdate: "tool_call",
      toolCallId: "t1",
      title: "ls",
      status: "pending",
      kind: "execute",
      rawInput: { command: "ls" },
    });
    state = applyUpdate(state, { sessionUpdate: "tool_call_update", toolCallId: "t1", status: "in_progress" });
    state = applyUpdate(state, {
      sessionUpdate: "tool_call_update",
      toolCallId: "t1",
      status: "completed",
      content: [{ type: "content", content: { type: "text", text: "a.txt\nb.txt" } }],
    });
    expect(state.items).toHaveLength(1);
    expect(state.items[0]).toEqual({
      kind: "tool",
      call: { toolCallId: "t1", title: "ls", status: "completed", toolKind: "execute", input: { command: "ls" }, output: "a.txt\nb.txt" },
    });
    // Text after a tool call starts a new agent message.
    state = applyUpdate(state, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Done." } });
    expect(state.items[1]).toEqual({ kind: "agent", text: "Done." });
  });

  it("a permission request waits until answered", () => {
    let state = pushPermission(emptyConversation(), "r1", {
      toolCall: { toolCallId: "t1", title: "rm a.txt" },
      options: [
        { optionId: "allow", name: "Allow once", kind: "allow_once" },
        { optionId: "deny", name: "Reject", kind: "reject_once" },
      ],
    });
    expect(state.items[0]).toMatchObject({ kind: "permission", requestId: "r1", answered: null });
    state = answerPermission(state, "r1", "deny");
    expect(state.items[0]).toMatchObject({ kind: "permission", answered: "deny" });
  });

  it("keeps thoughts apart and ignores updates it does not draw", () => {
    let state = applyUpdate(emptyConversation(), { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "hmm" } });
    state = applyUpdate(state, { sessionUpdate: "usage_update", used: 1, size: 2 } as never);
    expect(state.items).toEqual([{ kind: "thought", text: "hmm" }]);
  });
});
