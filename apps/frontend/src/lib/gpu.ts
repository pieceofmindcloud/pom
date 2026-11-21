/**
 * Computations over a machine's boards, in the place where both screens read them.
 *
 * The Flow's miniature card and the Panel's details card answer different
 * questions - "how much VRAM does this machine have" and "how is each board
 * doing" - but the sum is the same, and two sums diverge.
 */
import type { GpuStat, NodeConfig, SystemLoad } from "./types";

/**
 * The summed VRAM of the boards that report it.
 *
 * `null` when none reports: unified memory (Apple) and a board with no reading
 * land here, and an invented total would become a bar full of nothing. Boards
 * that report and boards that do not on the same machine sum only the first -
 * saying "80 GiB of 80" would be worse than saying "16 of 16, and there is
 * another I do not know about".
 */
export function sumVram(gpus: GpuStat[]): { used: number; total: number } | null {
  let used = 0;
  let total = 0;
  let known = false;
  for (const gpu of gpus) {
    if (gpu.vram_total_mb == null) continue;
    known = true;
    total += gpu.vram_total_mb;
    used += gpu.vram_used_mb ?? 0;
  }
  return known ? { used, total } : null;
}

/** The summed occupancy, in percentage points, or `null` without VRAM to measure. */
export function vramUsedPct(gpus: GpuStat[]): number | null {
  const vram = sumVram(gpus);
  if (!vram || vram.total <= 0) return null;
  return (vram.used / vram.total) * 100;
}

/**
 * The machine as one node sees it: when its deployment runs on a subset of the
 * GPUs (`config.gpus`, chosen in the Deploy Studio, as an instance usually
 * does), only those boards, so a node's card does not show the cards another
 * node of the same machine serves on.
 */
export function servedSystem(system: SystemLoad | undefined, config: NodeConfig | null | undefined): SystemLoad | undefined {
  const chosen = config?.gpus;
  if (!system || !chosen?.length) return system;
  return { ...system, gpus: system.gpus.filter((gpu) => chosen.includes(gpu.index)) };
}
