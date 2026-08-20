/**
 * What was deployed last time, per model.
 *
 * A deploy is a long form: which ranges, on which machines, in which order,
 * with eleven execution settings and a decision per machine about the KV cache.
 * Retyping all of it to change one number is the reason the screen felt heavy,
 * and the answer is almost always "the same as last time" — the cluster is the
 * same cluster.
 *
 * Kept in this browser's `localStorage`, next to the chat transcripts and for
 * the same reason: it is the operator's own convenience, not cluster state, and
 * putting it on a node would mean deciding whose preference wins. The trade is
 * that it does not follow you to another browser.
 */
import type { DeploySelection, DeployStrategy, RuntimeParams } from "./types";
import { storageKey } from "./brand";

const STORAGE = storageKey("deploy.last");
/** Models remembered before the oldest is dropped. */
const MAX_MODELS = 20;

export interface DeployMemory {
  selection: DeploySelection[];
  params: RuntimeParams | null;
  merge: boolean;
  deployment_name?: string;
  strategy?: DeployStrategy;
  at: string;
}

type Store = Record<string, DeployMemory>;

function read(): Store {
  try {
    const raw = localStorage.getItem(STORAGE);
    const parsed = raw ? JSON.parse(raw) : {};
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
  } catch {
    return {};
  }
}

export function rememberDeploy(group: string, memory: Omit<DeployMemory, "at">) {
  if (!group) return;
  try {
    const store = read();
    store[group] = { ...memory, at: new Date().toISOString() };
    // Oldest first out, so a machine that has seen many models does not grow
    // this forever.
    const trimmed = Object.entries(store)
      .sort((a, b) => b[1].at.localeCompare(a[1].at))
      .slice(0, MAX_MODELS);
    localStorage.setItem(STORAGE, JSON.stringify(Object.fromEntries(trimmed)));
  } catch {
    /* quota exhausted — a convenience is not worth breaking the screen over */
  }
}

export function recallDeploy(group: string): DeployMemory | null {
  const found = read()[group];
  return found && Array.isArray(found.selection) ? found : null;
}

/**
 * The remembered choice, minus everything the cluster no longer offers.
 *
 * Slices get deleted and machines leave, and a remembered row pointing at
 * either would come back as a blocked plan the operator never asked for. What
 * survives is restored; what does not is silently dropped, which is the same
 * thing the screen would have shown had they picked it by hand today.
 *
 * Returns `null` when nothing survives, so the caller keeps the node's own
 * suggestion instead of showing an empty selection.
 */
export function usableSelection(
  remembered: DeploySelection[],
  ranges: Array<{ start_layer: number; end_layer: number }>,
  machineIds: string[],
): DeploySelection[] | null {
  const hasRange = (r: DeploySelection) =>
    ranges.some((c) => c.start_layer === r.start_layer && c.end_layer === r.end_layer);
  const kept = remembered
    .filter(hasRange)
    // An empty target is a row with no machine, which is a legitimate thing to
    // restore; a target naming a machine that left is not.
    .map((r) =>
      r.target_node_id && !machineIds.includes(r.target_node_id)
        ? { ...r, target_node_id: "" }
        : r,
    );
  return kept.length > 0 ? kept : null;
}
