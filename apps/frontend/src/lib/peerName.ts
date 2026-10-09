/**
 * What a machine is called on screen.
 *
 * The name is what the node **announces** - `node_name` from `pom.json`, or the
 * system hostname when nobody named the machine. The panel, the pipeline and the
 * nodes screen read that name from `/api/ui/cluster`; the slices screen reads it
 * from `/api/ui/cluster/models`, and the two routes now answer the same thing.
 *
 * The shortened `node_id` is the last resort, not the name: it is a hexadecimal
 * prefix, and seeing `83bee8670abc` in place of `ksia` in a destination selector
 * does not say which machine the file is going to.
 */
export interface NamedPeer {
  node_id: string;
  hostname?: string | null;
}

export function peerDisplayName(peer: NamedPeer): string {
  return peer.hostname?.trim() || peer.node_id.slice(0, 12);
}

/**
 * The same name, tie-broken when two peers in the list repeat it.
 *
 * Two nodes on the same machine share a hostname on purpose (it is the way to
 * give a large window without risking the smaller one's RAM), and then only the
 * `node_id` tells one from the other. The tie-break is within the displayed
 * list: adding the prefix to a name that is already unique would only make it
 * harder to read.
 */
export function uniquePeerName(peer: NamedPeer, all: NamedPeer[]): string {
  const name = peerDisplayName(peer);
  const repeated = all.filter((p) => peerDisplayName(p) === name).length > 1;
  return repeated ? `${name} · ${peer.node_id.slice(0, 6)}` : name;
}
