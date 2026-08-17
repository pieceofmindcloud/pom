/**
 * The guide diagrams.
 *
 * Inline SVG rather than images: they have to survive both themes, scale to a
 * phone, and be searchable text. Colour comes from the same tokens the rest of
 * the panel uses (`fill-iris`, `stroke-line`), so a theme switch redraws them
 * for free and a screenshot is never stale.
 *
 * The lettering inside a diagram is deliberately language-neutral - machine
 * letters, layer ranges, `prompt`, `token`, byte counts. The sentence that
 * explains the picture is the caption, which is localized content. That is what
 * keeps one drawing serving both locales instead of two drifting copies.
 */
import type { DiagramId } from "./content";

const BOX = "fill-raised stroke-line";
const BOX_LIVE = "fill-iris/10 stroke-iris/50";
const LABEL = "fill-fg font-sans text-[11px] font-semibold";
const SUB = "fill-muted font-mono text-[9px]";
const FAINT = "fill-faint font-mono text-[9px]";
const FLOW = "stroke-iris/70";

function Arrow({ id }: { id: string }) {
  return (
    <marker
      id={id}
      viewBox="0 0 10 10"
      refX="9"
      refY="5"
      markerWidth="6"
      markerHeight="6"
      orient="auto-start-reverse"
    >
      <path d="M 0 0 L 10 5 L 0 10 z" className="fill-iris/70" />
    </marker>
  );
}

function Machine({
  x,
  y,
  name,
  range,
  live = true,
}: {
  x: number;
  y: number;
  name: string;
  range: string;
  live?: boolean;
}) {
  return (
    <g>
      <rect
        x={x}
        y={y}
        width="112"
        height="56"
        rx="10"
        className={live ? BOX_LIVE : BOX}
        strokeWidth="1"
      />
      <text x={x + 12} y={y + 23} className={LABEL}>
        {name}
      </text>
      <text x={x + 12} y={y + 40} className={SUB}>
        {range}
      </text>
    </g>
  );
}

function Topology() {
  return (
    <svg viewBox="0 0 520 200" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-topo" />
      </defs>
      <text x="8" y="18" className={FAINT}>
        prompt
      </text>
      <path
        d="M 8 30 L 8 58 L 60 58"
        className={FLOW}
        fill="none"
        strokeWidth="1.5"
        markerEnd="url(#a-topo)"
      />
      <Machine x={64} y={30} name="A" range="layers 0–15" />
      <Machine x={224} y={30} name="B" range="layers 16–23" />
      <Machine x={384} y={30} name="C" range="layers 24–31" />
      <path d="M 180 58 L 218 58" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-topo)" />
      <path d="M 340 58 L 378 58" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-topo)" />
      {/*
        One label for the whole row, not one per arrow: at this scale a caption
        over a 38px gap runs under the next box.
      */}
      <text x="64" y="102" className={FAINT}>
        hidden state · 8 KB per token, per hop
      </text>

      <rect x="64" y="126" width="432" height="46" rx="10" className={BOX} strokeWidth="1" />
      <text x="78" y="147" className={LABEL}>
        gateway
      </text>
      <text x="78" y="163" className={SUB}>
        every node serves the UI + API
      </text>
      <path d="M 120 112 L 120 122" className="stroke-line" strokeWidth="1" strokeDasharray="3 3" />
      <path d="M 280 112 L 280 122" className="stroke-line" strokeWidth="1" strokeDasharray="3 3" />
      <path d="M 440 112 L 440 122" className="stroke-line" strokeWidth="1" strokeDasharray="3 3" />
    </svg>
  );
}

function Request() {
  return (
    <svg viewBox="0 0 520 210" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-req" />
      </defs>
      <Machine x={16} y={40} name="A · head" range="prompt → h" />
      <Machine x={200} y={40} name="B · middle" range="h → h" />
      <Machine x={384} y={40} name="C · tail" range="h → token" />
      <path d="M 132 68 L 194 68" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-req)" />
      <path d="M 316 68 L 378 68" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-req)" />
      {/* One label on the free top row: over a 62px gap it would run under the
          next box, and under the row it collides with the return arc. */}
      <text x="300" y="26" className={FAINT}>
        → hidden state (f16), hop by hop
      </text>

      <text x="16" y="26" className={FAINT}>
        prompt in
      </text>
      <path
        d="M 440 102 C 440 156, 72 156, 72 108"
        className="stroke-aqua/70"
        fill="none"
        strokeWidth="1.5"
        strokeDasharray="4 3"
        markerEnd="url(#a-req)"
      />
      <text x="150" y="180" className="fill-muted font-mono text-[9px]">
        token → back to the entry node (N+1 crossings)
      </text>
    </svg>
  );
}

function Slicing() {
  const slices = [
    { from: 0, to: 15, gb: "1.4 GB", x: 24 },
    { from: 16, to: 23, gb: "1.8 GB", x: 200 },
    { from: 24, to: 31, gb: "1.8 GB", x: 376 },
  ];
  return (
    <svg viewBox="0 0 520 180" role="img" className="h-auto w-full">
      <rect x="24" y="16" width="472" height="26" rx="7" className={BOX} strokeWidth="1" />
      <text x="36" y="33" className={SUB}>
        model.gguf · 32 layers · 5.0 GB
      </text>
      {slices.map((s, i) => (
        <g key={s.from}>
          <path
            d={`M ${s.x + 60} 46 L ${s.x + 60} 74`}
            className="stroke-line-strong"
            strokeWidth="1"
            strokeDasharray="3 3"
          />
          <rect x={s.x} y={78} width="120" height="60" rx="10" className={BOX_LIVE} strokeWidth="1" />
          <text x={s.x + 14} y={100} className={LABEL}>
            {`slice ${i + 1}`}
          </text>
          <text x={s.x + 14} y={116} className={SUB}>
            {`layers ${s.from}–${s.to}`}
          </text>
          <text x={s.x + 14} y={130} className={FAINT}>
            {s.gb}
          </text>
        </g>
      ))}
      <text x="24" y="162" className={FAINT}>
        target in GB per machine → layer range (block sizes are read from the file)
      </text>
    </svg>
  );
}

function Lifecycle() {
  return (
    <svg viewBox="0 0 520 150" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-life" />
      </defs>
      <rect x="16" y="40" width="128" height="52" rx="10" className={BOX} strokeWidth="1" />
      <text x="30" y="62" className={LABEL}>
        pending
      </text>
      <text x="30" y="78" className={SUB}>
        apply wrote it
      </text>

      <rect x="196" y="40" width="128" height="52" rx="10" className={BOX_LIVE} strokeWidth="1" />
      <text x="210" y="62" className={LABEL}>
        serving
      </text>
      <text x="210" y="78" className={SUB}>
        /health confirmed
      </text>

      <rect x="376" y="40" width="128" height="52" rx="10" className={BOX} strokeWidth="1" />
      <text x="390" y="62" className={LABEL}>
        cleared
      </text>
      <text x="390" y="78" className={SUB}>
        stop, or gave up
      </text>

      <path d="M 148 66 L 190 66" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-life)" />
      <path d="M 328 66 L 370 66" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-life)" />
      <text x="16" y="122" className={FAINT}>
        only “serving” is resumed after a restart
      </text>
    </svg>
  );
}

const DIAGRAMS: Record<DiagramId, () => JSX.Element> = {
  topology: Topology,
  request: Request,
  slicing: Slicing,
  lifecycle: Lifecycle,
};

export function Diagram({ id }: { id: DiagramId }) {
  const D = DIAGRAMS[id];
  return D ? <D /> : null;
}
