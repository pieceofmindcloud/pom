import { useEffect, useRef, useState } from "react";
import { api } from "./api";

/**
 * Round trip from this node to the node that publishes a network it is not
 * in, measured by the node over the lobby's keyed connection
 * (`GET /api/ui/network-chat/lobby/latency?key=`). The browser never talks to
 * the other network, so this is the latency the node would see, not the
 * browser's. `failed` is a network that did not answer.
 */
export type PublicLatency = number | "checking" | "failed";

const CACHE_TTL_MS = 60_000;
/** Each measurement dials another network; a few at a time is enough. */
const CONCURRENCY = 3;

const cache = new Map<string, { value: number | "failed"; at: number }>();

export function resetPublicLatencyCache() {
  cache.clear();
}

async function measure(key: string): Promise<number | "failed"> {
  try {
    const response = await api.get<{ rtt_ms: number }>(`/api/ui/network-chat/lobby/latency?key=${encodeURIComponent(key)}`);
    return typeof response.rtt_ms === "number" ? response.rtt_ms : "failed";
  } catch {
    return "failed";
  }
}

/**
 * Latency per publisher key. Nothing is measured until `enabled`: measuring
 * opens a connection to every listed network, so the screen asks for it only
 * when the operator wants to compare them.
 */
export function usePublicLatencies(keys: string[], enabled: boolean, refreshKey = 0): Record<string, PublicLatency> {
  const [latencies, setLatencies] = useState<Record<string, PublicLatency>>({});
  const previousRefreshKey = useRef(refreshKey);
  const wanted = Array.from(new Set(keys.filter(Boolean))).sort().join("\n");

  useEffect(() => {
    const force = previousRefreshKey.current !== refreshKey;
    previousRefreshKey.current = refreshKey;
    if (!enabled || !wanted) return;
    let active = true;
    const now = Date.now();
    const known: Record<string, PublicLatency> = {};
    const queue: string[] = [];
    for (const key of wanted.split("\n")) {
      const cached = cache.get(key);
      if (!force && cached && now - cached.at < CACHE_TTL_MS) known[key] = cached.value;
      else {
        known[key] = "checking";
        queue.push(key);
      }
    }
    setLatencies(known);
    const worker = async () => {
      for (let key = queue.shift(); key !== undefined; key = queue.shift()) {
        const value = await measure(key);
        cache.set(key, { value, at: Date.now() });
        if (!active) return;
        const measured = key;
        setLatencies((current) => ({ ...current, [measured]: value }));
      }
    };
    void Promise.all(Array.from({ length: Math.min(CONCURRENCY, queue.length) }, worker));
    return () => {
      active = false;
    };
  }, [enabled, wanted, refreshKey]);

  return enabled ? latencies : {};
}
