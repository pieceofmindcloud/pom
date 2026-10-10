/**
 * Talking to the node.
 *
 * Paths are relative: in production the node serves this app on its own gateway
 * port, and in development Vite proxies the same paths (see `vite.config.ts`).
 *
 * The connection key authenticates node-to-node and selected administrative
 * operations. Inference uses
 * the browser's dedicated `pom-[node_id]` API key, created by `/api/ui/chat-key`.
 * The separate `X-POM-Access-Token` carries the human access password to the
 * administrative surface; it is kept in memory for this browser tab.
 */

import { getActiveLocale } from "../i18n/locales";
import { storageKey } from "./brand";
import { modelSearchPath, type ModelSearchPathOptions } from "./modelProvider";
import type {
  Deployment,
  AddNodeOptionsResponse,
  HuggingFaceTokenConfig,
  RemoteModelDetails,
  RemoteModelProviderStatus,
  RemoteModelSearchResponse,
  RemoteStorage,
  RuntimeParams,
  KvCacheType,
} from "./types";
import type { KvCacheCleared, NetworkChatRequest } from "./types";

const KEY_STORAGE = storageKey("connectionKey");

let connectionKey: string = localStorage.getItem(KEY_STORAGE) ?? "";
let adminAccessToken = "";
let chatApiKey: string | null = null;
let chatApiKeyRequest: Promise<string> | null = null;

export const ADMIN_ACCESS_TOKEN_HEADER = "X-POM-Access-Token";
/**
 * The node names the administrative refusal in this header (`backend/auth.rs`).
 * The body's sentence is for the person and changes with the language; this
 * value is stable, and it is what separates "the administrative session ended"
 * from any other 401.
 */
export const ADMIN_ERROR_HEADER = "x-pom-admin-error";
const ADMIN_SESSION_LOST = ["missing-token", "invalid-token"];

const adminSessionLostListeners = new Set<() => void>();

/**
 * Signals when the administrative session stopped being valid.
 *
 * The session lives in the node's memory and the human password lives in this
 * tab's memory: a node restart, or the end of the session's 8 h, drops both
 * without the tab noticing. Before this, each screen printed the server's raw
 * 401 and the operator had no way back to the gate other than reloading by hand.
 */
export function onAdminSessionLost(handler: () => void): () => void {
  adminSessionLostListeners.add(handler);
  return () => {
    adminSessionLostListeners.delete(handler);
  };
}

/** Discards the dead credential and wakes whoever knows how to return to the gate. */
function noteAdminSessionLost(res: Response, path: string) {
  if (!isAdminSurfacePath(path)) return;
  const kind = res.headers.get(ADMIN_ERROR_HEADER);
  if (!kind || !ADMIN_SESSION_LOST.includes(kind)) return;
  if (adminAccessToken) setAdminAccessToken("");
  for (const handler of adminSessionLostListeners) handler();
}

export function isAdminSurfacePath(path: string): boolean {
  const pathname = path.split("?", 1)[0];
  return pathname === "/api/ui"
    || pathname.startsWith("/api/ui/")
    || pathname === "/admin"
    || pathname.startsWith("/admin/");
}

export function getConnectionKey(): string {
  return connectionKey;
}

export function setConnectionKey(key: string) {
  const nextKey = key ?? "";
  if (nextKey !== connectionKey) clearConditionalCache();
  connectionKey = nextKey;
  if (connectionKey) localStorage.setItem(KEY_STORAGE, connectionKey);
  else localStorage.removeItem(KEY_STORAGE);
}

export function getAdminAccessToken(): string {
  return adminAccessToken;
}

export function setAdminAccessToken(token: string) {
  const nextToken = token ?? "";
  if (nextToken !== adminAccessToken) clearConditionalCache();
  adminAccessToken = nextToken;
}

export function clearAdminAccessToken() {
  setAdminAccessToken("");
}

function isInferencePath(path: string): boolean {
  const pathname = path.split("?", 1)[0];
  return pathname === "/api/generate" || pathname === "/generate" || pathname.startsWith("/v1/");
}

export function forgetChatApiKey() {
  chatApiKey = null;
}

/** Return the node-owned API key reserved for this UI's inference requests. */
export async function ensureChatApiKey(force = false): Promise<string> {
  if (!force && chatApiKey) return chatApiKey;
  if (chatApiKeyRequest) return chatApiKeyRequest;
  chatApiKeyRequest = (async () => {
    const response = await request<{ key?: unknown }>("/api/ui/chat-key", {
      method: "POST",
    });
    if (typeof response.key !== "string" || !response.key) {
      throw new ApiError(502, "the chat API key was not provisioned");
    }
    chatApiKey = response.key;
    return response.key;
  })().finally(() => {
    chatApiKeyRequest = null;
  });
  return chatApiKeyRequest;
}

/** Every network-chat action carries the node-owned POM API key in its frame. */
export async function postNetworkChat<T = { accepted: boolean }>(
  action: NetworkChatRequest,
): Promise<T> {
  return api.post<T>("/api/ui/network-chat", {
    ...action,
    api_key: await ensureChatApiKey(),
  });
}

export async function downloadNetworkChatFile(messageId: string): Promise<Blob> {
  const headers: Record<string, string> = {};
  if (connectionKey) headers.Authorization = `Bearer ${connectionKey}`;
  const response = await fetch(`/api/ui/network-chat/file/${encodeURIComponent(messageId)}`, {
    headers,
    credentials: "same-origin",
  });
  if (!response.ok) {
    const { message, body } = await explain(response);
    throw new ApiError(response.status, message, body);
  }
  return response.blob();
}

/** Thrown for any non-2xx answer, carrying whatever the node explained. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    /**
     * Decoded body of the response, when it was JSON.
     *
     * A refusal sometimes carries more than a sentence: deletion returns 409
     * with the list of warnings the screen needs to show before asking whether
     * the operator wants to force it. Flattening that into a string would lose
     * the list.
     */
    readonly body?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function explain(res: Response): Promise<{ message: string; body?: unknown }> {
  const text = await res.text().catch(() => "");
  if (!text) return { message: `HTTP ${res.status}` };
  try {
    const body = JSON.parse(text);
    const detail =
      body?.error?.message ?? body?.error ?? body?.message ?? body?.detail;
    if (typeof detail === "string" && detail) return { message: detail, body };
    return { message: text.slice(0, 400), body };
  } catch {
    /* not JSON — the raw body is the best explanation available */
  }
  return { message: text.slice(0, 400) };
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
  const inference = isInferencePath(path);
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (inference) {
      headers["X-API-Key"] = await ensureChatApiKey(attempt > 0);
    } else if (auth && connectionKey) {
      headers.Authorization = `Bearer ${connectionKey}`;
    }
    if (adminAccessToken && isAdminSurfacePath(path)) {
      headers[ADMIN_ACCESS_TOKEN_HEADER] = adminAccessToken;
    }

    const res = await fetch(path, {
      method,
      headers,
      credentials: "same-origin",
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
    });

    if (inference && (res.status === 401 || res.status === 403) && attempt === 0) {
      forgetChatApiKey();
      continue;
    }
    if (!res.ok) {
      noteAdminSessionLost(res, path);
      const { message, body: detail } = await explain(res);
      throw new ApiError(res.status, message, detail);
    }
    if (res.status === 204) return undefined as T;

    const text = await res.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }
  throw new ApiError(502, "inference authentication failed");
}

/** Read a node-served text asset while preserving the same admin session. */
export async function requestText(
  path: string,
  { auth = true, signal }: Pick<RequestOptions, "auth" | "signal"> = {},
): Promise<string> {
  const headers: Record<string, string> = {};
  if (auth && connectionKey) headers.Authorization = "Bearer " + connectionKey;
  if (adminAccessToken && isAdminSurfacePath(path)) {
    headers[ADMIN_ACCESS_TOKEN_HEADER] = adminAccessToken;
  }
  const res = await fetch(path, { method: "GET", headers, credentials: "same-origin", signal });
  if (!res.ok) {
    noteAdminSessionLost(res, path);
    const { message, body } = await explain(res);
    throw new ApiError(res.status, message, body);
  }
  return res.text();
}

type ConditionalCacheEntry = {
  etag: string;
  data: unknown;
};

const conditionalCache = new Map<string, ConditionalCacheEntry>();

/**
 * Forget conditional GET snapshots. Tests use this to isolate requests; a
 * caller changing the connection target can also clear it before switching
 * nodes so an old node never supplies a valid-looking 304.
 */
export function clearConditionalCache() {
  conditionalCache.clear();
}

/**
 * GET that sends the last ETag and keeps the last parsed value on 304.
 *
 * The regular request helper deliberately stays simple for mutations and
 * one-shot reads. These four live snapshots are different: a 304 has no JSON
 * to parse, so React Query receives the same object it already renders.
 */
export async function requestConditional<T>(
  path: string,
  { auth = true, signal }: Pick<RequestOptions, "auth" | "signal"> = {},
): Promise<T> {
  const cached = conditionalCache.get(path);
  const inference = isInferencePath(path);
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const headers: Record<string, string> = {};
    if (inference) headers["X-API-Key"] = await ensureChatApiKey(attempt > 0);
    else if (auth && connectionKey) headers.Authorization = `Bearer ${connectionKey}`;
    if (adminAccessToken && isAdminSurfacePath(path)) {
      headers[ADMIN_ACCESS_TOKEN_HEADER] = adminAccessToken;
    }
    if (cached) headers["If-None-Match"] = cached.etag;

    let res = await fetch(path, { method: "GET", headers, credentials: "same-origin", signal });
    if (inference && (res.status === 401 || res.status === 403) && attempt === 0) {
      forgetChatApiKey();
      continue;
    }
    // A 304 without a local value is not useful to a new tab. Recover with one
    // unconditional read instead of exposing an impossible empty state.
    if (res.status === 304 && !cached) {
      res = await fetch(path, { method: "GET", headers: { ...headers, "If-None-Match": "" }, credentials: "same-origin", signal });
    }
    if (res.status === 304 && cached) return cached.data as T;
    if (!res.ok) {
      noteAdminSessionLost(res, path);
      const { message, body: detail } = await explain(res);
      throw new ApiError(res.status, message, detail);
    }

    const text = await res.text();
    const data = (text ? JSON.parse(text) : undefined) as T;
    const etag = res.headers.get("ETag");
    if (etag) conditionalCache.set(path, { etag, data });
    else conditionalCache.delete(path);
    return data;
  }
  throw new ApiError(502, "inference authentication failed");
}

export const api = {
  get: <T>(path: string, opts?: RequestOptions) => request<T>(path, opts),
  getText: (path: string, opts?: Pick<RequestOptions, "auth" | "signal">) => requestText(path, opts),
  getConditional: <T>(
    path: string,
    opts?: Pick<RequestOptions, "auth" | "signal">,
  ) => requestConditional<T>(path, opts),
  post: <T>(path: string, body?: unknown, opts?: RequestOptions) =>
    request<T>(path, { ...opts, method: "POST", body }),
  del: <T>(path: string, opts?: RequestOptions) =>
    request<T>(path, { ...opts, method: "DELETE" }),
};

export interface RemoteModelSearchOptions extends ModelSearchPathOptions {}

export function searchRemoteModels(
  options: RemoteModelSearchOptions,
  signal?: AbortSignal,
) {
  return api.get<RemoteModelSearchResponse>(modelSearchPath(options), { signal });
}

/** Starts the process of an on-demand plugin (menu click). */
export function openPlugin(code: string) {
  return api.post<{ code: string; running: boolean }>(
    `/api/ui/plugins/${encodeURIComponent(code)}/open`,
    {},
  );
}

/** Stops the process of an on-demand plugin (X badge). */
export function closePlugin(code: string) {
  return api.post<{ code: string; running: boolean }>(
    `/api/ui/plugins/${encodeURIComponent(code)}/close`,
    {},
  );
}

export function getRemoteModelDetails(
  repoId: string,
  revision?: string,
  signal?: AbortSignal,
) {
  const params = new URLSearchParams({ repo_id: repoId });
  if (revision) params.set("revision", revision);
  return api.get<RemoteModelDetails>(`/api/ui/model-provider/details?${params}`, { signal });
}

export function getRemoteModelStorage(signal?: AbortSignal) {
  return api.get<RemoteStorage>("/api/ui/model-provider/storage", { signal });
}

export function getRemoteModelProviderStatus(signal?: AbortSignal) {
  return api.get<RemoteModelProviderStatus>("/api/ui/model-provider/status", { signal });
}

export function getHuggingFaceTokenConfig(signal?: AbortSignal) {
  return api.get<HuggingFaceTokenConfig>("/api/ui/model-provider/config", { signal });
}

export function saveHuggingFaceToken(token: string | null) {
  return api.post<HuggingFaceTokenConfig>("/api/ui/model-provider/config", { token });
}

export interface RemoteModelDownloadRequest {
  repoId: string;
  revision?: string;
  filename: string;
  quantization?: string;
}

export function startRemoteModelDownload(request: RemoteModelDownloadRequest) {
  return api.post<{ download_id: string }>("/api/ui/model-provider/downloads", {
    repo_id: request.repoId,
    revision: request.revision ?? null,
    filename: request.filename,
    quantization: request.quantization ?? null,
  });
}

export function cancelRemoteModelDownload(downloadId: string) {
  return api.post<{ download_id: string; status: string }>(
    `/api/ui/model-provider/downloads/${encodeURIComponent(downloadId)}/cancel`,
  );
}

export function retryRemoteModelDownload(downloadId: string) {
  return api.post<{ download_id: string }>(
    `/api/ui/model-provider/downloads/${encodeURIComponent(downloadId)}/retry`,
  );
}

/**
 * Updates and restarts a machine of the cluster.
 *
 * Without `nodeId`, or with this node's own `node_id`, it updates this machine.
 * With another peer's `node_id`, this node forwards the call to that peer's
 * `/api/ui/dev/update` (`src/devupdate.rs`) - the panel never talks directly to
 * another peer, `api.ts` only uses relative paths.
 */
export function devUpdate(nodeId?: string) {
  return api.post<{ job_id: string }>(
    "/api/ui/dev/update",
    nodeId ? { node_id: nodeId } : {},
  );
}

/**
 * Adds a machine to a model that is already online.
 *
 * It touches only the target node: whoever is already serving is not restarted.
 * The response carries the layers that ended up overlapping, which is how the
 * operator knows whether they created a parallel route or filled a gap.
 */
export function addNode(req: {
  group: string;
  deploy_id?: string;
  kind?: "shards" | "whole";
  target_node_id: string;
  start_layer: number;
  end_layer: number;
  n_gpu_layers?: number;
  offload_kqv?: boolean;
  cache_type_k?: KvCacheType;
  cache_type_v?: KvCacheType;
}) {
  return api.post<{
    applied: boolean;
    overlapping_layers: number[];
    deploy_id?: string;
    version?: number;
  }>(
    "/api/ui/deploy/node",
    req,
  );
}

/** Inventory and settings of the machine currently chosen in the dialog. */
export function addNodeOptions(group: string, targetNodeId: string, signal?: AbortSignal) {
  return api.post<AddNodeOptionsResponse>(
    "/api/ui/deploy/node/options",
    { group, target_node_id: targetNodeId },
    { signal },
  );
}

/**
 * Removes a machine from a model, without touching the others.
 *
 * An empty `uncovered_layers` means another machine serves the same range: the
 * network loses a parallel route and keeps serving.
 */
export function removeNode(nodeId: string, group: string, deployId?: string) {
  return api.post<{
    removed: boolean;
    uncovered_layers: number[];
    deploy_id?: string;
    version?: number;
    serving?: boolean;
  }>(
    "/api/ui/deploy/node/remove",
    { group, target_node_id: nodeId, ...(deployId ? { deploy_id: deployId } : {}) },
  );
}

/** What happened to a machine in an undeployment. */
export interface UndeployOutcome {
  node_id: string;
  hostname: string;
  start_layer: number;
  end_layer: number;
  /** `true` only when that machine confirmed. */
  released: boolean;
  error?: string;
}

export interface UndeployReport {
  group: string;
  released: number;
  failed: number;
  results: UndeployOutcome[];
}

/**
 * Takes the whole pipeline offline: frees RAM and VRAM on **all** the machines
 * that serve the model.
 *
 * Nothing is deleted from disk - the slices stay where they are and the same
 * deployment comes back with one click. The response is a machine-by-machine
 * report, and one that did not answer comes with `released: false` and the reason.
 */
export function undeployPipeline(group: string, deployId?: string) {
  return api.post<UndeployReport>("/api/ui/undeploy", {
    group,
    ...(deployId ? { deploy_id: deployId } : {}),
  });
}

/** A deployment with what the screen needs to know about its state. */
export interface DeploymentRow {
  deployment: Deployment;
  /** A live peer is announcing this `deploy_id`. */
  serving: boolean;
  /** There are live peers, but all the peers of this deployment are idle. */
  idle?: boolean;
  /** This is the one that serves whoever asks for just the model name, without an `@` suffix. */
  is_default: boolean;
  /** Which nodes keep a copy of the record. */
  holders: string[];
}

/**
 * All the cluster's deployments.
 *
 * Each one's record lives in one copy per participating node, so this node
 * asks the peers and merges - no single machine knows the whole list.
 */
export function listDeployments() {
  return api.get<{ node_id: string; deployments: DeploymentRow[] }>(
    "/api/ui/deployments",
  );
}

/**
 * Redeploys a `deploy_id` that already exists.
 *
 * Same `deploy_id`, same machines, same ranges: the machines reload the same
 * file. Nothing is planned or copied, and whoever is using `model@deploy_id`
 * keeps reaching the same place.
 *
 * `version` redeploys an already registered version (without appending another).
 * `params` redeploys with new parameters: ones equal to an existing version's
 * reactivate that version, different ones append a new version. Provide one of
 * the two.
 */
export function redeployDeployment(
  deployId: string,
  body: {
    params?: RuntimeParams;
    version?: number;
    per_node?: Record<string, { n_gpu_layers?: number; offload_kqv?: boolean }>;
  },
) {
  return api.post<{ job_id: string; deploy_id: string }>(
    `/api/ui/deployments/${encodeURIComponent(deployId)}/redeploy`,
    body,
  );
}

/** Makes this the deployment that serves whoever asks for just the model name. */
export function activateDeployment(deployId: string) {
  return api.post<{ activated: boolean; deploy_id: string; model: string }>(
    `/api/ui/deployments/${encodeURIComponent(deployId)}/activate`,
    {},
  );
}

/**
 * Takes a machine out of the network: it unloads the model, stops announcing
 * and forgets the peers. Its gateway keeps serving the interface, which is how
 * it comes back.
 *
 * Without `nodeId` this machine leaves. With another's `node_id`, this node
 * forwards the request to that peer's `/api/ui/leave` - the panel only uses
 * relative paths.
 */
export function leaveNetwork(nodeId?: string) {
  return api.post<{ detached: boolean }>(
    "/api/ui/leave",
    nodeId ? { node_id: nodeId } : {},
  );
}

/**
 * Empties the KV cache of **one** machine: the snapshots on disk and what its
 * inference server keeps in RAM. As in `leaveNetwork`, the request goes to the
 * local node, which forwards it to the disk's owner.
 */
export function clearKvCache(nodeId?: string) {
  return api.post<KvCacheCleared>(
    "/api/ui/kv-cache/clear",
    nodeId ? { node_id: nodeId } : {},
  );
}

/** Open an SSE stream and yield each `data:` payload as it lands. */
export async function* streamSse(
  path: string,
  body: unknown,
  signal?: AbortSignal,
): AsyncGenerator<string> {
  let res: Response | undefined;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
      Accept: "text/event-stream",
      "X-API-Key": await ensureChatApiKey(attempt > 0),
    };
    if (adminAccessToken && isAdminSurfacePath(path)) {
      headers[ADMIN_ACCESS_TOKEN_HEADER] = adminAccessToken;
    }
    res = await fetch(path, {
      method: "POST",
      headers,
      credentials: "same-origin",
      body: JSON.stringify(body),
      signal,
    });
    if ((res.status === 401 || res.status === 403) && attempt === 0) {
      forgetChatApiKey();
      continue;
    }
    break;
  }
  if (!res || !res.ok) {
    if (res) noteAdminSessionLost(res, path);
    const { message, body: detail } = await explain(res!);
    throw new ApiError(res?.status ?? 502, message, detail);
  }
  if (!res.body) {
    throw new ApiError(
      0,
      getActiveLocale() === "pt-BR"
        ? "a resposta não trouxe corpo para ler"
        : "the response did not include a readable body",
    );
  }

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
