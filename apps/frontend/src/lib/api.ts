/**
 * Talking to the node.
 *
 * Paths are relative: in production the node serves this app on its own gateway
 * port, and in development Vite proxies the same paths (see `vite.config.ts`).
 *
 * The connection key authenticates nodes and pipeline clients. The separate
 * `X-POM-Access-Token` only authenticates a person to the administrative
 * surface; it is kept in memory for this browser tab and never replaces the
 * connection key.
 */

import { getActiveLocale } from "../i18n/locales";
import { storageKey } from "./brand";
import { modelSearchPath, type ModelSearchPathOptions } from "./modelProvider";
import type {
  Deployment,
  HuggingFaceTokenConfig,
  RemoteModelDetails,
  RemoteModelProviderStatus,
  RemoteModelSearchResponse,
  RemoteStorage,
  RuntimeParams,
} from "./types";

const KEY_STORAGE = storageKey("connectionKey");

let connectionKey: string = localStorage.getItem(KEY_STORAGE) ?? "";
let adminAccessToken = "";

export const ADMIN_ACCESS_TOKEN_HEADER = "X-POM-Access-Token";

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

/** Thrown for any non-2xx answer, carrying whatever the node explained. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    /**
     * Corpo decodificado da resposta, quando era JSON.
     *
     * Uma recusa às vezes traz mais do que uma frase: a exclusão devolve 409
     * com a lista de avisos que a tela precisa mostrar antes de perguntar se
     * o operador quer forçar. Achatar isso numa string perderia a lista.
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
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (auth && connectionKey) headers.Authorization = `Bearer ${connectionKey}`;
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

  if (!res.ok) {
    const { message, body: detail } = await explain(res);
    throw new ApiError(res.status, message, detail);
  }
  if (res.status === 204) return undefined as T;

  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
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
  const headers: Record<string, string> = {};
  if (auth && connectionKey) headers.Authorization = `Bearer ${connectionKey}`;
  if (adminAccessToken && isAdminSurfacePath(path)) {
    headers[ADMIN_ACCESS_TOKEN_HEADER] = adminAccessToken;
  }
  if (cached) headers["If-None-Match"] = cached.etag;

  let res = await fetch(path, {
    method: "GET",
    headers,
    credentials: "same-origin",
    signal,
  });

  // A 304 without a local value is not useful to a new tab. Recover with one
  // unconditional read instead of exposing an impossible empty state.
  if (res.status === 304 && !cached) {
    res = await fetch(path, {
      method: "GET",
      headers: {
        ...(auth && connectionKey ? { Authorization: `Bearer ${connectionKey}` } : {}),
        ...(adminAccessToken && isAdminSurfacePath(path)
          ? { [ADMIN_ACCESS_TOKEN_HEADER]: adminAccessToken }
          : {}),
      },
      credentials: "same-origin",
      signal,
    });
  }

  if (res.status === 304 && cached) return cached.data as T;
  if (!res.ok) {
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

export const api = {
  get: <T>(path: string, opts?: RequestOptions) => request<T>(path, opts),
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
 * Atualiza e reinicia uma máquina do cluster.
 *
 * Sem `nodeId`, ou com o `node_id` deste próprio nó, atualiza esta máquina.
 * Com o `node_id` de outro peer, este nó repassa a chamada para o
 * `/api/ui/dev/update` daquele peer (`src/devupdate.rs`) — o painel nunca fala
 * direto com outro peer, `api.ts` só usa caminhos relativos.
 */
export function devUpdate(nodeId?: string) {
  return api.post<{ job_id: string }>(
    "/api/ui/dev/update",
    nodeId ? { node_id: nodeId } : {},
  );
}

/**
 * Soma uma máquina a um modelo que já está no ar.
 *
 * Toca só o nó alvo: quem já está servindo não é reiniciado. A resposta traz
 * as camadas que ficaram sobrepostas, que é como o operador sabe se criou uma
 * rota paralela ou preencheu um buraco.
 */
export function addNode(req: {
  group: string;
  target_node_id: string;
  start_layer: number;
  end_layer: number;
}) {
  return api.post<{ applied: boolean; overlapping_layers: number[] }>(
    "/api/ui/deploy/node",
    req,
  );
}

/**
 * Tira uma máquina de um modelo, sem tocar nas outras.
 *
 * `uncovered_layers` vazio significa que outra máquina serve a mesma faixa: a
 * rede perde uma rota paralela e continua servindo.
 */
export function removeNode(nodeId: string, group: string) {
  return api.post<{ removed: boolean; uncovered_layers: number[] }>(
    "/api/ui/deploy/node/remove",
    { group, target_node_id: nodeId },
  );
}

/** O que aconteceu com uma máquina numa desimplantação. */
export interface UndeployOutcome {
  node_id: string;
  hostname: string;
  start_layer: number;
  end_layer: number;
  /** Só `true` quando aquela máquina confirmou. */
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
 * Tira a pipeline inteira do ar: libera RAM e VRAM em **todas** as máquinas que
 * servem o modelo.
 *
 * Nada é apagado do disco — as fatias ficam onde estão e a mesma implantação
 * volta com um clique. A resposta é um relatório máquina a máquina, e uma que
 * não respondeu vem com `released: false` e o motivo.
 */
export function undeployPipeline(group: string, deployId?: string) {
  return api.post<UndeployReport>("/api/ui/undeploy", {
    group,
    ...(deployId ? { deploy_id: deployId } : {}),
  });
}

/** Uma implantação com o que a tela precisa saber sobre o estado dela. */
export interface DeploymentRow {
  deployment: Deployment;
  /** Há peer vivo anunciando este `deploy_id`. */
  serving: boolean;
  /** É esta que atende quem pede só o nome do modelo, sem sufixo `@`. */
  is_default: boolean;
  /** Quais nós guardam uma cópia do registro. */
  holders: string[];
}

/**
 * Todas as implantações do cluster.
 *
 * O registro de cada uma vive numa cópia por nó participante, então este nó
 * pergunta aos peers e junta — nenhuma máquina sozinha sabe a lista inteira.
 */
export function listDeployments() {
  return api.get<{ node_id: string; deployments: DeploymentRow[] }>(
    "/api/ui/deployments",
  );
}

/**
 * Reimplanta mexendo só nos parâmetros.
 *
 * Mesmo `deploy_id`, mesmas máquinas, mesmas faixas: as máquinas recarregam o
 * mesmo arquivo com outra configuração. Nada é planejado nem copiado, e quem
 * estiver usando `modelo@deploy_id` continua chegando no mesmo lugar.
 */
export function redeployDeployment(
  deployId: string,
  body: {
    params: RuntimeParams;
    per_node?: Record<string, { n_gpu_layers?: number; offload_kqv?: boolean }>;
  },
) {
  return api.post<{ job_id: string; deploy_id: string }>(
    `/api/ui/deployments/${encodeURIComponent(deployId)}/redeploy`,
    body,
  );
}

/** Faz desta a implantação que atende quem pede só o nome do modelo. */
export function activateDeployment(deployId: string) {
  return api.post<{ activated: boolean; deploy_id: string; model: string }>(
    `/api/ui/deployments/${encodeURIComponent(deployId)}/activate`,
    {},
  );
}

/**
 * Tira uma máquina da rede: ela descarrega o modelo, para de anunciar e esquece
 * os peers. O gateway dela continua servindo a interface, que é por onde ela
 * volta.
 *
 * Sem `nodeId` sai esta máquina. Com o `node_id` de outra, este nó repassa o
 * pedido ao `/api/ui/leave` daquele peer — o painel só usa caminhos relativos.
 */
export function leaveNetwork(nodeId?: string) {
  return api.post<{ detached: boolean }>(
    "/api/ui/leave",
    nodeId ? { node_id: nodeId } : {},
  );
}

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
  if (adminAccessToken && isAdminSurfacePath(path)) {
    headers[ADMIN_ACCESS_TOKEN_HEADER] = adminAccessToken;
  }

  const res = await fetch(path, {
    method: "POST",
    headers,
    credentials: "same-origin",
    body: JSON.stringify(body),
    signal,
  });
  if (!res.ok) {
    const { message, body: detail } = await explain(res);
    throw new ApiError(res.status, message, detail);
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
