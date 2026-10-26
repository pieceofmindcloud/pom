/**
 * Event protocol between POM and the plugins (`pom-plugin-events/v1`).
 *
 * A single JSON envelope travels through every channel, so that any plugin
 * screen - a React component mounted by the host, a script without the SDK or an
 * embedded third-party application (such as the harness's) - receives the same
 * events:
 *
 * - SDK: `__POM_HOST__.events.subscribe(type, handler)`;
 * - DOM: `CustomEvent("pom:plugin-event")` dispatched on the `window`;
 * - iframes: `postMessage(envelope)` to the same-origin `iframe[data-pom-plugin]`;
 * - native backend: the `query`'s `host.event` operation (Rust side, `plugins.rs`).
 *
 * In the opposite direction, the plugin asks POM for actions with commands
 * (`notification.notify`, `notification.confirm`) through the SDK or through
 * `CustomEvent("pom:plugin-request")`/`postMessage`. The result of every command
 * comes back as the `notification.response` event, with the request's `request_id`.
 * See `docs/subsystems/plugins-ui.md`, section "Events and notifications".
 */
import { randomId as newId } from "./id";
import { api } from "./api";

import { applyLoaderCommand, MAX_BODY, MAX_TITLE, normalizeLoaderCommand, type LoaderCommandType } from "./pluginLoader";

type LoaderText = { title?: string; message?: string };

export const PLUGIN_EVENTS_PROTOCOL = "pom-plugin-events/v1";
export const PLUGIN_EVENT_DOM = "pom:plugin-event";
export const PLUGIN_REQUEST_DOM = "pom:plugin-request";

export type PluginEventType =
  | "locale.changed"
  | "theme.changed"
  | "preferences.changed"
  | "notification.received"
  | "notification.response"
  | "deployment.started"
  | "model.serving"
  | "model.stopped"
  /** The person clicked "Try again" on the plugin's loader error (`lib/pluginLoader.ts`). */
  | "loader.retry";

/** Types the node publishes in `GET /api/ui/plugins/events` and the bridge relays. */
const NODE_EVENT_TYPES: readonly PluginEventType[] = [
  "preferences.changed",
  "deployment.started",
  "model.serving",
  "model.stopped",
];

export interface PluginEvent<P = Record<string, unknown>> {
  protocol: typeof PLUGIN_EVENTS_PROTOCOL;
  id: string;
  type: PluginEventType;
  /** Destination `plugin_code`, or `*` for all plugins. */
  target: string;
  source: "pom";
  at: string;
  payload: P;
}

export type NotificationLevel = "info" | "success" | "warning" | "error";

export interface NotifyRequest {
  title: string;
  body?: string;
  level?: NotificationLevel;
  /** `local` notifies only whoever is on this interface; `network`, all nodes. */
  scope?: "local" | "network";
}

export interface ConfirmRequest {
  title: string;
  body?: string;
  acceptLabel?: string;
  cancelLabel?: string;
  tone?: "primary" | "danger";
  /**
   * `notification` (default): goes into the top bar's bell with Accept and
   * Refuse, like a transfer offer. `dialog`: opens a modal dialog
   * immediately.
   */
  display?: "notification" | "dialog";
}

export type NotificationAction = "delivered" | "failed" | "accepted" | "cancelled";

export interface NotificationResponse {
  request_id: string;
  kind: "notify" | "confirm" | "loader";
  action: NotificationAction;
  error?: string;
}

export interface PluginCommand {
  protocol: typeof PLUGIN_EVENTS_PROTOCOL;
  id: string;
  type: "notification.notify" | "notification.confirm" | LoaderCommandType;
  plugin_code: string;
  payload: NotifyRequest | ConfirmRequest | Record<string, unknown>;
}

type Handler = (event: PluginEvent) => void;

/** Who executes the commands: installed by the `PluginBridge` mounted in the App. */
export interface PluginCommandExecutor {
  isPlugin: (code: string) => boolean;
  notify: (code: string, request: NotifyRequest) => Promise<void>;
  confirm: (code: string, request: ConfirmRequest) => Promise<"accepted" | "cancelled">;
}

const handlers = new Map<string, Set<{ handler: Handler; plugin?: string }>>();
const lastByType = new Map<PluginEventType, PluginEvent>();
let executor: PluginCommandExecutor | null = null;

const LEVELS: readonly NotificationLevel[] = ["info", "success", "warning", "error"];

function envelope<P>(type: PluginEventType, payload: P, target: string): PluginEvent<P> {
  return {
    protocol: PLUGIN_EVENTS_PROTOCOL,
    id: newId(),
    type,
    target,
    source: "pom",
    at: new Date().toISOString(),
    payload,
  };
}

function reaches(event: PluginEvent, plugin?: string) {
  return event.target === "*" || plugin === undefined || plugin === event.target;
}

/** Publishes an event on all the browser channels. */
export function publishPluginEvent<P extends Record<string, unknown>>(
  type: PluginEventType,
  payload: P,
  target = "*",
): PluginEvent<P> {
  return deliver(envelope(type, payload, target));
}

/**
 * Relays to the screens an envelope the node has already built, preserving `id` and `at`.
 * Refuses what does not follow the protocol or is not a type published by the node.
 */
export function forwardNodeEvent(value: unknown): PluginEvent | null {
  if (typeof value !== "object" || value === null) return null;
  const event = value as Partial<PluginEvent>;
  if (
    event.protocol !== PLUGIN_EVENTS_PROTOCOL ||
    typeof event.id !== "string" ||
    typeof event.target !== "string" ||
    typeof event.at !== "string" ||
    !NODE_EVENT_TYPES.includes(event.type as PluginEventType) ||
    typeof event.payload !== "object" || event.payload === null
  ) {
    return null;
  }
  return deliver({ ...(event as PluginEvent), source: "pom" });
}

function deliver<P>(event: PluginEvent<P>): PluginEvent<P> {
  const target = event.target;
  const type = event.type;
  if (target === "*") lastByType.set(type, event as PluginEvent);
  for (const key of [type, "*"]) {
    for (const entry of handlers.get(key) ?? []) {
      if (!reaches(event as PluginEvent, entry.plugin)) continue;
      try {
        entry.handler(event as PluginEvent);
      } catch (error) {
        console.warn("[pom-plugin-events] handler falhou", error);
      }
    }
  }
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent(PLUGIN_EVENT_DOM, { detail: event }));
    for (const frame of document.querySelectorAll<HTMLIFrameElement>("iframe[data-pom-plugin]")) {
      const code = frame.dataset.pomPlugin;
      if (target !== "*" && code !== target) continue;
      frame.contentWindow?.postMessage(event, window.location.origin);
    }
  }
  return event;
}

/**
 * Subscribes to a type (or `*`). With `plugin`, it only receives events addressed to it or
 * to all. `replay` immediately delivers the last global event of that type -
 * useful to learn the current language and theme when mounting the screen.
 */
export function subscribePluginEvents(
  type: PluginEventType | "*",
  handler: Handler,
  options: { plugin?: string; replay?: boolean } = {},
): () => void {
  const entry = { handler, plugin: options.plugin };
  const set = handlers.get(type) ?? new Set();
  set.add(entry);
  handlers.set(type, set);
  if (options.replay) {
    const replayed = type === "*" ? [...lastByType.values()] : [lastByType.get(type)].filter(Boolean);
    for (const event of replayed as PluginEvent[]) handler(event);
  }
  return () => {
    set.delete(entry);
  };
}

export function lastPluginEvent(type: PluginEventType): PluginEvent | null {
  return lastByType.get(type) ?? null;
}

export function installPluginCommandExecutor(next: PluginCommandExecutor | null) {
  executor = next;
}

function normalizeNotify(value: unknown): NotifyRequest | null {
  if (typeof value !== "object" || value === null) return null;
  const request = value as Record<string, unknown>;
  const title = typeof request.title === "string" ? request.title.trim() : "";
  if (!title || title.length > MAX_TITLE) return null;
  if (request.body !== undefined && (typeof request.body !== "string" || request.body.length > MAX_BODY)) return null;
  const level = request.level ?? "info";
  if (!LEVELS.includes(level as NotificationLevel)) return null;
  const scope = request.scope ?? "local";
  if (scope !== "local" && scope !== "network") return null;
  return { title, body: request.body as string | undefined, level: level as NotificationLevel, scope };
}

function normalizeConfirm(value: unknown): ConfirmRequest | null {
  if (typeof value !== "object" || value === null) return null;
  const request = value as Record<string, unknown>;
  const title = typeof request.title === "string" ? request.title.trim() : "";
  if (!title || title.length > MAX_TITLE) return null;
  for (const key of ["body", "acceptLabel", "cancelLabel"]) {
    const field = request[key];
    if (field !== undefined && (typeof field !== "string" || field.length > MAX_BODY)) return null;
  }
  if (request.tone !== undefined && request.tone !== "primary" && request.tone !== "danger") return null;
  if (request.display !== undefined && request.display !== "notification" && request.display !== "dialog") return null;
  return {
    title,
    body: request.body as string | undefined,
    acceptLabel: request.acceptLabel as string | undefined,
    cancelLabel: request.cancelLabel as string | undefined,
    tone: request.tone as ConfirmRequest["tone"],
    display: (request.display as ConfirmRequest["display"]) ?? "notification",
  };
}

function respond(code: string, response: NotificationResponse): NotificationResponse {
  publishPluginEvent("notification.response", { ...response }, code);
  return response;
}

/**
 * Executes a plugin's command. It always ends with the `notification.response`
 * event for the plugin itself, including on refusal.
 */
export async function runPluginCommand(command: unknown): Promise<NotificationResponse> {
  const value = (typeof command === "object" && command !== null ? command : {}) as Partial<PluginCommand>;
  const requestId = typeof value.id === "string" && value.id ? value.id : newId();
  const code = typeof value.plugin_code === "string" ? value.plugin_code : "";
  const isLoader = typeof value.type === "string" && value.type.startsWith("loader.");
  const kind = isLoader ? "loader" : value.type === "notification.confirm" ? "confirm" : "notify";
  const fail = (error: string) => respond(code || "*", { request_id: requestId, kind, action: "failed", error });
  if (value.protocol !== PLUGIN_EVENTS_PROTOCOL) return fail("protocol_unsupported");
  // The loader answers to the plugin screen POM mounted from its manifest,
  // which proves more than the executor's catalog check, and needs no bridge.
  if (isLoader && code) {
    const command = normalizeLoaderCommand(value.type, value.payload);
    if (!command) return fail("request_invalid");
    const outcome = applyLoaderCommand(code, command.type, command.state);
    if (outcome !== "delivered") return fail(outcome);
    return respond(code, { request_id: requestId, kind, action: "delivered" });
  }
  if (!executor) return fail("host_unavailable");
  if (!code || !executor.isPlugin(code)) return fail("plugin_unknown");
  try {
    if (value.type === "notification.notify") {
      const request = normalizeNotify(value.payload);
      if (!request) return fail("request_invalid");
      await executor.notify(code, request);
      return respond(code, { request_id: requestId, kind, action: "delivered" });
    }
    if (value.type === "notification.confirm") {
      const request = normalizeConfirm(value.payload);
      if (!request) return fail("request_invalid");
      const action = await executor.confirm(code, request);
      return respond(code, { request_id: requestId, kind, action });
    }
    return fail("command_unknown");
  } catch (error) {
    return fail(error instanceof Error ? error.message : String(error));
  }
}

/** Channels without the SDK: `CustomEvent` on the `window` and same-origin `postMessage`. */
export function listenForPluginCommands(): () => void {
  const onDom = (event: Event) => void runPluginCommand((event as CustomEvent).detail);
  const onMessage = (event: MessageEvent) => {
    if (event.origin !== window.location.origin) return;
    const data = event.data as { protocol?: unknown; type?: unknown } | null;
    if (data?.protocol !== PLUGIN_EVENTS_PROTOCOL) return;
    const type = typeof data.type === "string" ? data.type : "";
    if (type !== "notification.notify" && type !== "notification.confirm" && !type.startsWith("loader.")) return;
    void runPluginCommand(data);
  };
  window.addEventListener(PLUGIN_REQUEST_DOM, onDom);
  window.addEventListener("message", onMessage);
  return () => {
    window.removeEventListener(PLUGIN_REQUEST_DOM, onDom);
    window.removeEventListener("message", onMessage);
  };
}

/** A plugin-scoped API, delivered by the SDK (`host.plugin(code)`). */
export function scopedPluginApi(code: string) {
  const command = (type: PluginCommand["type"], payload: PluginCommand["payload"]) =>
    runPluginCommand({ protocol: PLUGIN_EVENTS_PROTOCOL, id: newId(), type, plugin_code: code, payload });
  return {
    code,
    events: {
      subscribe: (type: PluginEventType | "*", handler: Handler, options: { replay?: boolean } = {}) =>
        subscribePluginEvents(type, handler, { plugin: code, replay: options.replay }),
      last: lastPluginEvent,
    },
    notifications: {
      notify: (request: NotifyRequest) => command("notification.notify", request),
      confirm: (request: ConfirmRequest) => command("notification.confirm", request),
    },
    /** POM's loader and installer over this plugin's mounted screen (host version 7). */
    loader: {
      show: (request: LoaderText = {}) => command("loader.show", { ...request }),
      progress: (request: LoaderText & { progress: number }) => command("loader.progress", { ...request }),
      error: (request: LoaderText & { message: string; retry?: boolean }) => command("loader.error", { ...request }),
      success: (request: LoaderText = {}) => command("loader.success", { ...request }),
      hide: () => command("loader.hide", {}),
    },
    /**
     * The plugin's own API: `POST /api/ui/plugins/<code>/rpc/<op>`, answered
     * by the plugin's `rpc.<op>` operation (see `docs/subsystems/plugins-ui.md`).
     */
    rpc: <T = unknown>(op: string, params?: unknown) =>
      api.post<T>(`/api/ui/plugins/${encodeURIComponent(code)}/rpc/${encodeURIComponent(op)}`, params ?? {}),
  };
}

/** For tests only. */
export function resetPluginEventsForTest() {
  handlers.clear();
  lastByType.clear();
  executor = null;
}

/** "Try again" on a loader error: tells only that plugin. */
export function publishLoaderRetry(code: string): void {
  publishPluginEvent("loader.retry", {}, code);
}
