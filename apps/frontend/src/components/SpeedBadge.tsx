import clsx from "clsx";
import { useI18n } from "../i18n/provider";
import type { SpeedRating } from "../lib/peerSpeed";
import type { ChatTelemetryPeerPerf } from "../lib/types";

/** The two grades of a machine: decode and prefill. */
export type PeerSpeed = { decode: SpeedRating; prefill: SpeedRating };

const SPEED_TONE_CLASS: Record<SpeedRating["tone"], string> = {
  fast: "border-aqua/35 bg-aqua/10 text-aqua",
  medium: "border-amber/40 bg-amber/10 text-amber",
  slow: "border-rose/45 bg-rose/10 text-rose",
  solo: "border-iris/35 bg-iris/10 text-iris",
  unknown: "border-line bg-raised/60 text-faint",
};

/**
 * Decode and prefill as badges, colored by comparison with the fastest slice of
 * the model (`rateSpeed`). The exact number and the color rule stay in the
 * `title`: on the card, the color is what has to jump out.
 */
function SpeedBadge({ kind, value, rating, compact = false }: { kind: "decode" | "prefill"; value: number | null | undefined; rating: SpeedRating; compact?: boolean }) {
  const { t } = useI18n();
  const label = t(kind === "decode" ? "canvas.speedDecode" : "canvas.speedPrefill");
  const shown = value != null && Number.isFinite(value) && value > 0 ? value.toFixed(kind === "decode" ? 1 : 0) : null;
  const hint = shown == null
    ? t("canvas.speedUnknown", { kind: label })
    : rating.ratio == null
      ? t("canvas.speedSolo", { kind: label, value: shown })
      : t("canvas.speedRelative", { kind: label, value: shown, percent: Math.round(rating.ratio * 100) });
  return <span
    data-testid={`speed-${kind}`}
    data-tone={rating.tone}
    title={hint}
    aria-label={hint}
    className={clsx("inline-flex min-w-0 items-baseline gap-1 whitespace-nowrap rounded-md border px-1.5 py-0.5 font-mono text-[10px] tabular-nums", SPEED_TONE_CLASS[rating.tone], !compact && "justify-between")}
  >
    <span className="text-[8px] font-semibold uppercase tracking-[0.12em] opacity-80">{label}</span>
    <span className="whitespace-nowrap"><span className="font-semibold">{shown ?? "-"}</span>{!compact && <span className="ml-1 text-[9px] opacity-70">tok/s</span>}</span>
  </span>;
}

export function SpeedBadges({ perf, rating, compact = false }: { perf?: ChatTelemetryPeerPerf; rating: PeerSpeed; compact?: boolean }) {
  const { t } = useI18n();
  const decode = perf?.decode_tokens_per_sec;
  const prefill = perf?.prefill_tokens_per_sec;
  if (decode == null && prefill == null) {
    return <p data-testid="machine-tps" className="font-mono text-[10px] text-faint">{t("canvas.machineTpsUnknown")}</p>;
  }
  return <div data-testid="machine-tps" className={clsx("min-w-0 gap-1.5", compact ? "flex items-center" : "grid grid-cols-2")}>
    <SpeedBadge kind="decode" value={decode} rating={rating.decode} compact={compact} />
    <SpeedBadge kind="prefill" value={prefill} rating={rating.prefill} compact={compact} />
  </div>;
}

