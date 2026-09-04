/**
 * Contas sobre as placas de uma máquina, no lugar onde as duas telas as leem.
 *
 * O card em miniatura do Fluxo e o card de detalhes do Painel respondem a
 * perguntas diferentes - "quanta VRAM esta máquina tem" e "como está cada
 * placa" - mas a soma é a mesma, e duas somas divergem.
 */
import type { GpuStat } from "./types";

/**
 * A VRAM somada das placas que a reportam.
 *
 * `null` quando nenhuma reporta: memória unificada (Apple) e placa sem leitura
 * caem aqui, e um total inventado viraria uma barra cheia de nada. Placas que
 * reportam e placas que não reportam na mesma máquina somam só as primeiras -
 * dizer "80 GiB de 80" seria pior que dizer "16 de 16, e há outra que não sei".
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

/** A ocupação somada, em pontos percentuais, ou `null` sem VRAM para medir. */
export function vramUsedPct(gpus: GpuStat[]): number | null {
  const vram = sumVram(gpus);
  if (!vram || vram.total <= 0) return null;
  return (vram.used / vram.total) * 100;
}
