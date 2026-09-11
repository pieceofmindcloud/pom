type RouteRttLabelProps = {
  rttMs: number | null | undefined;
  transport?: string | null;
  left: number;
  top: number;
};

/** O selo de RTT compartilhado pelos mapas de implantação e do Painel. */
export function RouteRttLabel({ rttMs, transport, left, top }: RouteRttLabelProps) {
  const transportLabel = transport
    ? transport === "quic"
      ? "QUIC"
      : transport === "https"
        ? "HTTPS"
        : transport === "http"
          ? "HTTP"
          : transport
    : null;
  return (
    <span
      className="absolute z-10 -translate-x-1/2 rounded-md border border-aqua/25 bg-ink/95 px-1.5 py-0.5 font-mono text-[10px] text-aqua"
      style={{ left, top }}
    >
      {transportLabel && <span className="mr-1 text-[9px] uppercase text-faint">{transportLabel}</span>}
      <span>{rttMs == null ? "RTT -" : `RTT ${rttMs.toFixed(1)} ms`}</span>
    </span>
  );
}
