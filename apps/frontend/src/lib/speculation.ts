import type { RuntimeParams } from "./types";

/**
 * The MTP confidence floor, the same `d_mtp_min_probability` as the node. Turning
 * MTP on in the screen writes this value, which the operator can edit afterwards.
 */
export const MTP_MIN_PROBABILITY = 0.75;

/**
 * The two speculations never run together: the gateway uses a single draft
 * batch, and with both on MTP wins (`generation.rs`). The screen therefore offers
 * a single choice - none, MTP (draft from the model's `nextn` head)
 * or lookup (draft taken from the text itself).
 */
export type SpeculationMode = "none" | "mtp" | "lookup";

export function speculationModeOf(value: RuntimeParams): SpeculationMode {
  if (value.mtp_enabled ?? false) return "mtp";
  if (value.speculative_decoding_enabled ?? false) return "lookup";
  return "none";
}

/** The fields a choice writes; turning MTP on restores the default confidence floor. */
export function speculationPatch(mode: SpeculationMode, current: RuntimeParams): Partial<RuntimeParams> {
  return {
    mtp_enabled: mode === "mtp",
    speculative_decoding_enabled: mode === "lookup",
    ...(mode === "mtp" && speculationModeOf(current) !== "mtp" ? { mtp_min_probability: MTP_MIN_PROBABILITY } : {}),
  };
}
