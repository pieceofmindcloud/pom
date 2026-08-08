/**
 * Talking to the node.
 *
 * Paths are relative: in production the node serves this app on its own gateway
 * port, and in development Vite proxies the same paths (see `vite.config.ts`).
 *
 * Authentication is the cluster's **connection key** — the same credential the
 * operator pastes into a second machine's setup screen. `GET /api/ui/node`
 * returns it without authentication, so the panel can pick it up on load and
 * then sign the routes that do demand it (`/admin/*`, `/api/ui/apply`, `/v1`).
 */

const KEY_STORAGE = "codai.connectionKey";

let connectionKey: string = localStorage.getItem(KEY_STORAGE) ?? "";

export function getConnectionKey(): string {
  return connectionKey;
}

export function setConnectionKey(key: string) {
  connectionKey = key ?? "";
  if (connectionKey) localStorage.setItem(KEY_STORAGE, connectionKey);
  else localStorage.removeItem(KEY_STORAGE);
}

/** Thrown for any non-2xx answer, carrying whatever the node explained. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function explain(res: Response): Promise<string> {
  const text = await res.text().catch(() => "");
  if (!text) return `HTTP ${res.status}`;
  try {
    const body = JSON.parse(text);
    const detail =
      body?.error?.message ?? body?.error ?? body?.message ?? body?.detail;
    if (typeof detail === "string" && detail) return detail;
  } catch {
    /* not JSON — the raw body is the best explanation available */
  }
  return text.slice(0, 400);
}

export interface RequestOptions {
  method?: string;
  body?: unknown;
  /** Send the connection key. On by default; the node's own reads don't need it. */
  auth?: boolean;
  signal?: AbortSignal;
}

export async function request<T>(
  path: string,
  { method = "GET", body, auth = true, signal }: RequestOptions = {},
): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (auth && connectionKey) headers.Authorization = `Bearer ${connectionKey}`;

  const res = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    signal,
  });

  if (!res.ok) throw new ApiError(res.status, await explain(res));
  if (res.status === 204) return undefined as T;

  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

export const api = {
  get: <T>(path: string, opts?: RequestOptions) => request<T>(path, opts),
  post: <T>(path: string, body?: unknown, opts?: RequestOptions) =>
    request<T>(path, { ...opts, method: "POST", body }),
  del: <T>(path: string, opts?: RequestOptions) =>
    request<T>(path, { ...opts, method: "DELETE" }),
};

/** Open an SSE stream and yield each `data:` payload as it lands. */
export async function* streamSse(
  path: string,
  body: unknown,
  signal?: AbortSignal,
): AsyncGenerator<string> {
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    Accept: "text/event-stream",
  };
  if (connectionKey) headers.Authorization = `Bearer ${connectionKey}`;

  const res = await fetch(path, {
    method: "POST",
    headers,
    body: JSON.stringify(body),
    signal,
  });
  if (!res.ok) throw new ApiError(res.status, await explain(res));
  if (!res.body) throw new ApiError(0, "a resposta não trouxe corpo para ler");

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    // Events are separated by a blank line; a partial one stays in the buffer.
    let cut = buffer.indexOf("\n\n");
    while (cut !== -1) {
      const chunk = buffer.slice(0, cut);
      buffer = buffer.slice(cut + 2);
      for (const line of chunk.split("\n")) {
        if (line.startsWith("data:")) yield line.slice(5).trim();
      }
      cut = buffer.indexOf("\n\n");
    }
  }
}
