// The version of the trust chain a node holds, as the node reports it:
// `<transfers>.<grants>.<demotions>.<open transfer>-<content hash>`
// (`AdminTrust::chain_version`). The counters order two versions; the hash
// tells apart two the counters call equal.

import { api } from "./api";

export interface ClusterVersionStatus {
  version: string;
  /** A peer announces a newer chain than this node holds. */
  outdated: boolean;
  /** Hosts announcing that newer chain. */
  newer_on: string[];
  /** Only in the answer to a sync: the chain changed. */
  updated?: boolean;
}

/** `1.4.2 · 9f3c21ab`: the counters and the start of the hash. */
export function formatClusterVersion(version: string | null | undefined): string {
  if (!version) return "";
  const [counters, hash = ""] = version.split("-");
  const numbers = counters.split(".").slice(0, 3).join(".");
  return hash ? `${numbers} · ${hash.slice(0, 8)}` : numbers;
}

export const clusterVersionApi = {
  sync: () => api.post<ClusterVersionStatus>("/api/ui/network/trust-sync"),
};
