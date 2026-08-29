type RouteRttLabelProps = {
  rttMs: number | null | undefined;
  left: number;
  top: number;
};

/** O selo de RTT compartilhado pelos mapas de implantação e do Painel. */
export function RouteRttLabel({ rttMs, left, top }: RouteRttLabelProps) {
  return (
    <span
      className="absolute z-10 -translate-x-1/2 rounded-md border border-aqua/25 bg-ink/95 px-1.5 py-0.5 font-mono text-[10px] text-aqua"
      style={{ left, top }}
    >
      {rttMs == null ? "RTT -" : `RTT ${rttMs.toFixed(1)} ms`}
    </span>
  );
}
