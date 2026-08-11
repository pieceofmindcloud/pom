/**
 * The signature of this panel: the cluster drawn as the path a request takes.
 *
 * The order on screen is the order of execution — peers are sorted by the first
 * layer they own, so the leftmost card is the one that tokenises the prompt and
 * the rightmost is the one that samples the answer. The connector between two
 * cards is the hidden state crossing the wire, and it is labelled with the layer
 * boundary it actually carries (`L15 → L16`), not with decoration.
 *
 * Underneath, one tick per layer: which machine owns it, and where the coverage
 * breaks. A gap here is exactly why a request answers `"source": "none"`.
 */
import clsx from "clsx";
import { motion } from "framer-motion";
import { ArrowRight, CircleDashed, MessageSquareText, Sparkles } from "lucide-react";
import type { ClusterView, PeerView } from "../lib/types";
import {
  isServing,
  layerRange,
  serverStateLabel,
  serverStateTone,
} from "../lib/format";
import { MachineIcon, machineKind } from "./MachineIcon";
import { Badge, Dot, Footnote } from "./ui";
import { useI18n } from "../i18n/provider";

/** Track colours, in the order peers appear along the pipeline. */
const TRACKS = [
  { fill: "bg-iris", text: "text-iris", ring: "ring-iris/30", glow: "rgb(var(--iris))" },
  { fill: "bg-aqua", text: "text-aqua", ring: "ring-aqua/30", glow: "rgb(var(--aqua))" },
  { fill: "bg-amber", text: "text-amber", ring: "ring-amber/30", glow: "rgb(var(--amber))" },
  { fill: "bg-sky", text: "text-sky", ring: "ring-sky/30", glow: "rgb(var(--sky))" },
  { fill: "bg-lime", text: "text-lime", ring: "ring-lime/30", glow: "rgb(var(--lime))" },
  { fill: "bg-rose", text: "text-rose", ring: "ring-rose/30", glow: "rgb(var(--rose))" },
];

export const trackOf = (index: number) => TRACKS[index % TRACKS.length];

/** Peers serving the model this node is on, in execution order. */
export function pipelinePeers(cluster: ClusterView): PeerView[] {
  const onModel = cluster.peers.filter((p) => p.model === cluster.active_model);
  return (onModel.length ? onModel : cluster.peers)
    .slice()
    .sort((a, b) => a.start_layer - b.start_layer || a.short_id.localeCompare(b.short_id));
}

function Terminus({
  icon,
  label,
  sublabel,
}: {
  icon: React.ReactNode;
  label: string;
  sublabel: string;
}) {
  return (
    <div className="flex w-[104px] shrink-0 flex-col items-center gap-1.5 text-center">
      <div className="flex h-9 w-9 items-center justify-center rounded-full border border-dashed border-line-strong text-faint">
        {icon}
      </div>
      <span className="text-[0.78rem] font-medium text-muted">{label}</span>
      <span className="eyebrow">{sublabel}</span>
    </div>
  );
}

/**
 * The wire between two cards, and the elastic part of the rail: the cards and
 * the two ends keep their size and the connector gives way first. Three
 * machines then still show the whole path - including the "Resposta" end, which
 * is the point of the drawing - before the panel has to scroll.
 */
function Conduit({ label, active }: { label: string | null; active: boolean }) {
  return (
    <div className="relative flex w-16 min-w-16 shrink flex-col items-center justify-center gap-2 self-center sm:w-24">
      <svg
        viewBox="0 0 96 12"
        preserveAspectRatio="none"
        className="h-3 w-full"
        aria-hidden="true"
      >
        <line
          x1="2"
          y1="6"
          x2="94"
          y2="6"
          stroke="rgb(var(--line-strong))"
          strokeWidth="1.5"
        />
        {active && (
          <line
            x1="2"
            y1="6"
            x2="94"
            y2="6"
            stroke="rgb(var(--iris))"
            strokeWidth="1.5"
            strokeDasharray="6 6"
            className="animate-flow-dash"
          />
        )}
        <path
          d="M88 2 L94 6 L88 10"
          fill="none"
          stroke="rgb(var(--line-strong))"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
      {label && (
        <span className="whitespace-nowrap font-mono text-[0.62rem] tracking-tight text-faint">
          {label}
        </span>
      )}
    </div>
  );
}

function PeerCard({
  peer,
  index,
  onSelect,
  selected,
}: {
  peer: PeerView;
  index: number;
  onSelect?: (id: string) => void;
  selected?: boolean;
}) {
  const { t } = useI18n();
  const track = trackOf(index);
  const system = peer.node?.system;
  const kind = machineKind(system);
  const serving = isServing(peer.node?.status.state);
  const unreachable = Boolean(peer.unreachable);

  return (
    <motion.button
      layout
      onClick={() => onSelect?.(peer.node_id)}
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.28, delay: index * 0.05, ease: [0.22, 1, 0.36, 1] }}
      className={clsx(
        "group relative flex w-[200px] shrink-0 flex-col gap-3 rounded-xl border p-3.5 text-left",
        "transition-[border-color,transform,box-shadow] duration-200 ease-swift",
        "hover:-translate-y-0.5 hover:shadow-lift",
        selected ? "border-iris/50 bg-raised" : "border-line bg-surface",
        unreachable && "opacity-60",
      )}
    >
      {/* The track colour is the same one the layer ribbon paints below. */}
      <span
        className={clsx("absolute inset-x-3.5 top-0 h-[2px] rounded-full", track.fill)}
      />

      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2">
          <MachineIcon system={system} />
          <span className="truncate text-[0.86rem] font-semibold text-fg">
            {peer.node?.hostname || peer.short_id}
          </span>
        </div>
        {peer.is_self && (
          <Badge tone="neutral" className="shrink-0 px-1.5 py-0 text-[0.62rem]">
            {t("pipeline.here")}
          </Badge>
        )}
      </div>

      <div className="min-w-0">
        <div className={clsx("font-mono text-[0.95rem] font-semibold", track.text)}>
          {layerRange(peer.start_layer, peer.end_layer)}
        </div>
        <div className="truncate text-[0.72rem] text-faint" title={kind.label}>
          {kind.label}
        </div>
      </div>

      <div className="flex items-center justify-between gap-2 border-t border-line pt-2.5">
        <span className="flex min-w-0 items-center gap-1.5">
          <Dot
            tone={
              unreachable ? "rose" : serverStateTone(peer.node?.status.state)
            }
            pulse={serving && !unreachable}
          />
          <span className="truncate text-[0.72rem] text-muted">
            {unreachable
              ? t("pipeline.noResponse")
              : serverStateLabel(peer.node?.status.state)}
          </span>
        </span>
        {peer.load > 0 && (
          <span className="shrink-0 font-mono text-[0.68rem] text-amber">
            {t("pipeline.inFlight", { count: peer.load })}
          </span>
        )}
      </div>
    </motion.button>
  );
}

export function LayerRibbon({ cluster }: { cluster: ClusterView }) {
  const { t } = useI18n();
  const peers = pipelinePeers(cluster);
  const total = cluster.total_layers;
  if (total <= 0) return null;

  // Which peer owns each layer; -1 where nobody does.
  const owner = new Array<number>(total).fill(-1);
  peers.forEach((p, i) => {
    for (let l = p.start_layer; l <= Math.min(p.end_layer, total - 1); l++) {
      if (owner[l] === -1) owner[l] = i;
    }
  });

  return (
    <div className="min-w-0">
      <div className="mb-2 flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <span className="eyebrow">{t("pipeline.layerCoverage")}</span>
        <span className="font-mono text-[0.72rem] text-muted">
          {cluster.covered_layers}/{total}
          {!cluster.complete && (
            <span className="ml-2 text-amber">
              {t("pipeline.missing", { count: total - cluster.covered_layers })}
            </span>
          )}
        </span>
      </div>
      <div className="flex h-6 w-full gap-[2px] overflow-hidden rounded-md">
        {owner.map((o, l) => (
          <div
            key={l}
            title={
              o === -1
                ? t("pipeline.layerNone", { layer: l })
                : t("pipeline.layerOwner", { layer: l, hostname: peers[o].node?.hostname ?? peers[o].short_id })
            }
            className={clsx(
              "min-w-0 flex-1 rounded-[2px] transition-colors duration-300",
              o === -1
                ? "bg-line ring-1 ring-inset ring-rose/40"
                : trackOf(o).fill,
            )}
            style={{ opacity: o === -1 ? 1 : 0.85 }}
          />
        ))}
      </div>
    </div>
  );
}

export function PipelineRail({
  cluster,
  onSelect,
  selected,
}: {
  cluster: ClusterView;
  onSelect?: (id: string) => void;
  selected?: string | null;
}) {
  const { t } = useI18n();
  const peers = pipelinePeers(cluster);
  const flowing =
    cluster.complete && peers.some((p) => isServing(p.node?.status.state));

  if (peers.length === 0) {
    return (
      <div className="flex flex-col items-center gap-3 py-12 text-center">
        <CircleDashed className="h-6 w-6 text-faint" />
        <p className="font-display text-[0.95rem] font-semibold text-fg">
          {t("pipeline.empty")}
        </p>
        <p className="max-w-sm text-sm text-muted">
          {t("pipeline.emptyDetail")}
        </p>
      </div>
    );
  }

  return (
    <div className="min-w-0 space-y-6">
      {/*
        Wide clusters scroll here; the page itself never does. The mask fades
        the cut edges so a chain that continues past the panel looks continued
        rather than clipped.
      */}
      <div
        className="-mx-1 overflow-x-auto px-1 pb-2"
        style={{
          maskImage:
            "linear-gradient(to right, transparent 0, #000 14px, #000 calc(100% - 28px), transparent 100%)",
          WebkitMaskImage:
            "linear-gradient(to right, transparent 0, #000 14px, #000 calc(100% - 28px), transparent 100%)",
        }}
      >
        <div className="flex items-stretch gap-1 py-1">
          <Terminus
            icon={<MessageSquareText className="h-4 w-4" />}
            label={t("pipeline.request")}
            sublabel="prompt"
          />
          <Conduit label={null} active={flowing} />
          {peers.map((peer, i) => (
            <div key={peer.node_id} className="flex items-stretch">
              <PeerCard
                peer={peer}
                index={i}
                onSelect={onSelect}
                selected={selected === peer.node_id}
              />
              {i < peers.length - 1 && (
                <Conduit
                  label={`L${peer.end_layer} → L${peers[i + 1].start_layer}`}
                  active={flowing}
                />
              )}
            </div>
          ))}
          <Conduit label={null} active={flowing} />
          <Terminus
            icon={<Sparkles className="h-4 w-4" />}
            label={t("pipeline.response")}
            sublabel="tokens"
          />
        </div>
      </div>

      <LayerRibbon cluster={cluster} />

      <Footnote icon={<ArrowRight className="h-3.5 w-3.5" />}>
        {t("pipeline.footnote")}
      </Footnote>
    </div>
  );
}
