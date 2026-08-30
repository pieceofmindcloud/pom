import type { ThermalPolicy } from "./types";

export type TemperatureBand = "unknown" | "normal" | "attention" | "critical";

export function highestKnownTemperature(
  readings: Array<number | null | undefined>,
): number | null {
  const known = readings.filter((reading): reading is number =>
    reading != null && Number.isFinite(reading),
  );
  return known.length > 0 ? Math.max(...known) : null;
}

export function temperatureBand(
  temperature: number | null | undefined,
  policy: ThermalPolicy,
): TemperatureBand {
  if (temperature == null || !Number.isFinite(temperature)) return "unknown";
  if (temperature >= policy.limit_c) return "critical";
  if (temperature >= policy.limit_c - policy.hysteresis_c) return "attention";
  return "normal";
}
