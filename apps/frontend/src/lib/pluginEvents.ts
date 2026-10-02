/**
 * Protocolo de eventos entre o POM e os plugins (`pom-plugin-events/v1`).
 *
 * Um único envelope JSON viaja por todos os canais, para que qualquer tela de
 * plugin - um componente React montado pelo host, um script sem o SDK ou uma
 * aplicação de terceiros embutida (como a do harness) - receba os mesmos
 * eventos:
 *
 * - SDK: `__POM_HOST__.events.subscribe(tipo, handler)`;
 * - DOM: `CustomEvent("pom:plugin-event")` disparado no `window`;
 * - iframes: `postMessage(envelope)` para `iframe[data-pom-plugin]` da mesma
 *   origem;
 * - backend nativo: operação `host.event` do `query` (lado Rust, `plugins.rs`).
 *
 * No sentido inverso, o plugin pede ações ao POM com comandos
 * (`notification.notify`, `notification.confirm`) pelo SDK ou por
 * `CustomEvent("pom:plugin-request")`/`postMessage`. O resultado de todo comando
 * volta como o evento `notification.response`, com o `request_id` do pedido.
 * Ver `docs/subsystems/plugins-ui.md`, seção "Events and notifications".
 */
import { randomId as newId } from "./id";
import { api } from "./api";

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
  | "model.stopped";

/** Tipos que o nó publica em `GET /api/ui/plugins/events` e a ponte repassa. */
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
  /** `plugin_code` destinatário, ou `*` para todos os plugins. */
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
  /** `local` avisa só quem está nesta interface; `network`, todos os nós. */
  scope?: "local" | "network";
}

export interface ConfirmRequest {
  title: string;
  body?: string;
  acceptLabel?: string;
  cancelLabel?: string;
  tone?: "primary" | "danger";
  /**
   * `notification` (padrão): entra no sino da barra superior com Aceitar e
   * Recusar, como uma oferta de transferência. `dialog`: abre um diálogo
   * modal na hora.
   */
  display?: "notification" | "dialog";
}

export type NotificationAction = "delivered" | "failed" | "accepted" | "cancelled";

export interface NotificationResponse {
  request_id: string;
  kind: "notify" | "confirm";
  action: NotificationAction;
  error?: string;
}

export interface PluginCommand {
  protocol: typeof PLUGIN_EVENTS_PROTOCOL;
  id: string;
  type: "notification.notify" | "notification.confirm";
  plugin_code: string;
  payload: NotifyRequest | ConfirmRequest;
}

type Handler = (event: PluginEvent) => void;

/** Quem executa os comandos: instalado pela `PluginBridge` montada no App. */
export interface PluginCommandExecutor {
  isPlugin: (code: string) => boolean;
  notify: (code: string, request: NotifyRequest) => Promise<void>;
  confirm: (code: string, request: ConfirmRequest) => Promise<"accepted" | "cancelled">;
}

const handlers = new Map<string, Set<{ handler: Handler; plugin?: string }>>();
const lastByType = new Map<PluginEventType, PluginEvent>();
let executor: PluginCommandExecutor | null = null;

const LEVELS: readonly NotificationLevel[] = ["info", "success", "warning", "error"];
const MAX_TITLE = 200;
const MAX_BODY = 2048;

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

/** Publica um evento em todos os canais do navegador. */
export function publishPluginEvent<P extends Record<string, unknown>>(
  type: PluginEventType,
  payload: P,
  target = "*",
): PluginEvent<P> {
  return deliver(envelope(type, payload, target));
}

/**
 * Repassa às telas um envelope que o nó já montou, preservando `id` e `at`.
 * Recusa o que não segue o protocolo ou não é um tipo publicado pelo nó.
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
 * Assina um tipo (ou `*`). Com `plugin`, só recebe eventos endereçados a ele ou
 * a todos. `replay` entrega de imediato o último evento global daquele tipo -
 * útil para saber o idioma e o tema atuais ao montar a tela.
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
 * Executa um comando de um plugin. Sempre termina com o evento
 * `notification.response` para o próprio plugin, inclusive em recusa.
 */
export async function runPluginCommand(command: unknown): Promise<NotificationResponse> {
  const value = (typeof command === "object" && command !== null ? command : {}) as Partial<PluginCommand>;
  const requestId = typeof value.id === "string" && value.id ? value.id : newId();
  const code = typeof value.plugin_code === "string" ? value.plugin_code : "";
  const kind = value.type === "notification.confirm" ? "confirm" : "notify";
  const fail = (error: string) => respond(code || "*", { request_id: requestId, kind, action: "failed", error });
  if (value.protocol !== PLUGIN_EVENTS_PROTOCOL) return fail("protocol_unsupported");
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

/** Canais sem SDK: `CustomEvent` no `window` e `postMessage` da mesma origem. */
export function listenForPluginCommands(): () => void {
  const onDom = (event: Event) => void runPluginCommand((event as CustomEvent).detail);
  const onMessage = (event: MessageEvent) => {
    if (event.origin !== window.location.origin) return;
    const data = event.data as { protocol?: unknown; type?: unknown } | null;
    if (data?.protocol !== PLUGIN_EVENTS_PROTOCOL) return;
    if (data.type !== "notification.notify" && data.type !== "notification.confirm") return;
    void runPluginCommand(data);
  };
  window.addEventListener(PLUGIN_REQUEST_DOM, onDom);
  window.addEventListener("message", onMessage);
  return () => {
    window.removeEventListener(PLUGIN_REQUEST_DOM, onDom);
    window.removeEventListener("message", onMessage);
  };
}

/** API com escopo de um plugin, entregue pelo SDK (`host.plugin(code)`). */
export function scopedPluginApi(code: string) {
  const command = (type: PluginCommand["type"], payload: NotifyRequest | ConfirmRequest) =>
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
    /**
     * The plugin's own API: `POST /api/ui/plugins/<code>/rpc/<op>`, answered
     * by the plugin's `rpc.<op>` operation (see `docs/subsystems/plugins-ui.md`).
     */
    rpc: <T = unknown>(op: string, params?: unknown) =>
      api.post<T>(`/api/ui/plugins/${encodeURIComponent(code)}/rpc/${encodeURIComponent(op)}`, params ?? {}),
  };
}

/** Só para testes. */
export function resetPluginEventsForTest() {
  handlers.clear();
  lastByType.clear();
  executor = null;
}
