/**
 * The un-sliced models the cluster holds, named once.
 *
 * The deploy screen needs a menu before it has a plan: the node only surveys
 * one model at a time (`POST /api/ui/deploy/plan` takes a name), so the list of
 * names to offer has to come from the catalog fan-out the screen already loads.
 *
 * The identity is the catalog's `name`, which is exactly what the node matches
 * on when it surveys — the same string travels back as `group` and ends up as
 * the cluster-wide `model_label` every peer announces. Two machines that hold
 * the same model under different names (an LM Studio `publisher/repo` next to a
 * loose `.gguf`) are therefore two entries here, and that is honest: the node
 * cannot pair them either.
 *
 * A model whose GGUF header did not yield a layer count is kept in the list on
 * purpose. It is visible on the Models screen, so hiding it here would read as
 * the cluster having lost it; picking it gets a refusal from the node that says
 * what is actually wrong with the file.
 */
import { isWholeModel, tpPartsOf, type PeerCatalog } from "./types";

export interface WholeModelEntry {
  /** Catalog name — what `group` carries for a whole-model deploy. */
  name: string;
  /** `null` when no machine could read a layer count out of the header. */
  total_layers: number | null;
  size_bytes: number;
  quant: string | null;
  architecture: string | null;
  mmproj_path: string | null;
  mmproj_candidates: string[];
  mmproj_reason: string | null;
  /** Node ids that have this file. */
  holders: string[];
  /**
   * The repository it was downloaded from, by any holder, with the other parts
   * of a split file: what Download here asks a machine that lacks it to fetch.
   * `null` when every copy was placed by hand.
   */
  source: { repo_id: string; revision: string | null; filename: string; extra_files: string[] } | null;
}

/** The source of `model` on `peer`, when that peer downloaded it through POM. */
function sourceOf(peer: PeerCatalog, model: { path?: string; multipart_parts?: { path: string }[] }) {
  const found = (peer.hf_sources ?? []).find((source) => source.path === model.path);
  if (!found) return null;
  const base = (path: string) => path.split(/[\\/]/).pop() ?? path;
  const extra_files = (model.multipart_parts ?? [])
    .map((part) => base(part.path))
    .filter((name) => name !== base(found.filename));
  return { repo_id: found.repo_id, revision: found.revision, filename: found.filename, extra_files };
}

export interface TensorParallelRankEntry {
  rank: number;
  size: number;
  holders: string[];
}

/**
 * Tensor-parallel slicing rounds, grouped by source manifest and TP width.
 *
 * A TP part is not a pipeline stage: every rank covers the complete layer
 * range. The deployment screen therefore needs the rank inventory separately
 * from `wholeModelsOf`, otherwise it silently turns `1of2` and `2of2` into one
 * ordinary `L0-31` model.
 */
export interface TensorParallelGroupEntry {
  key: string;
  group: string;
  label: string;
  total_layers: number | null;
  size_bytes: number;
  quant: string | null;
  architecture: string | null;
  tp_size: number;
  ranks: TensorParallelRankEntry[];
  holders: string[];
}

export function wholeModelsOf(peers: PeerCatalog[] | undefined): WholeModelEntry[] {
  const byName = new Map<string, WholeModelEntry>();
  for (const peer of peers ?? []) {
    for (const model of peer.models ?? []) {
      if (!isWholeModel(model)) continue;
      const found = byName.get(model.name);
      if (!found) {
        const candidates = [...(model.mmproj_candidates ?? [])];
        if (model.mmproj_path && !candidates.includes(model.mmproj_path)) {
          candidates.push(model.mmproj_path);
        }
        byName.set(model.name, {
          name: model.name,
          total_layers: model.total_layers,
          size_bytes: model.size_bytes,
          quant: model.quant,
          architecture: model.architecture,
          mmproj_path: model.mmproj_path ?? null,
          mmproj_candidates: candidates,
          mmproj_reason: model.mmproj_reason ?? null,
          holders: [peer.node_id],
          source: sourceOf(peer, model),
        });
        continue;
      }
      if (!found.holders.includes(peer.node_id)) found.holders.push(peer.node_id);
      found.source ??= sourceOf(peer, model);
      // One machine reading the header and another failing to is a fact about
      // one file, not about the model: the answer that exists wins over the
      // absent one, and the deploy is planned against the machine that has it.
      found.total_layers ??= model.total_layers;
      found.quant ??= model.quant;
      found.architecture ??= model.architecture;
      found.mmproj_path ??= model.mmproj_path ?? null;
      for (const candidate of model.mmproj_candidates ?? []) {
        if (!found.mmproj_candidates.includes(candidate)) found.mmproj_candidates.push(candidate);
      }
      if (model.mmproj_path && !found.mmproj_candidates.includes(model.mmproj_path)) {
        found.mmproj_candidates.push(model.mmproj_path);
      }
      found.mmproj_reason ??= model.mmproj_reason ?? null;
      found.size_bytes = Math.max(found.size_bytes, model.size_bytes);
    }
  }
  return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
}

export function tensorParallelModelsOf(
  peers: PeerCatalog[] | undefined,
): TensorParallelGroupEntry[] {
  const byKey = new Map<string, TensorParallelGroupEntry>();
  for (const peer of peers ?? []) {
    for (const set of peer.shard_sets ?? []) {
      for (const part of tpPartsOf(set)) {
        if (part.tp_rank == null || part.tp_size == null || part.tp_size < 2) continue;
        const key = `${set.group}:${part.tp_size}`;
        const found = byKey.get(key);
        const entry = found ?? {
          key,
          group: set.group,
          label: set.model_label || set.group,
          total_layers: set.total_layers ?? part.total_layers,
          size_bytes: part.size_bytes,
          quant: part.quant,
          architecture: part.architecture,
          tp_size: part.tp_size,
          ranks: [],
          holders: [],
        };
        entry.total_layers ??= part.total_layers;
        entry.size_bytes = Math.max(entry.size_bytes, part.size_bytes);
        entry.quant ??= part.quant;
        entry.architecture ??= part.architecture;
        if (!entry.holders.includes(peer.node_id)) entry.holders.push(peer.node_id);
        let rank = entry.ranks.find((candidate) => candidate.rank === part.tp_rank);
        if (!rank) {
          rank = { rank: part.tp_rank, size: part.tp_size, holders: [] };
          entry.ranks.push(rank);
        }
        if (!rank.holders.includes(peer.node_id)) rank.holders.push(peer.node_id);
        byKey.set(key, entry);
      }
    }
  }
  return [...byKey.values()]
    .map((entry) => ({
      ...entry,
      ranks: entry.ranks.slice().sort((a, b) => a.rank - b.rank),
      holders: entry.holders.slice().sort(),
    }))
    .sort((a, b) => a.label.localeCompare(b.label) || a.tp_size - b.tp_size);
}

/**
 * Where `nodeId` downloaded the file named `fileName` (a sidecar the plan
 * would copy from it), read from its `hf_sources`; `null` when that machine
 * did not download it through POM.
 */
export function fileSourceOn(peers: PeerCatalog[] | undefined, nodeId: string, fileName: string) {
  const peer = (peers ?? []).find((candidate) => candidate.node_id === nodeId);
  const base = (path: string) => path.split(/[\\/]/).pop() ?? path;
  const found = (peer?.hf_sources ?? []).find((source) => base(source.path) === fileName || base(source.filename) === fileName);
  return found ? { repo_id: found.repo_id, revision: found.revision, filename: found.filename, extra_files: [] as string[] } : null;
}
