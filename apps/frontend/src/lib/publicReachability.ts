import { useEffect, useRef, useState } from "react";

export type PublicReachability = "checking" | "reachable" | "unreachable";

const PROBE_TIMEOUT_MS = 1800;
const CACHE_TTL_MS = 30_000;

interface ReachabilityCacheEntry {
  status: Exclude<PublicReachability, "checking">;
  checkedAt: number;
  pending?: Promise<Exclude<PublicReachability, "checking">>;
}

const cache = new Map<string, ReachabilityCacheEntry>();

export function resetPublicAddressReachabilityCache() {
  cache.clear();
}

export function healthUrlForPublicAddress(address: string): string | null {
  const raw = address.trim();
  if (!raw) return null;
  const withProtocol = /^[a-z][a-z\d+.-]*:\/\//i.test(raw) ? raw : `http://${raw}`;
  try {
    const url = new URL(withProtocol);
    url.pathname = "/health";
    url.search = "";
    url.hash = "";
    return url.toString();
  } catch {
    return null;
  }
}

export async function probePublicAddress(
  address: string,
  fetcher: typeof fetch = globalThis.fetch,
): Promise<Exclude<PublicReachability, "checking">> {
  const url = healthUrlForPublicAddress(address);
  if (!url) return "unreachable";

  const controller = new AbortController();
  const timeout = window.setTimeout(() => controller.abort(), PROBE_TIMEOUT_MS);
  try {
    const response = await fetcher(url, {
      method: "GET",
      mode: "no-cors",
      cache: "no-store",
      signal: controller.signal,
    });
    // Cross-origin health checks are opaque by design. A response of any kind
    // still proves that the published endpoint answered; same-origin responses
    // additionally expose the normal 2xx status.
    return response.ok || response.type === "opaque" ? "reachable" : "unreachable";
  } catch {
    return "unreachable";
  } finally {
    window.clearTimeout(timeout);
  }
}

export function publicAddressReachability(
  address: string,
  { force = false }: { force?: boolean } = {},
): Promise<Exclude<PublicReachability, "checking">> {
  const key = address.trim();
  const current = cache.get(key);
  const fresh = current && Date.now() - current.checkedAt < CACHE_TTL_MS;
  if (!force && current?.pending) return current.pending;
  if (!force && fresh) return Promise.resolve(current.status);

  const pending = probePublicAddress(address);
  cache.set(key, { status: "unreachable", checkedAt: Date.now(), pending });
  void pending.then((status) => {
    const entry = cache.get(key);
    if (entry?.pending === pending) cache.set(key, { status, checkedAt: Date.now() });
  });
  return pending;
}

export function usePublicAddressReachability(address: string, refreshKey = 0): PublicReachability {
  const [status, setStatus] = useState<PublicReachability>("checking");
  const previousRefreshKey = useRef(refreshKey);

  useEffect(() => {
    let active = true;
    const force = previousRefreshKey.current !== refreshKey;
    previousRefreshKey.current = refreshKey;
    setStatus("checking");
    void publicAddressReachability(address, { force }).then((next) => {
      if (active) setStatus(next);
    });
    return () => {
      active = false;
    };
  }, [address, refreshKey]);

  return status;
}
