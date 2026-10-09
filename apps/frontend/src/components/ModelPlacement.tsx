/**
 * How llama.cpp actually loaded the model on this machine.
 *
 * What the operator wants to know when opening a card is what the configuration does not
 * answer: which layers stayed on the GPU, which were left on the CPU, and where the KV
 * cache ended up. `n_gpu_layers` is the **request**; between the request and the result
 * are the `fit`, which reduces the number on a full GPU, and the buffer type
 * overrides, which send part of a layer - the experts of a MoE, typically -
 * to the CPU without changing the block's device.
 *
 * That is why the classification below reads the **weight buffers**, not the `device`
 * field. A layer with 90% of its bytes on the GPU and the experts on the CPU appears
 * as mixed, which is what it is. Deriving the range from `n_gpu_layers` would show
 * it as entirely on the GPU, and the memory math of whoever reads would not add up.
 */
import clsx from "clsx";
import { Cpu } from "lucide-react";
import { bytes } from "../lib/format";
import type { BufferBytes, ContextMemoryBuffer, LayerPlacement, ModelPlacement } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { Footnote, KeyValue } from "./ui";

export type Placement = "gpu" | "cpu" | "mixed" | "unknown";

/**
 * A buffer is a CPU one by the name ggml itself gives it.
 *
 * The backends name the buffer with the device name (`CUDA0`, `Metal`,
 * `Vulkan0`), and only the host is called `CPU` - with the `_Mapped` suffix when the weight
 * came from `mmap` of the file, which is the common case of a non-offloaded layer.
 */
export function isCpuBuffer(name: string): boolean {
  return name === "CPU" || name.startsWith("CPU_");
}

/** Where a group's weights ended up, by the bytes in each buffer. */
export function placementOf(buffers: BufferBytes[]): Placement {
  const total = buffers.reduce((sum, b) => sum + b.bytes, 0);
  if (total === 0) return "unknown";
  const cpu = buffers.filter((b) => isCpuBuffer(b.buffer)).reduce((sum, b) => sum + b.bytes, 0);
  if (cpu === 0) return "gpu";
  if (cpu === total) return "cpu";
  return "mixed";
}

/*
 * The colors come from the product tokens (`tailwind.config.js`), not from Tailwind's
 * numeric palette: here `lime` and `amber` are flat colors, so
 * `bg-lime-400` does not exist and Tailwind generated no class at all - the layer
 * strip came out invisible, an empty band in the middle of the card.
 */
const CELL: Record<Placement, string> = {
  gpu: "bg-lime/80",
  cpu: "bg-line-strong",
  mixed: "bg-amber/80",
  unknown: "bg-line",
};

/** How many bytes of a group are in a CPU buffer and how many are not. */
function split(buffers: BufferBytes[]): { cpu: number; gpu: number } {
  let cpu = 0;
  let gpu = 0;
  for (const b of buffers) {
    if (isCpuBuffer(b.buffer)) cpu += b.bytes;
    else gpu += b.bytes;
  }
  return { cpu, gpu };
}

/** The summary of a layer, for the cell's `title`. */
function layerTitle(layer: LayerPlacement): string {
  const where = layer.buffers.map((b) => `${b.buffer} ${bytes(b.bytes)}`).join(" · ");
  return `${layer.layer}: ${where || layer.device}`;
}

/**
 * The layer strip: one cell per block, in the order the model runs them.
 *
 * A strip, and not a list: with 80 layers the question being asked is about the
 * *shape* of the cut - "how many were left out and where" - and that shape is read at a
 * glance in a band, never in eighty lines of text.
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
 * Where the KV cache ended up, by the context bytes of each buffer.
 *
 * `context` in llama.cpp's breakdown is the KV cache (plus the recurrent state,
 * in a hybrid model). A buffer with `context` zeroed holds no cache at all and
 * does not enter the sentence.
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
  /** What was **requested** for the KV cache. It only appears in the absence of the measurement. */
  offloadKqv: boolean;
  fitApplied: boolean;
}) {
  const { t } = useI18n();

  if (!placement) {
    return (
      <div className="border-t border-line px-3 py-2.5">
        <div className="eyebrow mb-1.5">{t("placement.title")}</div>
        <p className="text-[0.7rem] leading-relaxed text-muted">{t("placement.unavailable")}</p>
      </div>
    );
  }

  const counts = { gpu: 0, cpu: 0, mixed: 0, unknown: 0 };
  for (const layer of placement.layers) counts[placementOf(layer.buffers)]++;
  const weights = split(placement.totals);
  const kv = contextMemory ? kvCacheBuffers(contextMemory) : [];

  return (
    <div className="space-y-2 border-t border-line px-3 py-2.5">
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <div className="eyebrow">{t("placement.title")}</div>
        <span className="font-mono text-[0.7rem] text-muted">
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

      <dl className="grid gap-x-3 gap-y-2 pt-0.5 sm:grid-cols-2">
        <KeyValue compact label={t("placement.weights")}>
          <span className="font-mono text-[0.7rem]">
            {t("placement.weightsDetail", {
              gpu: bytes(weights.gpu),
              cpu: bytes(weights.cpu),
            })}
          </span>
        </KeyValue>
        <KeyValue compact label={t("placement.kvCache")}>
          {kv.length > 0 ? (
            <span className="font-mono text-[0.7rem]">
              {kv.map((b) => `${b.buffer} ${bytes(b.context)}`).join(" · ")}
            </span>
          ) : (
            // Without a measurement, the flag is all that can be said - and it is said as
            // requested, not as fact, because on a full GPU llama.cpp
            // reloads with it turned off.
            <span className="text-[0.7rem] text-muted">
              {t(offloadKqv ? "placement.kvRequestedGpu" : "placement.kvRequestedCpu")}
            </span>
          )}
        </KeyValue>
        {/* The output head is a separate question: it only goes to the GPU
            when `n_gpu_layers` exceeds the number of blocks, and in a pipeline it is the
            last peer that loads it. */}
        <KeyValue compact label={t("placement.output")}>
          <span className="font-mono text-[0.7rem]">
            {placement.output.buffers.map((b) => `${b.buffer} ${bytes(b.bytes)}`).join(" · ") ||
              placement.output.device}
          </span>
        </KeyValue>
        <KeyValue compact label={t("placement.devices")}>
          <span className="text-[0.7rem]">
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
