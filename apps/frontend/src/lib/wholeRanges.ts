/**
 * Layer ranges of a whole model, each served by a machine that holds the
 * whole file itself (`whole_ranges` in the deploy request). Pure arithmetic
 * over the rows the Deploy Studio keeps, so the screen and its tests share it.
 */
import type { DeploySelection } from "./types";

export interface WholeRange {
  node_id: string;
  start_layer: number;
  end_layer: number;
}

/** True when the rows split the whole model over machines that each hold it:
 *  every target is a holder and at least one row is not the whole range (a
 *  set of whole-range rows is replicas, which the plain selection serves). */
export function isWholeRangeSplit(rows: DeploySelection[], holders: string[], total: number | null): boolean {
  if (total == null || rows.length === 0) return false;
  return rows.every((row) => holders.includes(row.target_node_id))
    && rows.some((row) => !(row.start_layer === 0 && row.end_layer === total - 1));
}

export function wholeRangesOf(rows: DeploySelection[]): WholeRange[] {
  return rows.map((row) => ({ node_id: row.target_node_id, start_layer: row.start_layer, end_layer: row.end_layer }));
}

/** The layer ends a range may stop at: the GGUF's cuts when known (the plan's
 *  `layer_options`), every layer otherwise, and always the last layer. */
export function validEnds(total: number, cuts: number[] | null): number[] {
  const ends = cuts ?? Array.from({ length: total }, (_, layer) => layer);
  return [...new Set([...ends.filter((end) => end >= 0 && end < total), total - 1])].sort((a, b) => a - b);
}

function nearest(ends: number[], target: number): number {
  return ends.reduce((best, end) => (Math.abs(end - target) < Math.abs(best - target) ? end : best), ends[0]);
}

/** Splits the model evenly over the machines, in order, with each boundary on
 *  the nearest valid cut. Fewer ranges come back when the cuts are too coarse
 *  for every machine to get one. */
export function evenSplit(nodeIds: string[], total: number, ends: number[]): DeploySelection[] {
  const rows: DeploySelection[] = [];
  let start = 0;
  for (let index = 0; index < nodeIds.length && start < total; index++) {
    const last = index === nodeIds.length - 1;
    const ideal = Math.round(((index + 1) * total) / nodeIds.length) - 1;
    const candidates = ends.filter((end) => end >= start && (last || end < total - 1));
    if (candidates.length === 0) break;
    const end = last ? total - 1 : nearest(candidates, ideal);
    rows.push({ start_layer: start, end_layer: end, target_node_id: nodeIds[index], n_gpu_layers: null, offload_kqv: null });
    start = end + 1;
  }
  if (rows.length > 0) rows[rows.length - 1].end_layer = total - 1;
  return rows;
}

export type StepLimit = "noCut" | "neighbourEmpty" | "locked" | "neighbourLocked";

/** One − or + on the range at `index` (rows in layer order): moves the cut it
 *  shares with a neighbour by one valid step. The last range moves the cut
 *  before it; every other range moves the cut after it. Returns the new rows,
 *  or why the cut cannot move. */
export function stepRange(
  rows: DeploySelection[],
  index: number,
  direction: "grow" | "shrink",
  ends: number[],
  locked: ReadonlySet<number> = new Set(),
): DeploySelection[] | StepLimit {
  if (rows.length < 2) return "neighbourEmpty";
  if (locked.has(index)) return "locked";
  const lastIndex = rows.length - 1;
  // The boundary is the end of row `b`; moving it up gives layers to row b.
  const b = index === lastIndex ? index - 1 : index;
  if (locked.has(b === index ? b + 1 : b)) return "neighbourLocked";
  const up = index === lastIndex ? direction === "shrink" : direction === "grow";
  const current = rows[b].end_layer;
  const low = rows[b].start_layer;
  const high = rows[b + 1].end_layer - 1;
  const inRange = ends.filter((end) => end >= low && end <= high);
  const next = up
    ? inRange.find((end) => end > current)
    : [...inRange].reverse().find((end) => end < current);
  if (next == null) {
    const blockedByNeighbour = up
      ? ends.some((end) => end > high && end > current)
      : ends.some((end) => end < low && end < current);
    return blockedByNeighbour ? "neighbourEmpty" : "noCut";
  }
  return rows.map((row, position) => {
    if (position === b) return { ...row, end_layer: next };
    if (position === b + 1) return { ...row, start_layer: next + 1 };
    return row;
  });
}

/**
 * Redistributes the layers of the unlocked ranges in proportion to each
 * machine's free memory (`capacity`), at valid cuts. Locked ranges keep their
 * exact layers; each run of unlocked ranges between them is balanced within
 * its own span, and every machine keeps at least one valid range. Sizes come
 * from `prefixBytes(end)`: the bytes of layers 0..=end, known at valid ends.
 * Returns why nothing can be balanced instead of rows when that is the case.
 */
export function autoBalance(
  rows: DeploySelection[],
  locked: ReadonlySet<number>,
  ends: number[],
  prefixBytes: (end: number) => number | null,
  capacity: (nodeId: string) => number,
): DeploySelection[] | "allLocked" | "noSizes" {
  const unlocked = rows.map((_, index) => index).filter((index) => !locked.has(index));
  if (unlocked.length < 2 && rows.length > 1) return "allLocked";
  const next = rows.map((row) => ({ ...row }));
  let index = 0;
  while (index < rows.length) {
    if (locked.has(index)) { index += 1; continue; }
    let last = index;
    while (last + 1 < rows.length && !locked.has(last + 1)) last += 1;
    const run = rows.slice(index, last + 1);
    if (run.length > 1) {
      const start = run[0].start_layer;
      const end = run[run.length - 1].end_layer;
      const base = start === 0 ? 0 : prefixBytes(start - 1);
      const top = prefixBytes(end);
      if (base == null || top == null) return "noSizes";
      const weights = run.map((row) => {
        const free = capacity(row.target_node_id);
        return Number.isFinite(free) && free > 0 ? free : 0;
      });
      const totalWeight = weights.reduce((sum, weight) => sum + weight, 0);
      const share = totalWeight > 0 ? weights : run.map(() => 1);
      const shareTotal = share.reduce((sum, weight) => sum + weight, 0);
      const candidates = ends.filter((cut) => cut >= start && cut < end);
      let previous = start - 1;
      let cumulative = 0;
      for (let m = 0; m < run.length - 1; m++) {
        cumulative += share[m];
        const target = base + ((top - base) * cumulative) / shareTotal;
        const remaining = run.length - 2 - m;
        const feasible = candidates.filter((cut, position) =>
          cut > previous && candidates.length - 1 - position >= remaining && prefixBytes(cut) != null);
        if (feasible.length === 0) return "noSizes";
        const cut = feasible.reduce((best, candidate) =>
          Math.abs((prefixBytes(candidate) as number) - target) < Math.abs((prefixBytes(best) as number) - target) ? candidate : best,
        feasible[0]);
        next[index + m].end_layer = cut;
        next[index + m + 1].start_layer = cut + 1;
        previous = cut;
      }
    }
    index = last + 1;
  }
  return next;
}
