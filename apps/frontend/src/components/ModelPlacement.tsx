/**
 * Como o llama.cpp de fato carregou o modelo nesta máquina.
 *
 * O que o operador quer saber ao abrir um card é o que a configuração não
 * responde: quais camadas ficaram na GPU, quais sobraram na CPU, e onde o cache
 * KV foi parar. `n_gpu_layers` é o **pedido**; entre o pedido e o resultado
 * estão o `fit`, que reduz o número numa GPU cheia, e os overrides de buffer
 * type, que mandam parte de uma camada - os experts de um MoE, tipicamente -
 * para a CPU sem mudar o dispositivo do bloco.
 *
 * Por isso a classificação abaixo lê os **buffers dos pesos**, não o campo
 * `device`. Uma camada com 90% dos bytes na GPU e os experts na CPU aparece
 * como mista, que é o que ela é. Derivar a faixa de `n_gpu_layers` mostraria
 * ela como inteira na GPU, e a conta de memória de quem lê não fecharia.
 */
import clsx from "clsx";
import { Cpu } from "lucide-react";
import { bytes } from "../lib/format";
import type { BufferBytes, ContextMemoryBuffer, LayerPlacement, ModelPlacement } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { Footnote, KeyValue } from "./ui";

export type Placement = "gpu" | "cpu" | "mixed" | "unknown";

/**
 * Um buffer é de CPU pelo nome que o próprio ggml lhe dá.
 *
 * Os backends nomeiam o buffer com o nome do dispositivo (`CUDA0`, `Metal`,
 * `Vulkan0`), e só o host se chama `CPU` - com o sufixo `_Mapped` quando o peso
 * veio de `mmap` do arquivo, que é o caso comum de camada não descarregada.
 */
export function isCpuBuffer(name: string): boolean {
  return name === "CPU" || name.startsWith("CPU_");
}

/** Onde os pesos de um grupo ficaram, pelos bytes em cada buffer. */
export function placementOf(buffers: BufferBytes[]): Placement {
  const total = buffers.reduce((sum, b) => sum + b.bytes, 0);
  if (total === 0) return "unknown";
  const cpu = buffers.filter((b) => isCpuBuffer(b.buffer)).reduce((sum, b) => sum + b.bytes, 0);
  if (cpu === 0) return "gpu";
  if (cpu === total) return "cpu";
  return "mixed";
}

const CELL: Record<Placement, string> = {
  gpu: "bg-lime-400/80",
  cpu: "bg-line-strong",
  mixed: "bg-amber-400/80",
  unknown: "bg-line",
};

/** Quantos bytes de um grupo estão em buffer de CPU e quantos não estão. */
function split(buffers: BufferBytes[]): { cpu: number; gpu: number } {
  let cpu = 0;
  let gpu = 0;
  for (const b of buffers) {
    if (isCpuBuffer(b.buffer)) cpu += b.bytes;
    else gpu += b.bytes;
  }
  return { cpu, gpu };
}

/** O resumo de uma camada, para o `title` da célula. */
function layerTitle(layer: LayerPlacement): string {
  const where = layer.buffers.map((b) => `${b.buffer} ${bytes(b.bytes)}`).join(" · ");
  return `${layer.layer}: ${where || layer.device}`;
}

/**
 * A tira de camadas: uma célula por bloco, na ordem em que o modelo as executa.
 *
 * Uma tira, e não uma lista: com 80 camadas a pergunta que se faz é sobre a
 * *forma* do corte - "quantas ficaram de fora e onde" - e essa forma se lê de
 * relance numa faixa, nunca em oitenta linhas de texto.
 */
function LayerStrip({ layers }: { layers: LayerPlacement[] }) {
  return (
    <div className="flex flex-wrap gap-[2px]">
      {layers.map((layer) => (
        <span
          key={layer.layer}
          title={layerTitle(layer)}
          className={clsx("h-3.5 w-2 rounded-[1px]", CELL[placementOf(layer.buffers)])}
        />
      ))}
    </div>
  );
}

function Legend({ tone, label }: { tone: Placement; label: string }) {
  return (
    <span className="flex items-center gap-1.5 text-[0.72rem] text-muted">
      <span className={clsx("h-2.5 w-2.5 rounded-[1px]", CELL[tone])} />
      {label}
    </span>
  );
}

/**
 * Onde o cache KV ficou, pelos bytes de contexto de cada buffer.
 *
 * `context` na repartição do llama.cpp é o cache KV (mais o estado recorrente,
 * num modelo híbrido). Um buffer com `context` zerado não guarda cache nenhum e
 * não entra na frase.
 */
export function kvCacheBuffers(buffers: ContextMemoryBuffer[]): ContextMemoryBuffer[] {
  return buffers.filter((b) => b.context > 0);
}

export function ModelPlacementSection({
  placement,
  contextMemory,
  offloadKqv,
  fitApplied,
}: {
  placement: ModelPlacement | null;
  contextMemory: ContextMemoryBuffer[] | null;
  /** O que foi **pedido** para o cache KV. Só aparece na falta da medida. */
  offloadKqv: boolean;
  fitApplied: boolean;
}) {
  const { t } = useI18n();

  if (!placement) {
    return (
      <div className="border-t border-line px-5 py-3">
        <div className="eyebrow mb-2">{t("placement.title")}</div>
        <p className="text-xs leading-relaxed text-muted">{t("placement.unavailable")}</p>
      </div>
    );
  }

  const counts = { gpu: 0, cpu: 0, mixed: 0, unknown: 0 };
  for (const layer of placement.layers) counts[placementOf(layer.buffers)]++;
  const weights = split(placement.totals);
  const kv = contextMemory ? kvCacheBuffers(contextMemory) : [];

  return (
    <div className="space-y-3 border-t border-line px-5 py-4">
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <div className="eyebrow">{t("placement.title")}</div>
        <span className="font-mono text-xs text-muted">
          {t("placement.summary", {
            gpu: counts.gpu + counts.mixed,
            total: placement.n_layer,
          })}
        </span>
      </div>

      <LayerStrip layers={placement.layers} />

      <div className="flex flex-wrap gap-x-4 gap-y-1">
        <Legend tone="gpu" label={t("placement.onGpu", { count: counts.gpu })} />
        {counts.mixed > 0 && (
          <Legend tone="mixed" label={t("placement.partial", { count: counts.mixed })} />
        )}
        <Legend tone="cpu" label={t("placement.onCpu", { count: counts.cpu })} />
      </div>

      <dl className="grid gap-x-6 gap-y-4 pt-1 sm:grid-cols-2">
        <KeyValue label={t("placement.weights")}>
          <span className="font-mono text-xs">
            {t("placement.weightsDetail", {
              gpu: bytes(weights.gpu),
              cpu: bytes(weights.cpu),
            })}
          </span>
        </KeyValue>
        <KeyValue label={t("placement.kvCache")}>
          {kv.length > 0 ? (
            <span className="font-mono text-xs">
              {kv.map((b) => `${b.buffer} ${bytes(b.context)}`).join(" · ")}
            </span>
          ) : (
            // Sem medida, a flag é tudo que se pode dizer - e se diz como
            // pedido, não como fato, porque numa GPU cheia o llama.cpp
            // recarrega com ela desligada.
            <span className="text-xs text-muted">
              {t(offloadKqv ? "placement.kvRequestedGpu" : "placement.kvRequestedCpu")}
            </span>
          )}
        </KeyValue>
        {/* A cabeça de saída é uma pergunta à parte: ela só vai para a GPU
            quando `n_gpu_layers` passa do número de blocos, e num pipeline é o
            último peer quem a carrega. */}
        <KeyValue label={t("placement.output")}>
          <span className="font-mono text-xs">
            {placement.output.buffers.map((b) => `${b.buffer} ${bytes(b.bytes)}`).join(" · ") ||
              placement.output.device}
          </span>
        </KeyValue>
        <KeyValue label={t("placement.devices")}>
          <span className="text-xs">
            {placement.devices.map((d) => d.description || d.name).join(" · ") ||
              t("placement.cpuOnly")}
          </span>
        </KeyValue>
      </dl>

      {placement.has_tensor_overrides && (
        <Footnote icon={<Cpu className="h-3.5 w-3.5" />}>{t("placement.overrides")}</Footnote>
      )}
      {fitApplied && (
        <Footnote icon={<Cpu className="h-3.5 w-3.5" />}>{t("placement.fit")}</Footnote>
      )}
    </div>
  );
}
