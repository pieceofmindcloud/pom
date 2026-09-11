import { Thermometer } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { num } from "../lib/format";
import { temperatureBand } from "../lib/thermal";
import type { GpuKind, ThermalPolicy } from "../lib/types";
import { Badge } from "./ui";

const FALLBACK_POLICY: ThermalPolicy = {
  limit_c: 85,
  hysteresis_c: 8,
  cooldown_secs: 30,
};

const toneByBand = {
  unknown: "neutral",
  normal: "aqua",
  attention: "amber",
  critical: "rose",
} as const;

export function GpuTemperature({
  gpuKind,
  temperature,
  policy,
  label,
  compact = false,
}: {
  gpuKind: GpuKind;
  temperature: number | null | undefined;
  policy?: ThermalPolicy;
  label?: string;
  compact?: boolean;
}) {
  const { t } = useI18n();
  if (gpuKind === "none") return null;

  const band = temperatureBand(temperature, policy ?? FALLBACK_POLICY);
  const bandLabel = {
    unknown: t("dashboard.temperatureSensorUnavailable"),
    normal: t("dashboard.temperatureNormal"),
    attention: t("dashboard.temperatureAttention"),
    critical: t("dashboard.temperatureCritical"),
  }[band];
  const value = band === "unknown"
    ? bandLabel
    : `${num(temperature, 1)} °C · ${bandLabel}`;

  return (
    <div
      className={compact
        ? "flex items-center justify-between gap-2 text-[9px]"
        // Duas placas cabem lado a lado num card de máquina, e o nome delas
        // pode carregar o índice que as distingue: o aperto de antes cortava
        // "GeForce RTX 5060 Ti #0" em "GeForce RT…".
        : "flex min-w-0 items-center justify-between gap-2 rounded-lg border border-line bg-raised/60 px-2 py-1.5 text-[0.7rem]"}
      aria-label={`${label ?? t("dashboard.gpuTemperature")}: ${value}`}
    >
      <span className="flex min-w-0 items-center gap-1.5 text-faint">
        <Thermometer className="h-3.5 w-3.5 shrink-0" />
        <span className="truncate">{label ?? t("dashboard.gpuTemperature")}</span>
      </span>
      <Badge tone={toneByBand[band]} className="shrink-0 px-1.5 py-0 text-[0.58rem]">
        {value}
      </Badge>
    </div>
  );
}
