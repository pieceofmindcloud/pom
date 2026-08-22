/**
 * Onde as camadas desta máquina foram parar: GPU ou CPU.
 *
 * O número que importa é o que o motor **relatou** depois de carregar
 * (`status.reported_gpu_layers`, lido do `/health` do servidor de inferência),
 * nunca o `n_gpu_layers` da config. A config é o *pedido*, e o pedido quase
 * sempre é o sentinela 99 ("descarregue tudo"); quem decide de verdade é o
 * `fit` do llama.cpp, que baixa esse número até caber na VRAM. Derivar a
 * divisão do pedido responde "todas na GPU" exatamente no caso em que algumas
 * não estão - que é o único caso que o operador precisa enxergar.
 *
 * Sem o relato do motor a resposta é `null`, e a tela não mostra nada. Um
 * palpite aqui seria pior que o silêncio: diria "tudo na GPU" sem saber.
 */
export interface LayerPlacement {
  /** Camadas que esta máquina serve. */
  served: number;
  gpu: number;
  cpu: number;
}

export function layerPlacement(
  startLayer: number | null | undefined,
  endLayer: number | null | undefined,
  reportedGpuLayers: number | null | undefined,
): LayerPlacement | null {
  if (startLayer == null || endLayer == null || reportedGpuLayers == null) return null;
  const served = endLayer - startLayer + 1;
  if (!Number.isFinite(served) || served <= 0) return null;
  // Negativo é "todas" na convenção do llama.cpp - tratá-lo como zero diria
  // que a máquina roda inteira na CPU justamente quando ela roda inteira na GPU.
  const gpu = reportedGpuLayers < 0 ? served : Math.min(reportedGpuLayers, served);
  return { served, gpu, cpu: served - gpu };
}
