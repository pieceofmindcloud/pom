import type { ClusterView, ModelCapability, PeerView } from "./types";

export type GraphRange = ModelCapability & Pick<PeerView, "node_id" | "short_id" | "load" | "node" | "unreachable" | "rediscovering" | "engine" | "role" | "model">;

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

/** The models the Flow's bar offers: inventory, announced ranges and the active one. */
export function canvasModelIds(cluster: ClusterView): string[] {
  return Array.from(new Set([
    ...(cluster.model_inventory ?? []).map((inventory) => inventory.model_id),
    ...cluster.peers.flatMap((peer) => peer.capabilities.map((capability) => capability.model_id)),
    ...(cluster.active_model ? [cluster.active_model] : []),
  ])).sort();
}

/**
 * The model the Flow shows: the one picked while it still exists, otherwise the
 * active model, otherwise the first. The Dashboard resolves it the same way so its
 * Add and Undeploy act on what the map shows.
 */
export function selectedCanvasModel(
  cluster: ClusterView,
  picked: string | null | undefined,
  modelIds: string[] = canvasModelIds(cluster),
): string | undefined {
  return picked && modelIds.includes(picked) ? picked : cluster.active_model || modelIds[0];
}
