import { Info } from "lucide-react";
import { useI18n } from "../i18n/provider";

type RouteRttLabelProps = {
  rttMs: number | null | undefined;
  transport?: string | null;
  /** The edge goes through a circuit on a relay: orange highlight and explanation. */
  relayed?: boolean;
  left: number;
  top: number;
};

/** The RTT badge shared by the deployment maps and the Dashboard's. */
export function RouteRttLabel({ rttMs, transport, relayed = false, left, top }: RouteRttLabelProps) {
  const { t } = useI18n();
  const transportLabel = relayed
    ? t("canvas.relayBadge")
    : transport
      ? transport === "quic"
        ? "QUIC"
        : transport === "https"
          ? "HTTPS"
          : transport === "http"
            ? "HTTP"
            : transport
      : null;
  const rtt = rttMs == null ? "RTT -" : `RTT ${rttMs.toFixed(1)} ms`;
  if (relayed) {
    return (
      <span
        data-testid="route-relay-label"
        title={t("canvas.relayEdgeHint")}
        aria-label={`${t("canvas.relayBadge")} ${rtt}. ${t("canvas.relayEdgeHint")}`}
        className="absolute z-10 inline-flex -translate-x-1/2 cursor-help items-center gap-1 rounded-md border-2 border-relay bg-relay px-1.5 py-0.5 font-mono text-[10px] font-semibold text-white shadow-[0_0_12px_rgb(var(--relay)/0.55)]"
        style={{ left, top }}
      >
        <Info className="h-3 w-3" aria-hidden="true" />
        <span className="text-[9px] uppercase tracking-[0.08em]">{transportLabel}</span>
        <span>{rtt}</span>
      </span>
    );
  }
  return (
    <span
      className="absolute z-10 -translate-x-1/2 rounded-md border border-aqua/25 bg-ink/95 px-1.5 py-0.5 font-mono text-[10px] text-aqua"
      style={{ left, top }}
    >
      {transportLabel && <span className="mr-1 text-[9px] uppercase text-faint">{transportLabel}</span>}
      <span>{rtt}</span>
    </span>
  );
}
