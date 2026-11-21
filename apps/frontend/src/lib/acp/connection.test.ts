import { AgentSideConnection, type AnyMessage, type Stream } from "@agentclientprotocol/sdk";
import { describe, expect, it, vi } from "vitest";
import { connectAcp, wsUrl } from "./connection";

/** Two in-memory streams joined back to back: what one side writes, the other reads. */
function pair(): [Stream, Stream] {
  const up = new TransformStream<AnyMessage, AnyMessage>();
  const down = new TransformStream<AnyMessage, AnyMessage>();
  return [
    { writable: up.writable, readable: down.readable },
    { writable: down.writable, readable: up.readable },
  ];
}

function fakeAgent(options: { load?: boolean; list?: boolean; model?: boolean } = {}) {
  const [client, agent] = pair();
  const seen: { prompt?: unknown; config?: unknown; cwd?: string } = {};
  new AgentSideConnection(
    (conn) => ({
      initialize: async () => ({
        protocolVersion: 1,
        agentCapabilities: { loadSession: options.load ?? false, sessionCapabilities: options.list ? { list: {} } : {} },
      }),
      authenticate: async () => ({}),
      newSession: async (params) => {
        seen.cwd = params.cwd;
        return {
          sessionId: "s1",
          configOptions: options.model
            ? [{ id: "model", name: "Model", category: "model", type: "select", currentValue: "a", options: [{ value: "a", name: "A" }, { group: "g", name: "G", options: [{ value: "b", name: "B" }] }] }]
            : [],
        };
      },
      listSessions: async () => ({ sessions: [{ sessionId: "s0", cwd: "/w", title: "Earlier" }] }),
      setSessionConfigOption: async (params) => {
        seen.config = params;
        return { configOptions: [] };
      },
      prompt: async (params) => {
        seen.prompt = params.prompt;
        await conn.sessionUpdate({ sessionId: "s1", update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hi" } } });
        const answer = await conn.requestPermission({
          sessionId: "s1",
          toolCall: { toolCallId: "t1", title: "rm" },
          options: [{ optionId: "ok", name: "Allow", kind: "allow_once" }],
        });
        return { stopReason: answer.outcome.outcome === "selected" ? "end_turn" : "cancelled" };
      },
      cancel: async () => {},
    }),
    agent,
  );
  return { client, seen };
}

describe("ACP connection", () => {
  it("builds the WebSocket URL on the page's own origin", () => {
    expect(wsUrl("/api/ui/plugins/goose/proxy/acp", { protocol: "https:", host: "pom.local:8080" })).toBe(
      "wss://pom.local:8080/api/ui/plugins/goose/proxy/acp",
    );
    expect(wsUrl("/x", { protocol: "http:", host: "127.0.0.1:8080" })).toBe("ws://127.0.0.1:8080/x");
  });

  it("opens a session in the workspace, streams updates and answers a permission", async () => {
    const { client, seen } = fakeAgent({ model: true });
    const updates: unknown[] = [];
    const onPermission = vi.fn(async () => "ok");
    const conn = await connectAcp(client, { onUpdate: (n) => updates.push(n.update), onPermission });
    const session = await conn.newSession("/data/workspace");
    expect(seen.cwd).toBe("/data/workspace");
    expect(session.sessionId).toBe("s1");
    expect(session.model).toEqual({ configId: "model", current: "a", options: [{ value: "a", name: "A" }, { value: "b", name: "B" }] });
    const stop = await conn.prompt("s1", "hello");
    expect(stop).toBe("end_turn");
    expect(seen.prompt).toEqual([{ type: "text", text: "hello" }]);
    expect(updates).toEqual([{ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hi" } }]);
    expect(onPermission).toHaveBeenCalledWith(expect.any(String), expect.objectContaining({ toolCall: { toolCallId: "t1", title: "rm" } }));
    await conn.setModel("s1", "model", "b");
    expect(seen.config).toEqual({ sessionId: "s1", configId: "model", value: "b" });
  });

  it("a declined permission is a cancelled outcome", async () => {
    const { client } = fakeAgent();
    const conn = await connectAcp(client, { onUpdate: () => {}, onPermission: async () => null });
    await conn.newSession("/w");
    expect(await conn.prompt("s1", "x")).toBe("cancelled");
  });

  it("reports the session capabilities and lists sessions only when offered", async () => {
    const plain = await connectAcp(fakeAgent().client, { onUpdate: () => {}, onPermission: async () => null });
    expect(plain.canLoad).toBe(false);
    expect(plain.canList).toBe(false);
    expect(await plain.listSessions("/w")).toEqual([]);
    const rich = await connectAcp(fakeAgent({ load: true, list: true }).client, { onUpdate: () => {}, onPermission: async () => null });
    expect(rich.canLoad).toBe(true);
    expect(await rich.listSessions("/w")).toEqual([{ sessionId: "s0", title: "Earlier", updatedAt: null }]);
  });
});
