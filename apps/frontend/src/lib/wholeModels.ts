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
import { isWholeModel, type PeerCatalog } from "./types";

export interface WholeModelEntry {
  /** Catalog name — what `group` carries for a whole-model deploy. */
  name: string;
  /** `null` when no machine could read a layer count out of the header. */
  total_layers: number | null;
  size_bytes: number;
  quant: string | null;
  architecture: string | null;
  /** Node ids that have this file. */
  holders: string[];
}

export function wholeModelsOf(peers: PeerCatalog[] | undefined): WholeModelEntry[] {
  const byName = new Map<string, WholeModelEntry>();
  for (const peer of peers ?? []) {
    for (const model of peer.models ?? []) {
      if (!isWholeModel(model)) continue;
      const found = byName.get(model.name);
      if (!found) {
        byName.set(model.name, {
          name: model.name,
          total_layers: model.total_layers,
          size_bytes: model.size_bytes,
          quant: model.quant,
          architecture: model.architecture,
          holders: [peer.node_id],
        });
        continue;
      }
      if (!found.holders.includes(peer.node_id)) found.holders.push(peer.node_id);
      // One machine reading the header and another failing to is a fact about
      // one file, not about the model: the answer that exists wins over the
      // absent one, and the deploy is planned against the machine that has it.
      found.total_layers ??= model.total_layers;
      found.quant ??= model.quant;
      found.architecture ??= model.architecture;
      found.size_bytes = Math.max(found.size_bytes, model.size_bytes);
    }
  }
  return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
}
