/**
 * What the Chat draws of a conversation with an ACP agent, as a pure state:
 * the person's turns, the agent's text and thoughts, tool calls (upserted by
 * `toolCallId`), plans and permission requests. `applyUpdate` folds one
 * `session/update` notification in; updates the Chat does not draw (usage,
 * commands, modes) leave the state alone.
 */
import type { SessionNotification, RequestPermissionRequest } from "@agentclientprotocol/sdk";

type SessionUpdate = SessionNotification["update"];

export type AcpToolStatus = "pending" | "in_progress" | "completed" | "failed";

export interface AcpToolCall {
  toolCallId: string;
  title: string;
  status: AcpToolStatus;
  toolKind?: string;
  input?: unknown;
  output?: string;
}

export type AcpPermission = Pick<RequestPermissionRequest, "toolCall" | "options">;

export type AcpItem =
  | { kind: "user"; text: string }
  | { kind: "agent"; text: string }
  | { kind: "thought"; text: string }
  | { kind: "tool"; call: AcpToolCall }
  | { kind: "plan"; entries: { content: string; status: string }[] }
  | { kind: "permission"; requestId: string; request: AcpPermission; answered: string | null };

export interface AcpConversation {
  items: AcpItem[];
}

export const emptyConversation = (): AcpConversation => ({ items: [] });

export function pushUser(state: AcpConversation, text: string): AcpConversation {
  return { items: [...state.items, { kind: "user", text }] };
}

export function pushPermission(state: AcpConversation, requestId: string, request: AcpPermission): AcpConversation {
  return { items: [...state.items, { kind: "permission", requestId, request, answered: null }] };
}

export function answerPermission(state: AcpConversation, requestId: string, optionId: string): AcpConversation {
  return {
    items: state.items.map((item) =>
      item.kind === "permission" && item.requestId === requestId ? { ...item, answered: optionId } : item,
    ),
  };
}

/** Text of a content block or of a tool call's content list. */
function textOf(content: unknown): string {
  if (!content || typeof content !== "object") return "";
  if (Array.isArray(content)) return content.map(textOf).filter(Boolean).join("\n");
  const block = content as { type?: string; text?: string; content?: unknown };
  if (block.type === "text" && typeof block.text === "string") return block.text;
  if (block.type === "content") return textOf(block.content);
  return "";
}

function appendText(state: AcpConversation, kind: "agent" | "thought", text: string): AcpConversation {
  if (!text) return state;
  const items = [...state.items];
  const last = items[items.length - 1];
  if (last && last.kind === kind) items[items.length - 1] = { kind, text: last.text + text };
  else items.push({ kind, text });
  return { items };
}

export function applyUpdate(state: AcpConversation, update: SessionUpdate): AcpConversation {
  switch (update.sessionUpdate) {
    case "agent_message_chunk":
      return appendText(state, "agent", textOf(update.content));
    case "agent_thought_chunk":
      return appendText(state, "thought", textOf(update.content));
    case "user_message_chunk": {
      // Replayed history (`session/load`) brings the person's turns back.
      const text = textOf(update.content);
      const last = state.items[state.items.length - 1];
      if (last && last.kind === "user") {
        const items = [...state.items];
        items[items.length - 1] = { kind: "user", text: last.text + text };
        return { items };
      }
      return pushUser(state, text);
    }
    case "tool_call":
    case "tool_call_update": {
      const u = update as {
        toolCallId: string;
        title?: string | null;
        status?: AcpToolStatus | null;
        kind?: string | null;
        rawInput?: unknown;
        content?: unknown;
        rawOutput?: unknown;
      };
      const index = state.items.findIndex((item) => item.kind === "tool" && item.call.toolCallId === u.toolCallId);
      const previous: AcpToolCall =
        index >= 0 ? (state.items[index] as { kind: "tool"; call: AcpToolCall }).call : { toolCallId: u.toolCallId, title: "", status: "pending" };
      const output = textOf(u.content) || (typeof u.rawOutput === "string" ? u.rawOutput : "");
      const call: AcpToolCall = {
        ...previous,
        ...(u.title ? { title: u.title } : {}),
        ...(u.status ? { status: u.status } : {}),
        ...(u.kind ? { toolKind: u.kind } : {}),
        ...(u.rawInput !== undefined && u.rawInput !== null ? { input: u.rawInput } : {}),
        ...(output ? { output } : {}),
      };
      const items = [...state.items];
      if (index >= 0) items[index] = { kind: "tool", call };
      else items.push({ kind: "tool", call });
      return { items };
    }
    case "plan": {
      const entries = ((update as { entries?: { content: string; status: string }[] }).entries ?? []).map(({ content, status }) => ({ content, status }));
      const items = state.items.filter((item) => item.kind !== "plan");
      return { items: [...items, { kind: "plan", entries }] };
    }
    default:
      return state;
  }
}
