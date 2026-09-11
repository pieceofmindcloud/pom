import { sumVram } from "./gpu";
import type { ClusterView } from "./types";

export interface NetworkStats {
  nodeCount: number;
  averageLatencyMs: number | null;
  vramTotalMb: number | null;
  models: string[];
}

/** Calcula somente os fatos publicados no retrato atual do cluster. */
export function networkStats(cluster: ClusterView | null | undefined): NetworkStats {
  const peers = cluster?.peers ?? [];
  const latencies = peers
    .map((peer) => peer.route?.rtt_ms)
    .filter((value): value is number => value != null && Number.isFinite(value) && value >= 0);
  const totals = peers
    .map((peer) => peer.node?.system?.gpus ?? [])
    .map((gpus) => sumVram(gpus)?.total ?? null)
    .filter((value): value is number => value != null);
  const models = new Set<string>();
  for (const model of cluster?.model_inventory ?? []) if (model.model_id) models.add(model.model_id);
  for (const peer of peers) if (peer.model) models.add(peer.model);
  if (cluster?.active_model) models.add(cluster.active_model);

  return {
    nodeCount: peers.length,
    averageLatencyMs: latencies.length ? latencies.reduce((sum, value) => sum + value, 0) / latencies.length : null,
    vramTotalMb: totals.length ? totals.reduce((sum, value) => sum + value, 0) : null,
    models: [...models].sort(),
  };
}
