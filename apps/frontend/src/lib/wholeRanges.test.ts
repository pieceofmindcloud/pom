import { describe, expect, it } from "vitest";
import { autoBalance, evenSplit, isWholeRangeSplit, stepRange, validEnds, wholeRangesOf } from "./wholeRanges";
import type { DeploySelection } from "./types";

const row = (start: number, end: number, node: string): DeploySelection => ({
  start_layer: start,
  end_layer: end,
  target_node_id: node,
  n_gpu_layers: null,
  offload_kqv: null,
});

describe("whole model ranges", () => {
  it("is a split only when every machine holds the whole model and a range is partial", () => {
    expect(isWholeRangeSplit([row(0, 15, "a"), row(16, 31, "b")], ["a", "b"], 32)).toBe(true);
    expect(isWholeRangeSplit([row(0, 15, "a"), row(16, 31, "c")], ["a", "b"], 32)).toBe(false);
    expect(isWholeRangeSplit([row(0, 31, "a"), row(0, 31, "b")], ["a", "b"], 32)).toBe(false);
    expect(wholeRangesOf([row(0, 15, "a")])).toEqual([{ node_id: "a", start_layer: 0, end_layer: 15 }]);
  });

  it("splits evenly on the valid cuts", () => {
    expect(evenSplit(["a", "b"], 32, validEnds(32, null)).map((r) => [r.start_layer, r.end_layer])).toEqual([[0, 15], [16, 31]]);
    // A hybrid model that can only be cut every 4 layers; a tie goes to the lower cut.
    const ends = validEnds(30, [3, 7, 11, 15, 19, 23, 27]);
    expect(evenSplit(["a", "b", "c"], 30, ends).map((r) => [r.start_layer, r.end_layer])).toEqual([[0, 7], [8, 19], [20, 29]]);
  });

  it("moves the shared cut by one valid step, and says why it cannot", () => {
    const ends = validEnds(32, [7, 15, 23]);
    const rows = [row(0, 15, "a"), row(16, 31, "b")];
    const grown = stepRange(rows, 0, "grow", ends);
    expect(Array.isArray(grown) && grown.map((r) => [r.start_layer, r.end_layer])).toEqual([[0, 23], [24, 31]]);
    // The last machine grows by moving the cut before it down.
    const last = stepRange(rows, 1, "grow", ends);
    expect(Array.isArray(last) && last.map((r) => [r.start_layer, r.end_layer])).toEqual([[0, 7], [8, 31]]);
    // No valid cut left before the neighbour's last layer.
    expect(stepRange([row(0, 23, "a"), row(24, 31, "b")], 0, "grow", ends)).toBe("neighbourEmpty");
    // No valid cut below layer 7 at all.
    expect(stepRange([row(0, 7, "a"), row(8, 31, "b")], 0, "shrink", ends)).toBe("noCut");
    expect(stepRange([row(0, 31, "a")], 0, "grow", ends)).toBe("neighbourEmpty");
  });

  it("keeps locked ranges and says why their cuts cannot move", () => {
    const ends = validEnds(32, null);
    const rows = [row(0, 9, "a"), row(10, 19, "b"), row(20, 31, "c")];
    expect(stepRange(rows, 1, "grow", ends, new Set([1]))).toBe("locked");
    // b grows by moving the cut it shares with c, which is locked.
    expect(stepRange(rows, 1, "grow", ends, new Set([2]))).toBe("neighbourLocked");
    // a shares no cut with c: it still moves.
    expect(Array.isArray(stepRange(rows, 0, "grow", ends, new Set([2])))).toBe(true);
  });

  it("balances the unlocked ranges by free memory, at valid cuts, around locked ones", () => {
    const ends = validEnds(32, null);
    // One byte per layer: prefix(end) = end + 1.
    const prefix = (end: number) => end + 1;
    const memory: Record<string, number> = { a: 3000, b: 1000, c: 4000 };
    const balanced = autoBalance([row(0, 15, "a"), row(16, 31, "b")], new Set(), ends, prefix, (node) => memory[node]);
    expect(Array.isArray(balanced) && balanced.map((r) => [r.start_layer, r.end_layer])).toEqual([[0, 23], [24, 31]]);

    // c is locked at 20-31; a and b share 0-19 by memory (3:1).
    const around = autoBalance([row(0, 9, "a"), row(10, 19, "b"), row(20, 31, "c")], new Set([2]), ends, prefix, (node) => memory[node]);
    expect(Array.isArray(around) && around.map((r) => [r.start_layer, r.end_layer])).toEqual([[0, 14], [15, 19], [20, 31]]);

    // A hybrid model cut every 8 layers: the nearest valid cut wins.
    const coarse = autoBalance([row(0, 15, "a"), row(16, 31, "b")], new Set(), validEnds(32, [7, 15, 23]), prefix, (node) => memory[node]);
    expect(Array.isArray(coarse) && coarse.map((r) => [r.start_layer, r.end_layer])).toEqual([[0, 23], [24, 31]]);

    expect(autoBalance([row(0, 15, "a"), row(16, 31, "b")], new Set([0]), ends, prefix, () => 1)).toBe("allLocked");
    expect(autoBalance([row(0, 15, "a"), row(16, 31, "b")], new Set(), ends, () => null, () => 1)).toBe("noSizes");
  });
});
