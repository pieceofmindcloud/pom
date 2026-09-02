/**
 * Every read the panel does, in one place.
 *
 * Polling intervals are deliberately unequal: the cluster view fans out to each
 * peer's own `/api/ui/node`, and the catalog can walk a slow volume, so they are
 * not asked for at the same cadence as a progress bar. React Query pauses
 * refetching while the tab is hidden, which keeps an open panel from loading a
 * node nobody is watching.
 */
import { useQueries, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import {
  ApiError,
  api,
  getRemoteModelDetails,
  getRemoteModelProviderStatus,
  getRemoteModelStorage,
  getHuggingFaceTokenConfig,
  listDeployments,
  searchRemoteModels,
  setConnectionKey,
} from "./api";
import { modelDownloadJobs } from "./modelProvider";
import { jobsPollInterval, offersPollInterval } from "./polling";
import type {
  AdminNode,
  AdminAccessInfo,
  AdminAccessStatus,
  ApiKey,
  CacheTypesResponse,
  CatalogResponse,
  ChatTelemetryNetwork,
  ClusterView,
  Job,
  NodeInfo,
  PathsResponse,
  PeerCatalog,
  PeerRuntime,
  RequestLogResponse,
  RequestStats,
  TransferJob,
  TransferOffer,
} from "./types";

/**
 * Answer `fallback` instead of failing when the node does not know the route.
 *
 * A node built before this panel existed has no `/api/ui/jobs` and no
 * `/api/ui/requests`. That is a node missing a feature, not an error worth
 * putting on screen: the notification menu simply has nothing to list.
 */
async function optional<T>(promise: Promise<T>, fallback: T): Promise<T> {
  try {
    return await promise;
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return fallback;
    throw e;
  }
}

export const keys = {
  node: ["node"] as const,
  adminAccess: ["admin-access"] as const,
  adminSurface: ["admin-surface"] as const,
  adminAccessInfo: ["admin-access-info"] as const,
  cluster: ["cluster"] as const,
  models: ["models"] as const,
  clusterModels: ["cluster-models"] as const,
  paths: ["paths"] as const,
  cacheTypes: ["cache-types"] as const,
  jobs: ["jobs"] as const,
  transfers: ["transfers"] as const,
  offers: ["offers"] as const,
  adminNodes: ["admin-nodes"] as const,
  apiKeys: ["api-keys"] as const,
  requests: ["requests"] as const,
  requestStats: ["request-stats"] as const,
  peerRuntime: (nodeId: string) => ["peer-runtime", nodeId] as const,
  chatTelemetry: (executionId: string | undefined, nodeIds: string[]) => ["chat-telemetry", executionId ?? "idle", ...nodeIds] as const,
  networkRtt: (nodeIds: string[]) => ["network-rtt", ...nodeIds] as const,
  openaiModels: ["openai-models"] as const,
  deployments: ["deployments"] as const,
  remoteModels: (query: string, ggufOnly: boolean, page: number) =>
    ["remote-models", query, ggufOnly, page] as const,
  remoteModelDetails: (repoId: string, revision: string | undefined) =>
    ["remote-model-details", repoId, revision ?? "default"] as const,
  remoteModelStorage: ["remote-model-storage"] as const,
  remoteModelProviderStatus: ["remote-model-provider-status"] as const,
  huggingFaceTokenConfig: ["hugging-face-token-config"] as const,
};

/**
 * Whether this node has no configuration yet and is running the setup router.
 *
 * Deliberately its own query rather than a read of `useNode()`'s error: that one
 * polls every few seconds, and react-query clears `error` while a retry is in
 * flight, so keying the whole shell off it made the app flip from the setup
 * screen to the dashboard and back between polls. This asks once, never retries
 * and never goes stale — the answer cannot change without the node restarting,
 * and that reloads the page anyway.
 *
 * `undefined` means "still asking", and callers have to wait rather than guess,
 * or the setup screen flashes on every cold load of a configured node.
 */
export function useIsSetupMode() {
  return useQuery({
    queryKey: ["setup-mode"],
    queryFn: async () => {
      try {
        await api.get<unknown>("/api/ui/node", { auth: false });
        return false;
      } catch (e) {
        // Only a 404 means "this router has no /api/ui at all". A node that is
        // merely unreachable is not a node asking to be configured.
        return e instanceof ApiError && e.status === 404;
      }
    },
    retry: false,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  }).data;
}

export type AdminSurfaceState = "setup" | "available" | "requires-token" | "local-only";

/**
 * One probe decides which shell is safe to render. A valid server session
 * makes `/api/ui/node` succeed after a reload, so the token gate does not
 * return merely because React state was recreated by F5.
 */
export function useAdminSurface() {
  return useQuery<AdminSurfaceState>({
    queryKey: keys.adminSurface,
    queryFn: async () => {
      try {
        await api.get<unknown>("/api/ui/node", { auth: false });
        return "available";
      } catch (error) {
        if (error instanceof ApiError && error.status === 404) return "setup";
        if (error instanceof ApiError && error.status === 403) {
          try {
            const access = await api.get<AdminAccessStatus>("/api/access", { auth: false });
            if (access.admin_exposure === "localhost") return "local-only";
          } catch {
            // A transient policy read must not expose a guessed UI state.
          }
        }
        return "requires-token";
      }
    },
    retry: false,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
}

/** Public metadata needed before the app knows whether to show the token gate. */
export function useAdminAccess() {
  return useQuery({
    queryKey: keys.adminAccess,
    queryFn: () => api.get<AdminAccessStatus>("/api/access", { auth: false }),
    retry: false,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
}

/** Authenticated administrative state, including the token for the settings page. */
export function useAdminAccessInfo() {
  return useQuery({
    queryKey: keys.adminAccessInfo,
    queryFn: () => api.get<AdminAccessInfo>("/api/ui/access", { auth: false }),
    retry: false,
    staleTime: 10_000,
  });
}

export function useNode() {
  return useQuery({
    queryKey: keys.node,
    queryFn: async () => {
      const node = await api.get<NodeInfo>("/api/ui/node", { auth: false });
      // The key travels with the node's own description; picking it up here is
      // what lets every authenticated call below just work.
      if (node.connection_key) setConnectionKey(node.connection_key);
      return node;
    },
    refetchInterval: 4000,
  });
}

export function useCluster() {
  return useQuery({
    queryKey: keys.cluster,
    queryFn: () => api.getConditional<ClusterView>("/api/ui/cluster", { auth: false }),
    refetchInterval: 5000,
  });
}

/**
 * Como uma máquina carregou o modelo, buscado só com o card aberto.
 *
 * `enabled` é o ponto: isto sai do `/health` do peer, e pedir por peer a cada
 * ciclo do Painel seria um leque de chamadas por uma tela que quase sempre está
 * fechada. Um nó antigo sem a rota devolve 404, e aí a seção some em vez de
 * virar erro - mesmo critério do `optional` acima.
 */
export function usePeerRuntime(nodeId: string, enabled: boolean) {
  return useQuery({
    queryKey: keys.peerRuntime(nodeId),
    queryFn: () =>
      optional(
        api.get<PeerRuntime>(`/api/ui/peer/${encodeURIComponent(nodeId)}/runtime`, {
          auth: false,
        }),
        { loaded: false, reason: "rota indisponível neste nó" } as PeerRuntime,
      ),
    enabled,
    refetchInterval: 15_000,
    staleTime: 10_000,
  });
}

/** Snapshot de saúde dos peers que acabaram de participar de uma execução. */
export function usePeerRuntimes(nodeIds: string[], enabled: boolean, executionId?: string) {
  return useQueries({
    queries: nodeIds.map((nodeId) => ({
      queryKey: ["peer-runtime-snapshot", executionId ?? "idle", nodeId] as const,
      queryFn: () =>
        optional(
          api.get<PeerRuntime>(`/api/ui/peer/${encodeURIComponent(nodeId)}/runtime`, {
            auth: false,
          }),
          { loaded: false, reason: "rota indisponível neste nó" } as PeerRuntime,
        ),
      enabled,
      retry: false,
      staleTime: 10_000,
    })),
  });
}

/** RTT direcional das arestas da execução, consultado apenas após o stream. */
export function useChatTelemetryNetwork(nodeIds: string[], enabled: boolean, executionId?: string) {
  return useQuery({
    queryKey: keys.chatTelemetry(executionId, nodeIds),
    queryFn: () =>
      api.post<ChatTelemetryNetwork>("/api/ui/chat/telemetry", { node_ids: nodeIds }),
    enabled: enabled && nodeIds.length > 1,
    retry: false,
    staleTime: Infinity,
  });
}

/** RTT direcional da topologia atual, para o mapa do Painel. */
export function useNetworkRtt(nodeIds: string[], enabled: boolean) {
  return useQuery({
    queryKey: keys.networkRtt(nodeIds),
    queryFn: () =>
      api.post<ChatTelemetryNetwork>("/api/ui/chat/telemetry", { node_ids: nodeIds }),
    enabled: enabled && nodeIds.length > 1,
    retry: false,
    refetchInterval: 15000,
    staleTime: 10000,
  });
}

export function useLocalCatalog() {
  return useQuery({
    queryKey: keys.models,
    queryFn: () => api.get<CatalogResponse>("/api/ui/models", { auth: false }),
    // A fast inventory is useful immediately, but keep asking while the node
    // is enriching GGUF headers so screens do not wait for the normal 30 s
    // catalog cadence.
    refetchInterval: (query) => {
      const data = query.state.data as CatalogResponse | undefined;
      return data?.metadata_complete === false ? 2_000 : 30_000;
    },
    staleTime: 15_000,
  });
}

export function useClusterCatalog(enabled = true) {
  return useQuery({
    queryKey: keys.clusterModels,
    queryFn: () =>
      api.get<{ peers: PeerCatalog[] }>("/api/ui/cluster/models", { auth: false }),
    enabled,
    // Each peer publishes independently. Poll faster until every successful
    // peer reports complete metadata, then return to the quiet cadence.
    refetchInterval: (query) => {
      const data = query.state.data as { peers?: PeerCatalog[] } | undefined;
      return data?.peers?.some((peer) => peer.metadata_complete === false)
        ? 2_000
        : 60_000;
    },
    staleTime: 30_000,
  });
}

export function usePaths() {
  return useQuery({
    queryKey: keys.paths,
    queryFn: () => api.get<PathsResponse>("/api/ui/paths", { auth: false }),
    staleTime: 30_000,
  });
}

export function useCacheTypes() {
  return useQuery({
    queryKey: keys.cacheTypes,
    queryFn: () => api.get<CacheTypesResponse>("/api/ui/cache-types", { auth: false }),
    staleTime: 30_000,
  });
}

/**
 * Slicing and apply runs still in flight.
 *
 * This is the reason the notification menu survives a reload: the work lives on
 * the node, and the browser only asks what is running.
 */
export function useJobs() {
  return useQuery({
    queryKey: keys.jobs,
    queryFn: () =>
      optional(
        api.getConditional<{ jobs: Job[] }>("/api/ui/jobs", { auth: false }),
        { jobs: [] },
      ),
    refetchInterval: (query) => jobsPollInterval(query.state.data),
  });
}

/**
 * As implantações do cluster.
 *
 * Intervalo maior que o do painel: a lista muda quando alguém implanta ou
 * desimplanta, não a cada batida do coração, e cada chamada aqui é um fan-out
 * do nó para todos os peers.
 */
export function useDeployments() {
  return useQuery({
    queryKey: keys.deployments,
    queryFn: () => optional(listDeployments(), { node_id: "", deployments: [] }),
    refetchInterval: 10000,
  });
}

export function useTransfers() {
  return useQuery({
    queryKey: keys.transfers,
    queryFn: () =>
      optional(api.get<{ jobs: TransferJob[] }>("/api/transfer/jobs"), { jobs: [] }),
    refetchInterval: 2000,
  });
}

/**
 * Ofertas de fatia esperando uma resposta.
 *
 * Na mesma cadência das transferências, e não mais lenta: uma oferta é uma
 * pergunta parada esperando por um humano, não uma barra de progresso. Enquanto
 * o remetente hasheava o arquivo antes de oferecer, este intervalo era ruído
 * (medidos 3 min 23 s de MD5 numa fatia de 4,2 GB contra 5 s de polling); com a
 * oferta saindo em milissegundos, o que sobra da espera é justamente isto.
 */
export function useOffers() {
  return useQuery({
    queryKey: keys.offers,
    queryFn: () =>
      optional(
        api.getConditional<{ offers: TransferOffer[] }>("/api/transfer/offers"),
        { offers: [] },
      ),
    refetchInterval: (query) => offersPollInterval(query.state.data),
  });
}

export function useAdminNodes() {
  return useQuery({
    queryKey: keys.adminNodes,
    queryFn: () => api.get<AdminNode[]>("/admin/nodes"),
    refetchInterval: 8000,
  });
}

export function useApiKeys() {
  return useQuery({
    queryKey: keys.apiKeys,
    queryFn: () => api.get<ApiKey[]>("/admin/api-keys"),
  });
}

export function useRequestLog(
  params: {
    node_id?: string;
    api_key?: string;
    prompt?: string;
    from?: string;
    to?: string;
    limit?: number;
    offset?: number;
  } = {},
) {
  const search = new URLSearchParams();
  if (params.node_id) search.set("node_id", params.node_id);
  if (params.api_key) search.set("api_key", params.api_key);
  if (params.prompt) search.set("prompt", params.prompt);
  if (params.from) search.set("from", params.from);
  if (params.to) search.set("to", params.to);
  if (params.limit !== undefined) search.set("limit", String(params.limit));
  if (params.offset !== undefined) search.set("offset", String(params.offset));
  const qs = search.toString();
  return useQuery({
    queryKey: [...keys.requests, qs],
    queryFn: () =>
      optional(
        api.get<RequestLogResponse>(
          `/api/ui/requests${qs ? `?${qs}` : ""}`,
        ),
        {
          entries: [],
          total: 0,
          errors: 0,
          incomplete: 0,
          by_token: {},
          by_node: {},
          storage: "unavailable",
        },
      ),
    refetchInterval: 3000,
  });
}

export function useRequestStats(
  params: {
    node_id?: string;
    api_key?: string;
    prompt?: string;
    from?: string;
    to?: string;
  } = {},
  enabled = true,
) {
  const search = new URLSearchParams();
  if (params.node_id) search.set("node_id", params.node_id);
  if (params.api_key) search.set("api_key", params.api_key);
  if (params.prompt) search.set("prompt", params.prompt);
  if (params.from) search.set("from", params.from);
  if (params.to) search.set("to", params.to);
  const qs = search.toString();
  return useQuery({
    queryKey: [...keys.requestStats, qs],
    enabled,
    queryFn: () => api.get<RequestStats>(`/api/ui/requests/stats${qs ? `?${qs}` : ""}`),
    refetchInterval: enabled ? 30_000 : false,
  });
}

export function useServableModels() {
  return useQuery({
    queryKey: keys.openaiModels,
    queryFn: () =>
      api.get<{ data: Array<{ id: string }> }>("/v1/models").then((r) => r.data ?? []),
    staleTime: 20_000,
    refetchInterval: 30_000,
  });
}

export function useRemoteModelSearch(options: {
  query: string;
  ggufOnly: boolean;
  page: number;
  pageSize: number;
  sort?: "relevance" | "downloads" | "recent";
  enabled?: boolean;
}) {
  return useQuery({
    queryKey: [
      ...keys.remoteModels(options.query, options.ggufOnly, options.page),
      options.sort ?? "relevance",
    ],
    queryFn: ({ signal }) => searchRemoteModels(options, signal),
    enabled: options.enabled !== false && options.query.trim().length > 0,
    staleTime: 15_000,
    retry: false,
  });
}

export function useRemoteModelDetails(repoId: string | null, revision?: string) {
  return useQuery({
    queryKey: keys.remoteModelDetails(repoId ?? "", revision),
    queryFn: ({ signal }) => getRemoteModelDetails(repoId ?? "", revision, signal),
    enabled: Boolean(repoId),
    staleTime: 60_000,
    retry: false,
  });
}

export function useRemoteModelStorage() {
  return useQuery({
    queryKey: keys.remoteModelStorage,
    queryFn: ({ signal }) => getRemoteModelStorage(signal),
    staleTime: 15_000,
    retry: false,
  });
}

export function useRemoteModelProviderStatus() {
  return useQuery({
    queryKey: keys.remoteModelProviderStatus,
    queryFn: ({ signal }) => getRemoteModelProviderStatus(signal),
    staleTime: 30_000,
    retry: false,
  });
}

export function useHuggingFaceTokenConfig() {
  return useQuery({
    queryKey: keys.huggingFaceTokenConfig,
    queryFn: ({ signal }) => getHuggingFaceTokenConfig(signal),
    staleTime: 30_000,
    retry: false,
  });
}

export function useModelDownloads() {
  const jobs = useJobs();
  return {
    ...jobs,
    data: modelDownloadJobs(jobs.data?.jobs ?? []),
  };
}

/** After an action lands, the screens that could have changed refetch at once. */
export function invalidateAfterAction(qc: QueryClient) {
  for (const key of [
    keys.node,
    keys.cluster,
    keys.jobs,
    keys.adminNodes,
    keys.models,
    keys.remoteModelProviderStatus,
    keys.huggingFaceTokenConfig,
  ]) {
    void qc.invalidateQueries({ queryKey: key });
  }
}

export function useInvalidate() {
  const qc = useQueryClient();
  return () => invalidateAfterAction(qc);
}
