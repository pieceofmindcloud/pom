import { Thermometer } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { num } from "../lib/format";
import { temperatureBand } from "../lib/thermal";
import type { ThermalPolicy } from "../lib/types";
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
  temperature,
  policy,
  label,
  compact = false,
}: {
  temperature: number | null | undefined;
  policy?: ThermalPolicy;
  label?: string;
  compact?: boolean;
}) {
  const { t } = useI18n();
  const band = temperatureBand(temperature, policy ?? FALLBACK_POLICY);
  const value = band === "unknown"
    ? t("dashboard.temperatureUnknown")
    : `${num(temperature, 1)} °C`;
  const bandLabel = {
    unknown: t("dashboard.temperatureUnknown"),
    normal: t("dashboard.temperatureNormal"),
    attention: t("dashboard.temperatureAttention"),
    critical: t("dashboard.temperatureCritical"),
  }[band];

  return (
    <div
      className={compact
        ? "mt-2 flex items-center justify-between gap-2 text-[10px]"
        : "flex min-w-0 items-center justify-between gap-3 rounded-lg border border-line bg-raised/60 px-3 py-2"}
      aria-label={`${label ?? t("dashboard.gpuTemperature")}: ${value}`}
    >
      <span className="flex min-w-0 items-center gap-1.5 text-faint">
        <Thermometer className="h-3.5 w-3.5 shrink-0" />
        <span className="truncate">{label ?? t("dashboard.gpuTemperature")}</span>
      </span>
      <Badge tone={toneByBand[band]} className="shrink-0 px-1.5 py-0 text-[0.65rem]">
        {value} · {bandLabel}
      </Badge>
    </div>
  );
}
