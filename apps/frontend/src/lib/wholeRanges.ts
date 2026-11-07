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

export type StepLimit = "noCut" | "neighbourEmpty";

/** One − or + on the range at `index` (rows in layer order): moves the cut it
 *  shares with a neighbour by one valid step. The last range moves the cut
 *  before it; every other range moves the cut after it. Returns the new rows,
 *  or why the cut cannot move. */
export function stepRange(
  rows: DeploySelection[],
  index: number,
  direction: "grow" | "shrink",
  ends: number[],
): DeploySelection[] | StepLimit {
  if (rows.length < 2) return "neighbourEmpty";
  const lastIndex = rows.length - 1;
  // The boundary is the end of row `b`; moving it up gives layers to row b.
  const b = index === lastIndex ? index - 1 : index;
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
