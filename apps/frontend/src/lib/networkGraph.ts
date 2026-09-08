import type { ClusterView, ModelCapability, PeerView } from "./types";

export type GraphRange = ModelCapability & Pick<PeerView, "node_id" | "short_id" | "load" | "node" | "unreachable" | "rediscovering">;

export function graphFromCluster(cluster: ClusterView) {
  const byModel: Record<string, GraphRange[]> = {};
  for (const peer of cluster.peers) {
    for (const capability of peer.capabilities) {
      (byModel[capability.model_id] ??= []).push({ ...capability, ...peer });
    }
  }
  for (const ranges of Object.values(byModel)) {
    ranges.sort((a, b) => a.start_layer - b.start_layer || a.short_id.localeCompare(b.short_id));
  }
  return { models: Object.keys(byModel).sort(), byModel };
}
