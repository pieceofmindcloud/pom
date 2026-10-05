import type { RuntimeParams } from "./types";

/**
 * O piso de confiança do MTP, o mesmo `d_mtp_min_probability` do nó. Ligar o
 * MTP na tela grava este valor, que o operador pode editar depois.
 */
export const MTP_MIN_PROBABILITY = 0.75;

/**
 * As duas especulações nunca rodam juntas: o gateway usa um lote de rascunho
 * só, e com as duas ligadas o MTP ganha (`generation.rs`). A tela oferece por
 * isso uma escolha só - nenhuma, o MTP (rascunho da cabeça `nextn` do modelo)
 * ou o lookup (rascunho tirado do próprio texto).
 */
export type SpeculationMode = "none" | "mtp" | "lookup";

export function speculationModeOf(value: RuntimeParams): SpeculationMode {
  if (value.mtp_enabled ?? false) return "mtp";
  if (value.speculative_decoding_enabled ?? false) return "lookup";
  return "none";
}

/** Os campos que uma escolha grava; ligar o MTP repõe o piso de confiança padrão. */
export function speculationPatch(mode: SpeculationMode, current: RuntimeParams): Partial<RuntimeParams> {
  return {
    mtp_enabled: mode === "mtp",
    speculative_decoding_enabled: mode === "lookup",
    ...(mode === "mtp" && speculationModeOf(current) !== "mtp" ? { mtp_min_probability: MTP_MIN_PROBABILITY } : {}),
  };
}
