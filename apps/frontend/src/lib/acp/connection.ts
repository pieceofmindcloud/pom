/**
 * The Chat's ACP client: the generic Agent Client Protocol SDK
 * (`@agentclientprotocol/sdk`) over a WebSocket on the node's own origin,
 * through the plugin proxy (`/api/ui/plugins/<code>/proxy<acp.path>`). The
 * session cookie authenticates it; no token travels in the URL. Any agent a
 * plugin announces speaks through this one client.
 */
import {
  ClientSideConnection,
  PROTOCOL_VERSION,
  type RequestPermissionRequest,
  type SessionConfigOption,
  type SessionNotification,
  type Stream,
} from "@agentclientprotocol/sdk";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";

export interface AcpHandlers {
  onUpdate(notification: SessionNotification): void;
  /** Resolves with the chosen `optionId`, or `null` to decline. */
  onPermission(requestId: string, request: RequestPermissionRequest): Promise<string | null>;
}

/** The agent's model choice, when it offers one (`configOptions`, category `model`). */
export interface ModelChoice {
  configId: string;
  current: string;
  options: { value: string; name: string }[];
}

export interface AcpSessionInfo {
  sessionId: string;
  title: string | null;
  updatedAt: string | null;
}

export interface AcpConnection {
  canLoad: boolean;
  canList: boolean;
  newSession(cwd: string): Promise<{ sessionId: string; model: ModelChoice | null }>;
  loadSession(sessionId: string, cwd: string): Promise<{ model: ModelChoice | null }>;
  listSessions(cwd: string): Promise<AcpSessionInfo[]>;
  prompt(sessionId: string, text: string): Promise<string>;
  setModel(sessionId: string, configId: string, value: string): Promise<void>;
  cancel(sessionId: string): Promise<void>;
  /** Settles when the connection ends (the agent stopped, the socket closed). */
  closed: Promise<void>;
}

export function wsUrl(proxyPath: string, location: { protocol: string; host: string }): string {
  return `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}${proxyPath}`;
}

export function modelChoice(options: SessionConfigOption[] | null | undefined): ModelChoice | null {
  const option = (options ?? []).find((entry) => entry.category === "model" && entry.type === "select") as
    | (SessionConfigOption & { type: "select"; currentValue: string; options: unknown[] })
    | undefined;
  if (!option) return null;
  const flat = option.options.flatMap((entry) => {
    const item = entry as { value?: string; name: string; options?: { value: string; name: string }[] };
    return item.options ? item.options.map(({ value, name }) => ({ value, name })) : [{ value: item.value as string, name: item.name }];
  });
  return { configId: option.id, current: option.currentValue, options: flat };
}

let permissionSeq = 0;

export async function connectAcp(stream: Stream, handlers: AcpHandlers): Promise<AcpConnection> {
  const connection = new ClientSideConnection(
    () => ({
      sessionUpdate: async (notification) => handlers.onUpdate(notification),
      requestPermission: async (request) => {
        const optionId = await handlers.onPermission(`perm-${++permissionSeq}`, request);
        return { outcome: optionId === null ? { outcome: "cancelled" } : { outcome: "selected", optionId } };
      },
    }),
    stream,
  );
  const init = await connection.initialize({
    protocolVersion: PROTOCOL_VERSION,
    clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
    clientInfo: { name: "pom-chat", version: "1" },
  });
  const capabilities = init.agentCapabilities ?? {};
  const canList = Boolean(capabilities.sessionCapabilities?.list);
  return {
    canLoad: Boolean(capabilities.loadSession),
    canList,
    async newSession(cwd) {
      const response = await connection.newSession({ cwd, mcpServers: [] });
      return { sessionId: response.sessionId, model: modelChoice(response.configOptions) };
    },
    async loadSession(sessionId, cwd) {
      const response = await connection.loadSession({ sessionId, cwd, mcpServers: [] });
      return { model: modelChoice(response?.configOptions) };
    },
    async listSessions(cwd) {
      if (!canList) return [];
      const response = await connection.listSessions({ cwd });
      return response.sessions.map(({ sessionId, title, updatedAt }) => ({ sessionId, title: title ?? null, updatedAt: updatedAt ?? null }));
    },
    async prompt(sessionId, text) {
      const response = await connection.prompt({ sessionId, prompt: [{ type: "text", text }] });
      return response.stopReason;
    },
    async setModel(sessionId, configId, value) {
      await connection.setSessionConfigOption({ sessionId, configId, value });
    },
    async cancel(sessionId) {
      await connection.cancel({ sessionId });
    },
    closed: connection.closed,
  };
}

/** Opens the agent at `proxyPath` on this page's origin. */
export function openAcp(proxyPath: string, handlers: AcpHandlers): Promise<AcpConnection> {
  return connectAcp(createWebSocketStream(wsUrl(proxyPath, window.location), { cookies: "include" }), handlers);
}
