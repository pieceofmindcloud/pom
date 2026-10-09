/**
 * Where this machine's layers ended up: GPU or CPU.
 *
 * The number that matters is what the engine **reported** after loading
 * (`status.reported_gpu_layers`, read from the inference server's `/health`),
 * never the config's `n_gpu_layers`. The config is the *request*, and the request
 * is almost always the sentinel 99 ("offload everything"); what really decides is
 * llama.cpp's `fit`, which lowers that number until it fits in VRAM. Deriving the
 * split from the request answers "all on the GPU" exactly in the case where some
 * are not - which is the only case the operator needs to see.
 *
 * Without the engine's report the answer is `null`, and the screen shows nothing.
 * A guess here would be worse than silence: it would say "everything on the GPU"
 * without knowing.
 *
 * A machine without a GPU (`system.gpu_kind === "none"`) also answers `null`,
 * even if `reported_gpu_layers` carries a number: on Windows machines without a
 * GPU the engine reported layers on the GPU (the request's sentinel, not a real
 * reading) and the card displayed something like "25 on the GPU" on a machine
 * that has no board at all.
 */
export interface LayerPlacement {
  /** Layers this machine serves. */
  served: number;
  gpu: number;
  cpu: number;
}

export function layerPlacement(
  startLayer: number | null | undefined,
  endLayer: number | null | undefined,
  reportedGpuLayers: number | null | undefined,
  hasGpu: boolean,
): LayerPlacement | null {
  if (!hasGpu || startLayer == null || endLayer == null || reportedGpuLayers == null) return null;
  const served = endLayer - startLayer + 1;
  if (!Number.isFinite(served) || served <= 0) return null;
  // Negative means "all" in llama.cpp's convention - treating it as zero would say
  // the machine runs entirely on the CPU exactly when it runs entirely on the GPU.
  const gpu = reportedGpuLayers < 0 ? served : Math.min(reportedGpuLayers, served);
  return { served, gpu, cpu: served - gpu };
}
