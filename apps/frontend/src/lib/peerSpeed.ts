/**
 * How fast a machine's slice is, compared with the others of the same
 * model.
 *
 * The comparison is relative, not against a fixed number, because a slice's
 * tok/s depends on how many layers it carries and on which model it is: 40 tok/s
 * is great for half of a 70B and terrible for a quarter of an 8B. What the
 * operator needs to see is who holds the token back in the pipeline - a token's
 * time is the sum of the slices, and the slowest one costs the most.
 *
 * With only one slice measured there is nobody to compare with: the tone is
 * `solo`, neutral, instead of a green that would claim what nobody measured.
 */
export type SpeedTone = "fast" | "medium" | "slow" | "solo" | "unknown";

/** From what fraction of the fastest slice a slice is still "fast". */
export const FAST_RATIO = 0.75;
/** Below this, the slice is "slow": it holds the token back. */
export const MEDIUM_RATIO = 0.4;

function measured(value: number | null | undefined): value is number {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

export type SpeedRating = { tone: SpeedTone; ratio: number | null };

export function rateSpeed(value: number | null | undefined, peers: Array<number | null | undefined>): SpeedRating {
  if (!measured(value)) return { tone: "unknown", ratio: null };
  const values = peers.filter(measured);
  if (values.length < 2) return { tone: "solo", ratio: null };
  const best = Math.max(value, ...values);
  const ratio = value / best;
  const tone = ratio >= FAST_RATIO ? "fast" : ratio >= MEDIUM_RATIO ? "medium" : "slow";
  return { tone, ratio };
}
