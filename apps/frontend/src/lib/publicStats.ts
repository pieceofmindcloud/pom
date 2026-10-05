import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import type { NetworkStats } from "./networkStats";

/**
 * Models, node count and total VRAM of a network this machine is **not** in,
 * when that network chose to publish them. The node asks the network's
 * principal by its key (`GET <endpoint>?key=`); the browser never talks to
 * the other network. Latency is not part of it: nobody measured a route to
 * a network that was not joined.
 */
export type PublicStatsEndpoint = "/api/ui/network-chat/lobby/info" | "/api/setup/lobby/info";

interface InfoResponse {
  accepted: boolean;
  error: string | null;
  info: { models: string[]; node_count: number; vram_total_mb: number | null } | null;
}

const CACHE_TTL_MS = 30_000;
/** The principal computes on demand: the first ask may come back empty. */
const WARMUP_RETRY_MS = 3_000;

const cache = new Map<string, { stats: NetworkStats | null; at: number }>();

export function resetPublicStatsCache() {
  cache.clear();
}

async function ask(endpoint: PublicStatsEndpoint, key: string): Promise<{ stats: NetworkStats | null; warming: boolean }> {
  try {
    const response = await api.get<InfoResponse>(`${endpoint}?key=${encodeURIComponent(key)}`, { auth: endpoint !== "/api/setup/lobby/info" });
    if (!response.accepted) return { stats: null, warming: false };
    if (!response.info) return { stats: null, warming: true };
    return {
      stats: {
        nodeCount: response.info.node_count,
        averageLatencyMs: null,
        vramTotalMb: response.info.vram_total_mb,
        models: response.info.models,
      },
      warming: false,
    };
  } catch {
    return { stats: null, warming: false };
  }
}

/** `null` until answered, and when the network does not answer or publish. */
export function usePublicStats(endpoint: PublicStatsEndpoint, key: string, enabled: boolean, refreshKey = 0): NetworkStats | null {
  const [stats, setStats] = useState<NetworkStats | null>(null);
  const previousRefreshKey = useRef(refreshKey);

  useEffect(() => {
    const force = previousRefreshKey.current !== refreshKey;
    previousRefreshKey.current = refreshKey;
    if (!enabled || !key) {
      setStats(null);
      return;
    }
    const cacheKey = `${endpoint}|${key}`;
    const cached = cache.get(cacheKey);
    if (!force && cached && Date.now() - cached.at < CACHE_TTL_MS) {
      setStats(cached.stats);
      return;
    }
    let active = true;
    let timer: number | undefined;
    const run = (retry: boolean) => {
      void ask(endpoint, key).then((answer) => {
        if (!active) return;
        if (answer.warming && retry) {
          timer = window.setTimeout(() => run(false), WARMUP_RETRY_MS);
          return;
        }
        cache.set(cacheKey, { stats: answer.stats, at: Date.now() });
        setStats(answer.stats);
      });
    };
    run(true);
    return () => {
      active = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [enabled, endpoint, key, refreshKey]);

  return stats;
}
