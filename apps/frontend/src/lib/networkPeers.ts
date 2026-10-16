/**
 * How the Panel reads the node's peer list.
 *
 * Two different questions, and the screen needs both: "who serves the active
 * model, in the order the request passes" (`pipelinePeers`) and "who is on the
 * network" (`networkPeers`). The second includes a machine that is connected
 * with no deployed slice, which is precisely the one that appears nowhere when
 * you only look at the pipeline - and it is where you look for room for the
 * next slice.
 */
import type { ClusterView, PeerView } from "./types";

/** Peers serving the model this node is on, in execution order. */
export function pipelinePeers(cluster: ClusterView): PeerView[] {
  const onModel = cluster.peers.filter((p) => p.model === cluster.active_model);
  return (onModel.length ? onModel : cluster.peers)
    .slice()
    .sort((a, b) => a.start_layer - b.start_layer || a.short_id.localeCompare(b.short_id));
}

/**
 * Does the machine announce a slice? An empty `model` with a zeroed `total_layers`
 * is what the node sends for whoever is on the network with nothing deployed
 * (`webui::cluster`), and it is not a "0-0" range: it is the absence of a range.
 */
export function hasSlice(peer: PeerView): boolean {
  return Boolean(peer.model) && peer.total_layers > 0;
}

/**
 * The whole network, split into whoever is part of the active deployment and
 * whoever is not.
 *
 * The first list keeps the pipeline order; the second goes by name, which is
 * how the operator looks for a machine that has no role yet.
 */
export function networkPeers(cluster: ClusterView): {
  pipeline: PeerView[];
  others: PeerView[];
} {
  const pipeline = pipelinePeers(cluster);
  const inPipeline = new Set(pipeline.map((p) => p.node_id));
  const label = (p: PeerView) => p.node?.hostname || p.short_id;
  const others = cluster.peers
    .filter((p) => !inPipeline.has(p.node_id))
    .slice()
    .sort((a, b) => label(a).localeCompare(label(b)) || a.short_id.localeCompare(b.short_id));
  return { pipeline, others };
}
