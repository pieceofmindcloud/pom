/**
 * Every read the panel does, in one place.
 *
 * Polling intervals are deliberately unequal: the cluster view fans out to each
 * peer's own `/api/ui/node`, and the catalog can walk a slow volume, so they are
 * not asked for at the same cadence as a progress bar. React Query pauses
 * refetching while the tab is hidden, which keeps an open panel from loading a
 * node nobody is watching.
 */
import { useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { ApiError, api, setConnectionKey } from "./api";
import type {
  AdminNode,
  ApiKey,
  CatalogResponse,
  ClusterView,
  Job,
  NodeInfo,
  PathsResponse,
  PeerCatalog,
  RequestLogEntry,
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
  cluster: ["cluster"] as const,
  models: ["models"] as const,
  clusterModels: ["cluster-models"] as const,
  paths: ["paths"] as const,
  jobs: ["jobs"] as const,
  transfers: ["transfers"] as const,
  offers: ["offers"] as const,
  adminNodes: ["admin-nodes"] as const,
  apiKeys: ["api-keys"] as const,
  requests: ["requests"] as const,
  openaiModels: ["openai-models"] as const,
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
    queryFn: () => api.get<ClusterView>("/api/ui/cluster", { auth: false }),
    refetchInterval: 5000,
  });
}

export function useLocalCatalog() {
  return useQuery({
    queryKey: keys.models,
    queryFn: () => api.get<CatalogResponse>("/api/ui/models", { auth: false }),
    refetchInterval: 30_000,
    staleTime: 15_000,
  });
}

export function useClusterCatalog(enabled = true) {
  return useQuery({
    queryKey: keys.clusterModels,
    queryFn: () =>
      api.get<{ peers: PeerCatalog[] }>("/api/ui/cluster/models", { auth: false }),
    enabled,
    refetchInterval: 60_000,
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
      optional(api.get<{ jobs: Job[] }>("/api/ui/jobs", { auth: false }), { jobs: [] }),
    refetchInterval: 2000,
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
      optional(api.get<{ offers: TransferOffer[] }>("/api/transfer/offers"), {
        offers: [],
      }),
    refetchInterval: 2000,
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

export function useRequestLog(params: { node_id?: string; token?: string } = {}) {
  const search = new URLSearchParams();
  if (params.node_id) search.set("node_id", params.node_id);
  if (params.token) search.set("token", params.token);
  const qs = search.toString();
  return useQuery({
    queryKey: [...keys.requests, qs],
    queryFn: () =>
      optional(
        api.get<{ entries: RequestLogEntry[]; total: number }>(
          `/api/ui/requests${qs ? `?${qs}` : ""}`,
        ),
        { entries: [], total: 0 },
      ),
    refetchInterval: 3000,
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

/** After an action lands, the screens that could have changed refetch at once. */
export function invalidateAfterAction(qc: QueryClient) {
  for (const key of [
    keys.node,
    keys.cluster,
    keys.jobs,
    keys.adminNodes,
    keys.models,
  ]) {
    void qc.invalidateQueries({ queryKey: key });
  }
}

export function useInvalidate() {
  const qc = useQueryClient();
  return () => invalidateAfterAction(qc);
}
