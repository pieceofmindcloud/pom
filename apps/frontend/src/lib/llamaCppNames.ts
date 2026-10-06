import type { RuntimeParams } from "./types";

/**
 * The name llama.cpp gives to each deployment property that has a counterpart
 * there: the parameter and, where one exists, its command-line flag. Shown
 * next to the property's description so someone who knows llama.cpp finds what
 * they are looking for. A property without an entry is POM's own.
 */
export const LLAMA_CPP_NAMES: Partial<Record<keyof RuntimeParams | "cache_type_k" | "cache_type_v", string>> = {
  n_gpu_layers: "n_gpu_layers (--n-gpu-layers)",
  n_ctx: "n_ctx (--ctx-size)",
  n_batch: "n_batch (--batch-size)",
  n_ubatch: "n_ubatch (--ubatch-size)",
  max_new_tokens: "n_predict (--predict)",
  max_sessions: "n_parallel (--parallel)",
  offload_kqv: "offload_kqv (--no-kv-offload)",
  fit_memory: "fit_params (--fit)",
  fit_memory_margin_mb: "fit_params_target (--fit-target)",
  cache_type_k: "type_k (--cache-type-k)",
  cache_type_v: "type_v (--cache-type-v)",
  temperature: "temperature (--temp)",
  top_p: "top_p (--top-p)",
  top_k: "top_k (--top-k)",
  min_p: "min_p (--min-p)",
  seed: "seed (--seed)",
  dry_multiplier: "dry_multiplier (--dry-multiplier)",
  dry_base: "dry_base (--dry-base)",
  dry_allowed_length: "dry_allowed_length (--dry-allowed-length)",
  dry_penalty_last_n: "dry_penalty_last_n (--dry-penalty-last-n)",
  repeat_penalty: "repeat_penalty (--repeat-penalty)",
  repeat_last_n: "repeat_last_n (--repeat-last-n)",
  presence_penalty: "presence_penalty (--presence-penalty)",
  frequency_penalty: "frequency_penalty (--frequency-penalty)",
  no_thinking: "enable_thinking (--reasoning off)",
  reasoning_budget: "reasoning_budget (--reasoning-budget)",
  no_embeddings: "embeddings (--embedding)",
  speculative_decoding_enabled: "spec_type ngram (--spec-type)",
  speculative_k: "n_max (--draft-max)",
  mtp_enabled: "spec_type mtp (--spec-type)",
  moe_experts_in_ram: "cpu_moe (--cpu-moe)",
  moe_cache_mib: "moe_cache_size (--moe-cache-mib)",
  mtp_draft_tokens: "n_max (--draft-max)",
  mtp_min_probability: "p_min (--draft-p-min)",
};

/** `description` followed by the property's llama.cpp name, when it has one. */
export function withLlamaCppName(description: string, key: string): string {
  const name = (LLAMA_CPP_NAMES as Record<string, string | undefined>)[key];
  return name ? `${description} llama.cpp: ${name}.` : description;
}

/** The fixed seed POM has always used, shown when a deployment names none. */
export const DEFAULT_SAMPLER_SEED = "9600629759793949339";

/** Keeps only what a seed may hold: up to 20 digits. */
export function seedText(text: string): string {
  return text.replace(/\D/g, "").slice(0, 20);
}

/** What the budget field shows: empty while there is no limit. */
export function reasoningBudgetText(budget: number | undefined): string {
  return budget == null || budget < 0 ? "" : String(budget);
}

/** What an edited budget field means: empty is -1, no limit. */
export function reasoningBudgetValue(text: string): number {
  const digits = text.replace(/\D/g, "").slice(0, 7);
  return digits === "" ? -1 : Number(digits);
}
