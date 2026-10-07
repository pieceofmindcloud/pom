import { useEffect, useRef, useState } from "react";
import { api } from "./api";

/**
 * How a public network can be reached, according to the pkarr record of the
 * publisher's key. The node does the resolving
 * (`GET /api/public/networks/locate`): the browser does not speak libp2p and
 * never receives anyone's IP. `unknown` is a check that could not be made (the
 * node refused the key or did not answer): it says nothing about the network,
 * so it is never shown as a network missing from the DHT.
 */
export type PublicLocator = "checking" | "direct" | "relayed" | "unresolved" | "unknown";

interface LocatorResponse {
  reachability: Exclude<PublicLocator, "checking" | "unknown">;
  relays: number;
}

const CACHE_TTL_MS = 30_000;

interface CacheEntry {
  status: Exclude<PublicLocator, "checking">;
  checkedAt: number;
  pending?: Promise<Exclude<PublicLocator, "checking">>;
}

const cache = new Map<string, CacheEntry>();

export function resetPublicLocatorCache() {
  cache.clear();
}

export async function fetchPublicLocator(key: string): Promise<Exclude<PublicLocator, "checking">> {
  if (!key.trim()) return "unresolved";
  try {
    const response = await api.get<LocatorResponse>(
      `/api/public/networks/locate?key=${encodeURIComponent(key)}`,
      { auth: false },
    );
    return response.reachability;
  } catch {
    return "unknown";
  }
}

export function publicLocator(
  key: string,
  { force = false }: { force?: boolean } = {},
): Promise<Exclude<PublicLocator, "checking">> {
  const current = cache.get(key);
  const fresh = current && Date.now() - current.checkedAt < CACHE_TTL_MS;
  if (!force && current?.pending) return current.pending;
  if (!force && fresh) return Promise.resolve(current.status);

  const pending = fetchPublicLocator(key);
  cache.set(key, { status: "unresolved", checkedAt: Date.now(), pending });
  void pending.then((status) => {
    const entry = cache.get(key);
    if (entry?.pending === pending) cache.set(key, { status, checkedAt: Date.now() });
  });
  return pending;
}

export function usePublicLocator(key: string, refreshKey = 0): PublicLocator {
  const [status, setStatus] = useState<PublicLocator>("checking");
  const previousRefreshKey = useRef(refreshKey);

  useEffect(() => {
    let active = true;
    const force = previousRefreshKey.current !== refreshKey;
    previousRefreshKey.current = refreshKey;
    setStatus("checking");
    void publicLocator(key, { force }).then((next) => {
      if (active) setStatus(next);
    });
    return () => {
      active = false;
    };
  }, [key, refreshKey]);

  return status;
}
