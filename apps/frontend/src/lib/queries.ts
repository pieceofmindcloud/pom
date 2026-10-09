/**
 * Every read the panel does, in one place.
 *
 * Polling intervals are deliberately unequal: the cluster view fans out to each
 * peer's own `/api/ui/node`, and the catalog can walk a slow volume, so they are
 * not asked for at the same cadence as a progress bar. React Query pauses
 * refetching while the tab is hidden, which keeps an open panel from loading a
 * node nobody is watching.
 */
import type { ActivePluginUi } from "./pluginUi";
import { useQueries, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import {
  ApiError,
  api,
  onAdminSessionLost,
  getRemoteModelDetails,
  getRemoteModelProviderStatus,
  getRemoteModelStorage,
  getHuggingFaceTokenConfig,
  listDeployments,
  searchRemoteModels,
  setAdminAccessToken,
  setConnectionKey,
} from "./api";

/**
 * Administrative password written on first access (`auth::DEFAULT_ADMIN_PASSWORD`
 * on the node). It is not a secret: while it is in force, `/api/access` says
 * `default_password` and the interface enters without asking for it and asks for a change.
 */
export const DEFAULT_ADMIN_PASSWORD = "admin";
import { modelDownloadJobs } from "./modelProvider";
import { getTrendingModels } from "./publicModelDiscovery";
import { fetchPublicPlugin, fetchPublicPluginCatalog, type PublicPluginCatalogQuery } from "./pluginCatalog";
import { jobsPollInterval, offersPollInterval } from "./polling";
import type {
  SidecarLink,
  ChatCapabilities,
  AdminNode,
  AdminAccessInfo,
  AdminAccessStatus,
  AdminAuthorityView,
  ApiKey,
  CacheTypesResponse,
  CatalogResponse,
  ChatTelemetryNetwork,
  ClusterView,
  ClusterLogResponse,
  InferenceHeadsResponse,
  Job,
  NodeInfo,
  NetworkInfoView,
  NetworkChatRoom,
  NetworkChatSnapshot,
  NetworkOffer,
  PathsResponse,
  PeerCatalog,
  PeerRuntime,
  TransferJob,
  TrustAnchorView,
  WindowsFirewallStatus,
  PluginStatus,
  GithubRepository,
  DevelopmentPluginSources,
  PomCoreUpdateStatus,
} from "./types";
import { formatMessage, getActiveLocale } from "../i18n/locales";

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
  chatCapabilities: ["chat-capabilities"] as const,
  models: ["models"] as const,
  clusterModels: ["cluster-models"] as const,
  sidecarLinks: ["model-sidecar-links"] as const,
  paths: ["paths"] as const,
  cacheTypes: ["cache-types"] as const,
  jobs: ["jobs"] as const,
  transfers: ["transfers"] as const,
  offers: ["offers"] as const,
  adminAuthority: ["admin-authority"] as const,
  trustAnchor: ["trust-anchor"] as const,
  networkInfo: ["network-info"] as const,
  windowsFirewall: ["windows-firewall"] as const,
  networkChat: ["network-chat"] as const,
  adminNodes: ["admin-nodes"] as const,
  apiKeys: ["api-keys"] as const,
  plugins: ["plugins"] as const,
  githubRepositories: ["github-repositories"] as const,
  developmentPluginSources: ["development-plugin-sources"] as const,
  publicPluginCatalog: ["public-plugin-catalog"] as const,
  pomCoreUpdate: ["pom-core-update"] as const,
  peerRuntime: (nodeId: string) => ["peer-runtime", nodeId] as const,
  chatTelemetry: (executionId: string | undefined, nodeIds: string[]) => ["chat-telemetry", executionId ?? "idle", ...nodeIds] as const,
  networkRtt: (nodeIds: string[]) => ["network-rtt", ...nodeIds] as const,
  inferenceHeads: (model: string) => ["inference-heads", model] as const,
  openaiModels: ["openai-models"] as const,
  deployments: ["deployments"] as const,
  pluginUi: ["plugin-ui"] as const,
  clusterLogs: (deployId: string | undefined) => ["cluster-logs", deployId ?? "all"] as const,
  remoteModels: (query: string, ggufOnly: boolean, page: number) =>
    ["remote-models", query, ggufOnly, page] as const,
  remoteModelDetails: (repoId: string, revision: string | undefined) =>
    ["remote-model-details", repoId, revision ?? "default"] as const,
  remoteModelStorage: ["remote-model-storage"] as const,
  remoteModelProviderStatus: ["remote-model-provider-status"] as const,
  huggingFaceTokenConfig: ["hugging-face-token-config"] as const,
};

export type AdminSurfaceState = "available" | "requires-password" | "local-only";

/**
 * One probe decides which shell is safe to render. A valid server session
 * makes `/api/ui/node` succeed after a reload, so the password gate does not
 * return merely because React state was recreated by F5.
 */
export function useAdminSurface() {
  const client = useQueryClient();
  /*
   * The probe decides which shell is safe to draw and runs once per
   * load (`staleTime: Infinity`). But the administrative session lives
   * in the node's memory: a restart, or the 8 h of validity, drops it with the tab
   * open. Without redoing the probe, each screen started printing the raw
   * "missing administrative access password" in place of its content, and the
   * operator had no way back to the gate other than reloading by
   * hand.
   */
  useEffect(
    () =>
      onAdminSessionLost(() => {
        // Only a tab that believed it had a session probes again. The probe itself
        // answers 401 while there is no session, and that 401 also signals
        // "session lost": without this filter each answer asked for another probe,
        // thousands per minute, and the gate remounted nonstop.
        if (client.getQueryData(keys.adminSurface) !== "available") return;
        if (client.isFetching({ queryKey: keys.adminSurface }) > 0) return;
        void client.invalidateQueries({ queryKey: keys.adminSurface });
      }),
    [client],
  );
  return useQuery<AdminSurfaceState>({
    queryKey: keys.adminSurface,
    queryFn: async () => {
      try {
        await api.get<unknown>("/api/ui/node", { auth: false });
        return "available";
      } catch (error) {
        let access: AdminAccessStatus | null = null;
        try {
          access = await api.get<AdminAccessStatus>("/api/access", { auth: false });
        } catch {
          // A transient policy read must not expose a guessed UI state.
        }
        if (error instanceof ApiError && error.status === 403 && access?.admin_exposure === "localhost") {
          return "local-only";
        }
        // While the password is the first-access default, the gate does not
        // ask for it: the tab enters with it and the notice asks for the change.
        if (access?.default_password) {
          setAdminAccessToken(DEFAULT_ADMIN_PASSWORD);
          try {
            await api.get<unknown>("/api/ui/node", { auth: false });
            return "available";
          } catch {
            setAdminAccessToken("");
          }
        }
        return "requires-password";
      }
    },
    retry: false,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
}

function inactivePlugin(code: string): PluginStatus {
  return {
    code,
    active: false,
    development_install: false,
    state: "needs_activation",
    features: [],
    manage_url: "https://pieceofmind.cloud/plugins/",
    plugin_version: null,
    lease_expires_at: null,
    license_expires_at: null,
    activation_count: null,
    max_activations: null,
    licensed_plugins: [],
    plugin_usage: [],
    activation_candidates: [],
    installed_at: null,
    update_checked_at: null,
    available_update: null,
  };
}

/**
 * The whole plugin catalog, active or not - what the Plugins screen lists.
 * A missing endpoint (older node) is treated as an empty catalog, fail-closed.
 */
export function usePlugins() {
  return useQuery<PluginStatus[]>({
    queryKey: keys.plugins,
    queryFn: () => optional(api.get<PluginStatus[]>("/api/ui/plugins"), []),
    retry: false,
    refetchInterval: 30_000,
  });
}

/** Repository registrations are local to this POM node, never read from the license catalog. */
export function useGithubRepositories() {
  return useQuery<GithubRepository[]>({
    queryKey: keys.githubRepositories,
    queryFn: () => optional(api.get<GithubRepository[]>("/api/ui/plugins/github-repositories"), []),
    retry: false,
    refetchInterval: 60_000,
  });
}

/** Local mounted plugin sources are queried only by the development admin view. */
export function useDevelopmentPluginSources(enabled: boolean) {
  return useQuery<DevelopmentPluginSources>({
    queryKey: keys.developmentPluginSources,
    queryFn: () => api.get<DevelopmentPluginSources>("/api/ui/plugins/dev-sources"),
    enabled,
    retry: false,
    refetchInterval: 60_000,
  });
}

/**
 * Public store metadata is fetched directly without node credentials or cookies.
 * Without a query it is the whole catalog; with one, the license server searches
 * and pages it, and the previous page stays on screen while the next one loads.
 */
export function usePublicPluginCatalog(query?: PublicPluginCatalogQuery) {
  return useQuery({
    queryKey: query ? [...keys.publicPluginCatalog, query] : keys.publicPluginCatalog,
    queryFn: ({ signal }) => fetchPublicPluginCatalog(signal, query),
    retry: false,
    staleTime: 60_000,
    refetchInterval: 60_000,
    placeholderData: query ? (previous) => previous : undefined,
  });
}

/** One public store entry by plugin code (null when the store does not list it). */
export function usePublicPlugin(code: string) {
  return useQuery({
    queryKey: [...keys.publicPluginCatalog, "entry", code],
    queryFn: ({ signal }) => fetchPublicPlugin(code, signal),
    retry: false,
    staleTime: 60_000,
  });
}

/**
 * UI manifests of the active plugins (`pom-plugin-ui/v1`). An older node, an
 * inactive plugin or a failed request all mean "no plugin UI": the POM keeps
 * showing only its advertising fallbacks.
 */
export function usePluginUi() {
  return useQuery<{ plugins: ActivePluginUi[] }>({
    queryKey: keys.pluginUi,
    queryFn: () => optional(api.get<{ plugins: ActivePluginUi[] }>("/api/ui/plugins/ui", { auth: false }), { plugins: [] }),
    retry: false,
    refetchInterval: 30_000,
  });
}

/**
 * The `"logs"` plugin's status specifically (Request Logs/Statistics).
 * Built on `usePlugins()` so callers gating those two screens do not need to
 * know about the rest of the catalog.
 */
export function useEnterpriseStatus() {
  const query = usePlugins();
  const status = query.data?.find((plugin) => plugin.code === "logs") ?? inactivePlugin("logs");

  return { ...query, data: status };
}

/**
 * The `pom` binary's own update status - never mixed with plugin updates
 * (see the header of `crates/node/src/pom_core_update.rs`).
 */
export function usePomCoreUpdateStatus() {
  return useQuery<PomCoreUpdateStatus>({
    queryKey: keys.pomCoreUpdate,
    queryFn: () =>
      optional(api.get<PomCoreUpdateStatus>("/api/ui/pom-update/status", { auth: false }), {
        core_update: null,
      }),
    retry: false,
    refetchInterval: 30_000,
  });
}

/** Public metadata needed before the app knows whether to show the password gate. */
export function useAdminAccess() {
  return useQuery({
    queryKey: keys.adminAccess,
    queryFn: () => api.get<AdminAccessStatus>("/api/access", { auth: false }),
    retry: false,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
}

/** Authenticated administrative state, including only password presence for settings. */
export function useAdminAccessInfo() {
  return useQuery({
    queryKey: keys.adminAccessInfo,
    queryFn: () => api.get<AdminAccessInfo>("/api/ui/access", { auth: false }),
    retry: false,
    staleTime: 10_000,
  });
}

export function useNode(enabled = true) {
  return useQuery({
    queryKey: keys.node,
    queryFn: async () => {
      const node = await api.get<NodeInfo>("/api/ui/node", { auth: false });
      // The key travels with the node's own description; picking it up here is
      // what lets every authenticated call below just work.
      if (node.connection_key) setConnectionKey(node.connection_key);
      return node;
    },
    enabled,
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
 * Shared network-chat snapshot for the chat page and its navigation badge.
 * Keeping one query source means a message received off the chat screen is
 * visible to the menu without mounting the chat route.
 */
export function useNetworkChat(room: NetworkChatRoom = "network", enabled = true) {
  return useQuery({
    enabled,
    // The main room keeps the bare key: the menu's unread counter and the
    // plugin bridge share that cache entry.
    queryKey: room === "network" ? keys.networkChat : [...keys.networkChat, room],
    queryFn: () => api.get<NetworkChatSnapshot>(room === "network" ? "/api/ui/network-chat" : `/api/ui/network-chat?room=${room}`),
    refetchInterval: 1200,
    retry: false,
  });
}

const unavailableChatCapabilities: ChatCapabilities = {
  active_model: "",
  vision: "unavailable",
  coverage: "unavailable",
  thinking: "unavailable",
  rag: "unavailable",
};

/**
 * A refusal is not an empty cluster. The endpoint deliberately returns only
 * aggregate Chat facts, but an older node or a policy boundary can still deny
 * it. Keep that uncertainty explicit so the composer never invents a missing
 * model or missing vision support.
 */
export function useChatCapabilities() {
  return useQuery({
    queryKey: keys.chatCapabilities,
    queryFn: () =>
      api
        .getConditional<ChatCapabilities>("/api/ui/chat/capabilities", { auth: false })
        .catch(() => unavailableChatCapabilities),
    refetchInterval: 5000,
    staleTime: 2000,
  });
}

/**
 * How a machine loaded the model, fetched only with the card open.
 *
 * `enabled` is the point: this comes from the peer's `/health`, and asking per
 * peer on every Panel cycle would be a fan of calls for a screen that is almost
 * always closed. An old node without the route returns 404, and then the section
 * disappears instead of becoming an error - same criterion as the `optional` above.
 */
export function usePeerRuntime(nodeId: string, enabled: boolean) {
  return useQuery({
    queryKey: keys.peerRuntime(nodeId),
    queryFn: () =>
      optional(
        api.get<PeerRuntime>(`/api/ui/peer/${encodeURIComponent(nodeId)}/runtime`, {
          auth: false,
        }),
        { loaded: false, reason: formatMessage(getActiveLocale(), "peer.routeUnavailable") } as PeerRuntime,
      ),
    enabled,
    refetchInterval: 15_000,
    staleTime: 10_000,
  });
}

/** Health snapshot of the peers that have just taken part in an execution. */
export function usePeerRuntimes(nodeIds: string[], enabled: boolean, executionId?: string) {
  return useQueries({
    queries: nodeIds.map((nodeId) => ({
      queryKey: ["peer-runtime-snapshot", executionId ?? "idle", nodeId] as const,
      queryFn: () =>
        optional(
          api.get<PeerRuntime>(`/api/ui/peer/${encodeURIComponent(nodeId)}/runtime`, {
            auth: false,
          }),
          { loaded: false, reason: formatMessage(getActiveLocale(), "peer.routeUnavailable") } as PeerRuntime,
        ),
      enabled,
      retry: false,
      staleTime: 10_000,
    })),
  });
}

/** Directional RTT of the execution's edges, queried only after the stream. */
export function useChatTelemetryNetwork(nodeIds: string[], enabled: boolean, executionId?: string) {
  return useQuery({
    queryKey: keys.chatTelemetry(executionId, nodeIds),
    queryFn: () =>
      api.post<ChatTelemetryNetwork>("/api/ui/chat/telemetry", {
        node_ids: nodeIds,
        execution_id: executionId,
      }),
    enabled: enabled && nodeIds.length > 1,
    retry: false,
    staleTime: Infinity,
  });
}

/** Last RTT observed on the inference edges, for the Panel's map. */
export function useNetworkRtt(nodeIds: string[], enabled: boolean) {
  return useQuery({
    queryKey: keys.networkRtt(nodeIds),
    queryFn: () =>
      api.post<ChatTelemetryNetwork>("/api/ui/chat/telemetry", { node_ids: nodeIds }),
    // A whole-model deployment can have one head and no graph edge. The
    // endpoint still needs to enumerate that route and its return RTT.
    enabled,
    retry: false,
    refetchInterval: 15000,
    staleTime: 10000,
  });
}

/** Heads of the model this infer node can choose, without opening the whole network. */
export function useInferenceHeads(model: string, enabled: boolean) {
  return useQuery({
    queryKey: keys.inferenceHeads(model),
    queryFn: () =>
      api.get<InferenceHeadsResponse>(
        `/api/ui/infer/heads?model=${encodeURIComponent(model)}`,
      ),
    enabled: enabled && Boolean(model),
    retry: false,
    refetchInterval: 15_000,
    staleTime: 5_000,
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

/** Which projector and MTP draft belong to each model (kept on the principal). */
export function useSidecarLinks(enabled = true) {
  return useQuery({
    queryKey: keys.sidecarLinks,
    queryFn: () =>
      api.get<{ links: SidecarLink[]; stale: boolean }>("/api/ui/model-sidecar-links"),
    enabled,
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
 * The cluster's deployments.
 *
 * A longer interval than the panel's: the list changes when someone deploys or
 * undeploys, not on every heartbeat, and each call here is a fan-out from the
 * node to all the peers.
 */
export function useDeployments() {
  return useQuery({
    queryKey: keys.deployments,
    queryFn: () => optional(listDeployments(), { node_id: "", deployments: [] }),
    refetchInterval: 10000,
  });
}

/** Process logs in arrival order at the local replica. */
/** The cluster's process log, kept by the logs plugin; idle while it is not active here. */
export function useClusterLogs(deployId?: string, enabled = true) {
  const query = new URLSearchParams();
  if (deployId) query.set("deploy_id", deployId);
  const suffix = query.toString();
  return useQuery<ClusterLogResponse>({
    queryKey: keys.clusterLogs(deployId),
    queryFn: () =>
      api.get<ClusterLogResponse>(`/api/ui/cluster-logs${suffix ? `?${suffix}` : ""}`),
    enabled,
    refetchInterval: enabled ? 1000 : false,
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
 * Slice offers waiting for an answer.
 *
 * At the same cadence as the transfers, and not slower: an offer is a stalled
 * question waiting for a human, not a progress bar. While the sender hashed the
 * file before offering, this interval was noise (3 min 23 s of MD5 measured on a
 * 4.2 GB slice against 5 s of polling); with the offer going out in milliseconds,
 * what remains of the wait is precisely this.
 */
export function useOffers() {
  return useQuery({
    queryKey: keys.offers,
    queryFn: () =>
      optional(
        api.getConditional<{ offers: NetworkOffer[] }>("/api/transfer/offers"),
        { offers: [] },
      ).then((response) => ({
        offers: response.offers.map((offer) =>
          offer.kind ? offer : { ...offer, kind: "slice" as const },
        ),
      })),
    refetchInterval: (query) => offersPollInterval(query.state.data),
  });
}

/** Authority state is presentation data; mechanism details stay server-side. */
export function useAdminAuthority(enabled = true) {
  return useQuery({
    queryKey: keys.adminAuthority,
    queryFn: () => api.get<AdminAuthorityView>("/api/ui/network/admin-authority"),
    enabled,
    refetchInterval: 8000,
  });
}

/**
 * The anchoring state of this machine.
 *
 * No short polling: a trust root changes by an operator's gesture, not on its
 * own, and the screen rereads after installing.
 */
export function useTrustAnchor() {
  return useQuery({
    queryKey: keys.trustAnchor,
    queryFn: () => api.get<TrustAnchorView>("/api/ui/network/trust-anchor"),
  });
}

export function useNetworkInfo(enabled = true) {
  return useQuery({
    queryKey: keys.networkInfo,
    queryFn: () => api.get<NetworkInfoView>("/api/ui/network/info"),
    enabled,
    refetchInterval: 10_000,
  });
}

export function useWindowsFirewall(enabled = true) {
  return useQuery({
    queryKey: keys.windowsFirewall,
    queryFn: () => api.get<WindowsFirewallStatus>("/api/ui/network/firewall"),
    enabled,
    refetchInterval: 5000,
  });
}

export function useAdminNodes(enabled = true) {
  return useQuery({
    queryKey: keys.adminNodes,
    queryFn: () => api.get<AdminNode[]>("/admin/nodes"),
    enabled,
    refetchInterval: 8000,
  });
}

export function useApiKeys() {
  return useQuery({
    queryKey: keys.apiKeys,
    queryFn: () => api.get<ApiKey[]>("/admin/api-keys"),
  });
}

export function useServableModels() {
  return useQuery({
    queryKey: keys.openaiModels,
    queryFn: () =>
      api.get<{
        data: Array<{
          id: string;
          context_length?: number | null;
          max_input_tokens?: number | null;
          max_tokens?: number | null;
        }>;
      }>("/v1/models").then((r) => r.data ?? []),
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

export function useTrendingPublicModels() {
  return useQuery({
    queryKey: ["public-trending-models", "hugging-face", "gguf"],
    queryFn: ({ signal }) => getTrendingModels(signal),
    staleTime: 5 * 60_000,
    retry: 1,
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
    keys.offers,
    keys.adminAuthority,
    keys.networkInfo,
    keys.windowsFirewall,
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
