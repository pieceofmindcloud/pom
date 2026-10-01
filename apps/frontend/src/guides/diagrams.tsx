/**
 * The guide diagrams.
 *
 * Inline SVG rather than images: they have to survive both themes, scale to a
 * phone, and be searchable text. Colour comes from the same tokens the rest of
 * the panel uses (`fill-iris`, `stroke-line`), so a theme switch redraws them
 * for free and a screenshot is never stale.
 *
 * Motion explains the flow - a hidden state crossing a hop, a token coming
 * back, an invitation reaching a new machine. It is plain CSS (`guide-*` in
 * `index.css`), so the global `prefers-reduced-motion` rule freezes it and the
 * drawing still reads on its own: every animated packet sits on a static arrow.
 *
 * The lettering inside a diagram is deliberately language-neutral - machine
 * letters, layer ranges, `prompt`, `token`, byte counts. The sentence that
 * explains the picture is the caption, which is localized content. That is what
 * keeps one drawing serving both locales instead of two drifting copies.
 */
import type { CSSProperties } from "react";
import type { DiagramId } from "./content";

const BOX = "fill-raised stroke-line";
const BOX_LIVE = "fill-iris/10 stroke-iris/50";
const BOX_AQUA = "fill-aqua/10 stroke-aqua/50";
const BOX_AMBER = "fill-amber/10 stroke-amber/50";
const LABEL = "fill-fg font-sans text-[11px] font-semibold";
const SUB = "fill-muted font-mono text-[9px]";
const FAINT = "fill-faint font-mono text-[9px]";
const FLOW = "stroke-iris/70";

function Arrow({ id, tone = "iris" }: { id: string; tone?: "iris" | "aqua" | "amber" }) {
  const fill = tone === "aqua" ? "fill-aqua/70" : tone === "amber" ? "fill-amber/70" : "fill-iris/70";
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
      <path d="M 0 0 L 10 5 L 0 10 z" className={fill} />
    </marker>
  );
}

/** A dot that travels `dx`,`dy` from (`x`,`y`) and fades, on a loop. */
function Packet({
  x,
  y,
  dx,
  dy = 0,
  delay = 0,
  dur = 2.6,
  tone = "iris",
  r = 4,
}: {
  x: number;
  y: number;
  dx: number;
  dy?: number;
  delay?: number;
  dur?: number;
  tone?: "iris" | "aqua" | "amber";
  r?: number;
}) {
  const fill = tone === "aqua" ? "fill-aqua" : tone === "amber" ? "fill-amber" : "fill-iris";
  const style = {
    "--dx": `${dx}px`,
    "--dy": `${dy}px`,
    "--delay": `${delay}s`,
    "--dur": `${dur}s`,
  } as CSSProperties;
  return (
    <g className="guide-packet" style={style}>
      <circle cx={x} cy={y} r={r} className={fill} />
    </g>
  );
}

function delayStyle(delay: number): CSSProperties {
  return { "--delay": `${delay}s` } as CSSProperties;
}

function Machine({
  x,
  y,
  name,
  range,
  tone = "live",
  w = 112,
  h = 56,
  pulse,
}: {
  x: number;
  y: number;
  name: string;
  range: string;
  tone?: "live" | "plain" | "aqua" | "amber";
  w?: number;
  h?: number;
  /** Seconds of delay for a soft pulse; undefined means a still box. */
  pulse?: number;
}) {
  const cls =
    tone === "plain" ? BOX : tone === "aqua" ? BOX_AQUA : tone === "amber" ? BOX_AMBER : BOX_LIVE;
  return (
    <g>
      <rect
        x={x}
        y={y}
        width={w}
        height={h}
        rx="10"
        className={pulse === undefined ? cls : `${cls} guide-pulse`}
        style={pulse === undefined ? undefined : delayStyle(pulse)}
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
      <Machine x={64} y={30} name="A" range="layers 0–15" pulse={0} />
      <Machine x={224} y={30} name="B" range="layers 16–23" pulse={0.8} />
      <Machine x={384} y={30} name="C" range="layers 24–31" pulse={1.6} />
      <path d="M 180 58 L 218 58" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-topo)" />
      <path d="M 340 58 L 378 58" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-topo)" />
      <Packet x={180} y={58} dx={34} delay={0.4} dur={1.6} />
      <Packet x={340} y={58} dx={34} delay={1.2} dur={1.6} />
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
      <path d="M 120 112 L 120 122" className="stroke-line guide-dash" strokeWidth="1" />
      <path d="M 280 112 L 280 122" className="stroke-line guide-dash" strokeWidth="1" />
      <path d="M 440 112 L 440 122" className="stroke-line guide-dash" strokeWidth="1" />
    </svg>
  );
}

function Request() {
  return (
    <svg viewBox="0 0 520 210" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-req" />
        <Arrow id="a-req-back" tone="aqua" />
      </defs>
      <Machine x={16} y={40} name="A · head" range="prompt → h" pulse={0} />
      <Machine x={200} y={40} name="B · middle" range="h → h" pulse={0.9} />
      <Machine x={384} y={40} name="C · tail" range="h → token" pulse={1.8} />
      <path d="M 132 68 L 194 68" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-req)" />
      <path d="M 316 68 L 378 68" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-req)" />
      <Packet x={132} y={68} dx={58} delay={0} dur={1.8} />
      <Packet x={316} y={68} dx={58} delay={0.9} dur={1.8} />
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
        className="stroke-aqua/70 guide-dash"
        fill="none"
        strokeWidth="1.5"
        markerEnd="url(#a-req-back)"
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
      {/* the cut points on the source file */}
      <path d="M 200 12 L 200 46" className="stroke-amber/80 guide-pulse" strokeWidth="1.5" />
      <path d="M 376 12 L 376 46" className="stroke-amber/80 guide-pulse" style={delayStyle(0.6)} strokeWidth="1.5" />
      {slices.map((s, i) => (
        <g key={s.from}>
          <path
            d={`M ${s.x + 60} 46 L ${s.x + 60} 74`}
            className="stroke-line-strong guide-dash"
            strokeWidth="1"
          />
          <Packet x={s.x + 60} y={46} dx={0} dy={26} delay={i * 0.5} dur={2} r={3} tone="amber" />
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
      <rect x="16" y="40" width="128" height="52" rx="10" className={`${BOX} guide-pulse`} style={delayStyle(0)} strokeWidth="1" />
      <text x="30" y="62" className={LABEL}>
        pending
      </text>
      <text x="30" y="78" className={SUB}>
        apply wrote it
      </text>

      <rect x="196" y="40" width="128" height="52" rx="10" className={`${BOX_LIVE} guide-pulse`} style={delayStyle(0.8)} strokeWidth="1" />
      <text x="210" y="62" className={LABEL}>
        serving
      </text>
      <text x="210" y="78" className={SUB}>
        /health confirmed
      </text>

      <rect x="376" y="40" width="128" height="52" rx="10" className={`${BOX} guide-pulse`} style={delayStyle(1.6)} strokeWidth="1" />
      <text x="390" y="62" className={LABEL}>
        cleared
      </text>
      <text x="390" y="78" className={SUB}>
        stop, or gave up
      </text>

      <path d="M 148 66 L 190 66" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-life)" />
      <path d="M 328 66 L 370 66" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-life)" />
      <Packet x={148} y={66} dx={38} delay={0.4} dur={1.6} r={3} />
      <Packet x={328} y={66} dx={38} delay={1.2} dur={1.6} r={3} />
      <text x="16" y="122" className={FAINT}>
        only “serving” is resumed after a restart
      </text>
    </svg>
  );
}

/** First access: the administrator's invitation reaches a joining machine. */
function Setup() {
  return (
    <svg viewBox="0 0 520 200" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-setup" />
        <Arrow id="a-setup-aqua" tone="aqua" />
      </defs>
      <Machine x={16} y={64} name="A · admin" range="network + invite" w={128} h={60} pulse={0} />
      {/* the invitation */}
      <rect x="196" y="20" width="128" height="40" rx="8" className={BOX_AMBER} strokeWidth="1" />
      <text x="208" y="37" className={LABEL}>
        invite
      </text>
      <text x="208" y="51" className={SUB}>
        key + trust.json
      </text>
      <path d="M 144 84 C 168 84, 172 40, 192 40" className="stroke-amber/70" fill="none" strokeWidth="1.5" />
      <path d="M 324 40 C 348 40, 352 84, 372 84" className="stroke-amber/70" fill="none" strokeWidth="1.5" />

      <Machine x={376} y={64} name="B · node" range="joining → connected" w={128} h={60} tone="aqua" pulse={1.2} />

      {/* local discovery: the joining machine's probes */}
      <circle cx="488" cy="80" r="10" className="fill-none stroke-aqua/50 guide-ring" strokeWidth="1" />
      <circle cx="488" cy="80" r="10" className="fill-none stroke-aqua/50 guide-ring" style={delayStyle(1.4)} strokeWidth="1" />
      <circle cx="488" cy="80" r="3" className="fill-aqua" />

      <path d="M 376 112 C 300 160, 220 160, 144 112" className="stroke-aqua/70 guide-dash" fill="none" strokeWidth="1.5" markerEnd="url(#a-setup-aqua)" />
      <Packet x={144} y={84} dx={48} dy={-44} delay={0} dur={1.6} tone="amber" r={3.5} />
      <Packet x={324} y={40} dx={48} dy={44} delay={1.2} dur={1.6} tone="amber" r={3.5} />
      <text x="196" y="186" className={FAINT}>
        local discovery finds the admin · or paste the invite
      </text>
    </svg>
  );
}

/** What each profile reaches: the network, its own machine, the chat. */
function Profiles() {
  return (
    <svg viewBox="0 0 520 210" role="img" className="h-auto w-full">
      {/* admin: the whole network */}
      <rect x="12" y="14" width="496" height="182" rx="14" className="fill-iris/[0.04] stroke-iris/40" strokeDasharray="5 4" strokeWidth="1" />
      <text x="26" y="34" className={LABEL}>
        admin
      </text>
      <text x="70" y="34" className={SUB}>
        deploy · add machines · policies
      </text>

      {/* node: one machine */}
      <rect x="32" y="52" width="210" height="124" rx="12" className={`${BOX_AQUA} guide-pulse`} style={delayStyle(0)} strokeWidth="1" />
      <text x="46" y="74" className={LABEL}>
        node
      </text>
      <text x="46" y="90" className={SUB}>
        its own machine
      </text>
      <rect x="46" y="104" width="182" height="20" rx="5" className={BOX} strokeWidth="1" />
      <text x="56" y="118" className={FAINT}>
        slice · layers 16–31
      </text>
      <rect x="46" y="132" width="182" height="20" rx="5" className={BOX} strokeWidth="1" />
      <text x="56" y="146" className={FAINT}>
        plugins · API key · leave
      </text>

      {/* infer: chat */}
      <rect x="276" y="52" width="210" height="124" rx="12" className={`${BOX_AMBER} guide-pulse`} style={delayStyle(1.2)} strokeWidth="1" />
      <text x="290" y="74" className={LABEL}>
        infer
      </text>
      <text x="290" y="90" className={SUB}>
        talk to the models
      </text>
      <rect x="290" y="104" width="120" height="20" rx="10" className={BOX} strokeWidth="1" />
      <text x="300" y="118" className={FAINT}>
        prompt
      </text>
      <rect x="350" y="132" width="122" height="20" rx="10" className={BOX_LIVE} strokeWidth="1" />
      <text x="360" y="146" className={FAINT}>
        answer
      </text>
      <Packet x={410} y={114} dx={0} dy={22} delay={0.6} dur={2} r={3} tone="amber" />
    </svg>
  );
}

/** Two sites: UDP on the LAN, libp2p with a relay across the internet. */
function Network() {
  return (
    <svg viewBox="0 0 520 224" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-net" />
      </defs>
      {/* LAN 1 */}
      <rect x="10" y="20" width="170" height="176" rx="14" className="fill-none stroke-line" strokeDasharray="4 4" strokeWidth="1" />
      <text x="22" y="38" className={FAINT}>
        LAN · UDP 42777
      </text>
      <Machine x={24} y={50} name="A" range="gateway :8080" w={140} h={50} />
      <Machine x={24} y={130} name="B" range="infer :8181" w={140} h={50} />
      <circle cx="56" cy="115" r="9" className="fill-none stroke-iris/50 guide-ring" strokeWidth="1" />
      <circle cx="56" cy="115" r="9" className="fill-none stroke-iris/50 guide-ring" style={delayStyle(1.4)} strokeWidth="1" />
      <circle cx="56" cy="115" r="2.5" className="fill-iris" />
      <text x="72" y="118" className={FAINT}>
        broadcast
      </text>

      {/* internet */}
      <ellipse cx="260" cy="108" rx="58" ry="34" className="fill-raised stroke-line" strokeWidth="1" />
      <text x="232" y="104" className={LABEL}>
        internet
      </text>
      <text x="226" y="120" className={SUB}>
        relay · DCuTR
      </text>

      {/* LAN 2 */}
      <rect x="340" y="20" width="170" height="176" rx="14" className="fill-none stroke-line" strokeDasharray="4 4" strokeWidth="1" />
      <text x="352" y="38" className={FAINT}>
        behind NAT
      </text>
      <Machine x={354} y={84} name="C" range="libp2p TCP 52777" w={140} h={50} tone="aqua" pulse={0.6} />

      <path d="M 164 75 L 200 98" className="stroke-iris/60 guide-dash" fill="none" strokeWidth="1.5" />
      <path d="M 318 108 L 350 108" className="stroke-iris/60 guide-dash" fill="none" strokeWidth="1.5" markerEnd="url(#a-net)" />
      <Packet x={164} y={75} dx={36} dy={23} delay={0} dur={1.8} r={3} />
      <Packet x={318} y={108} dx={30} delay={0.9} dur={1.8} r={3} />
      <text x="150" y="214" className={FAINT}>
        --bootstrap host:42777 · advertised address
      </text>
    </svg>
  );
}

/** A client call: any gateway, the pipeline, and the proof in x_pom.source. */
function Api() {
  return (
    <svg viewBox="0 0 520 210" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-api" />
        <Arrow id="a-api-back" tone="aqua" />
      </defs>
      <rect x="10" y="50" width="96" height="56" rx="10" className={BOX} strokeWidth="1" />
      <text x="22" y="73" className={LABEL}>
        client
      </text>
      <text x="22" y="89" className={SUB}>
        API key
      </text>

      <Machine x={150} y={50} name="gateway" range="/v1 · any node" w={110} pulse={0} />
      <Machine x={300} y={20} name="A" range="layers 0–15" w={96} h={50} />
      <Machine x={300} y={90} name="B" range="layers 16–31" w={96} h={50} pulse={1} />

      <path d="M 106 78 L 144 78" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-api)" />
      <Packet x={106} y={78} dx={34} delay={0} dur={1.6} r={3} />
      <path d="M 260 66 L 294 50" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-api)" />
      <path d="M 348 70 L 348 86" className={FLOW} fill="none" strokeWidth="1.5" markerEnd="url(#a-api)" />
      <Packet x={348} y={70} dx={0} dy={14} delay={0.8} dur={1.4} r={3} />
      <path d="M 300 120 C 230 150, 120 150, 64 110" className="stroke-aqua/70 guide-dash" fill="none" strokeWidth="1.5" markerEnd="url(#a-api-back)" />

      <rect x="410" y="40" width="102" height="96" rx="10" className="fill-ink stroke-line" strokeWidth="1" />
      <text x="420" y="60" className={FAINT}>
        {"{"}
      </text>
      <text x="428" y="76" className={SUB}>
        "x_pom": {"{"}
      </text>
      <text x="434" y="92" className="fill-aqua font-mono text-[8px]">
        "source":
      </text>
      <text x="434" y="106" className="fill-aqua font-mono text-[8px] guide-pulse">
        "pipeline:A→B"
      </text>
      <text x="420" y="126" className={FAINT}>
        {"}}"}
      </text>
      <text x="64" y="186" className={FAINT}>
        two or more hops in source = joint inference
      </text>
    </svg>
  );
}

/** Plugins: one process per plugin, beside the node, activated per machine. */
function Plugins() {
  return (
    <svg viewBox="0 0 520 200" role="img" className="h-auto w-full">
      <defs>
        <Arrow id="a-plug" tone="amber" />
      </defs>
      <rect x="10" y="14" width="500" height="150" rx="14" className="fill-none stroke-line" strokeDasharray="4 4" strokeWidth="1" />
      <text x="22" y="32" className={FAINT}>
        one machine
      </text>
      <Machine x={28} y={56} name="pom" range="node + UI" w={120} h={60} />

      <Machine x={230} y={44} name="plugin-host" range="plugin A" w={124} h={50} tone="aqua" pulse={0} />
      <Machine x={230} y={104} name="plugin-host" range="plugin B" w={124} h={50} tone="aqua" pulse={1.2} />
      <path d="M 148 80 L 224 69" className="stroke-aqua/60 guide-dash" fill="none" strokeWidth="1.2" />
      <path d="M 148 92 L 224 129" className="stroke-aqua/60 guide-dash" fill="none" strokeWidth="1.2" />

      <rect x="400" y="54" width="96" height="44" rx="8" className={BOX_AMBER} strokeWidth="1" />
      <text x="412" y="72" className={LABEL}>
        license
      </text>
      <text x="412" y="88" className={SUB}>
        or GitHub
      </text>
      <path d="M 400 76 L 360 70" className="stroke-amber/70" fill="none" strokeWidth="1.5" markerEnd="url(#a-plug)" />
      <Packet x={400} y={76} dx={-36} dy={-5} delay={0.3} dur={1.8} tone="amber" r={3} />
      <text x="22" y="186" className={FAINT}>
        each plugin is its own process · independent licenses
      </text>
    </svg>
  );
}

const DIAGRAMS: Record<DiagramId, () => JSX.Element> = {
  topology: Topology,
  request: Request,
  slicing: Slicing,
  lifecycle: Lifecycle,
  setup: Setup,
  profiles: Profiles,
  network: Network,
  api: Api,
  plugins: Plugins,
};

export function Diagram({ id }: { id: DiagramId }) {
  const D = DIAGRAMS[id];
  return D ? <D /> : null;
}
