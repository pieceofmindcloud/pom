import { useEffect, useRef, useState } from "react";

export type PublicReachability = "checking" | "reachable" | "unreachable" | "ipv6-unavailable";

const PROBE_TIMEOUT_MS = 1800;
const CACHE_TTL_MS = 30_000;

// Endereço IPv6 literal, público e estável (DNS público do Google), usado
// somente para descobrir se esta máquina tem rota IPv6 - nunca para
// verificar o peer. Sem isso, um navegador sem IPv6 marcava todo peer
// anunciado por IPv6 como "unreachable" mesmo quando o peer está no ar.
const IPV6_PROBE_URL = "http://[2001:4860:4860::8888]/";
const IPV6_PROBE_TIMEOUT_MS = 1200;
const IPV6_CONNECTIVITY_CACHE_TTL_MS = 5 * 60_000;

interface ReachabilityCacheEntry {
  status: Exclude<PublicReachability, "checking">;
  checkedAt: number;
  pending?: Promise<Exclude<PublicReachability, "checking">>;
}

const cache = new Map<string, ReachabilityCacheEntry>();

let ipv6ConnectivityCache: { hasIpv6: boolean; checkedAt: number } | null = null;
let ipv6ConnectivityPending: Promise<boolean> | null = null;

export function resetPublicAddressReachabilityCache() {
  cache.clear();
  ipv6ConnectivityCache = null;
  ipv6ConnectivityPending = null;
}

export function isIpv6Hostname(hostname: string): boolean {
  return hostname.startsWith("[") || hostname.includes(":");
}

export function isIpv6Address(address: string): boolean {
  const url = healthUrlForPublicAddress(address);
  if (!url) return false;
  try {
    return isIpv6Hostname(new URL(url).hostname);
  } catch {
    return false;
  }
}

export async function hasIpv6Connectivity(
  fetcher: typeof fetch = globalThis.fetch,
): Promise<boolean> {
  const fresh = ipv6ConnectivityCache && Date.now() - ipv6ConnectivityCache.checkedAt < IPV6_CONNECTIVITY_CACHE_TTL_MS;
  if (fresh) return ipv6ConnectivityCache!.hasIpv6;
  if (ipv6ConnectivityPending) return ipv6ConnectivityPending;

  const controller = new AbortController();
  const timeout = window.setTimeout(() => controller.abort(), IPV6_PROBE_TIMEOUT_MS);
  const pending = fetcher(IPV6_PROBE_URL, {
    method: "GET",
    mode: "no-cors",
    cache: "no-store",
    signal: controller.signal,
  })
    .then(() => true)
    .catch(() => false)
    .finally(() => {
      window.clearTimeout(timeout);
    });
  ipv6ConnectivityPending = pending;
  const hasIpv6 = await pending;
  ipv6ConnectivityCache = { hasIpv6, checkedAt: Date.now() };
  ipv6ConnectivityPending = null;
  return hasIpv6;
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
    // A falha pode ser o peer fora do ar ou esta máquina sem rota IPv6 - só
    // um endereço IPv6 sem conectividade local vira "ipv6-unavailable"; o
    // resto continua "unreachable".
    if (isIpv6Hostname(new URL(url).hostname) && !(await hasIpv6Connectivity(fetcher))) {
      return "ipv6-unavailable";
    }
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
