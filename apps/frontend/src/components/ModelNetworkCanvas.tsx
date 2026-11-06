import clsx from "clsx";
import { LayoutGrid, Maximize2, Minimize2, Minus, PanelRightClose, PanelRightOpen, Plus, RotateCcw } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { gpuDisplayName, machineKind, MachineIcon } from "./MachineIcon";
import { PeerDetailDialog } from "./PeerDetailCard";
import { GpuTemperature } from "./GpuTemperature";
import { AlertTooltip, Button, Dot } from "./ui";
import { canSeePeerDetails } from "../lib/access";
import { canvasModelIds, graphFromCluster, selectedCanvasModel, type GraphRange } from "../lib/networkGraph";
import { pipelinePeers } from "../lib/networkPeers";
import { useProfile } from "../lib/profile";
import { useI18n } from "../i18n/provider";
import {
  autoLayout,
  CARD_HEIGHT,
  CARD_WIDTH,
  clearPositions,
  COMPACT_CARD_HEIGHT,
  COMPACT_CARD_SIZE,
  COMPACT_CARD_WIDTH,
  deploymentLayoutKey,
  fallbackPosition,
  GATEWAY_LAYOUT_ID,
  GATEWAY_SIZE,
  gatewayRouteGeometry,
  isCardCompact,
  pipelineGatewayPosition,
  readCardModes,
  readPositions,
  routeGeometry,
  writeCardModes,
  writePositions,
  type CanvasPosition,
  type CardModes,
  type CardSize,
} from "../lib/networkLayout";
import { rateSpeed } from "../lib/peerSpeed";
import { SpeedBadges, type PeerSpeed } from "./SpeedBadge";
import { isServing, serverStateLabel, serverStateTone } from "../lib/format";
import { sumVram } from "../lib/gpu";
import { layerPlacement, warnsLayersOnCpu, type LayerPlacement } from "../lib/layerPlacement";
import type { ChatTelemetryPeerPerf, ChatTelemetryRtt, ClusterView, PluginService, RoutingEdge } from "../lib/types";
import { RouteRttLabel } from "./RouteRttLabel";
import { PipelineGatewayNode, type PipelineCycleCost, type PipelineRouteCost } from "./PipelineGatewayNode";
import { PluginServiceTether, SERVICE_TETHER_HEIGHT, servicesInUse } from "./PluginServices";

export type EdgeState = "principal" | "paralela" | "inativa";

/** Three states, because with replicas "chosen or not" no longer sufficed:
 *  an edge may not be in use and still be a ready path. */
/** Edge that goes through a relay. */
const RELAY_EDGE_STYLE = { className: "fill-none stroke-relay", strokeWidth: "4", strokeDasharray: "10 4", markerEnd: "url(#route-arrow-relay)" };

export function edgeStyle(state: EdgeState) {
  if (state === "principal") {
    return { className: "fill-none stroke-aqua", strokeWidth: "3", strokeDasharray: undefined, markerEnd: "url(#route-arrow)" };
  }
  if (state === "paralela") {
    return { className: "fill-none stroke-iris", strokeWidth: "2", strokeDasharray: undefined, markerEnd: undefined };
  }
  return { className: "fill-none stroke-faint/70", strokeWidth: "2", strokeDasharray: "7 7", markerEnd: undefined };
}

function keyFor(range: Pick<GraphRange, "node_id" | "model_id" | "start_layer" | "end_layer">) {
  return `${range.node_id}:${range.model_id}:${range.start_layer}:${range.end_layer}`;
}

function edgeKey(edge: Pick<RoutingEdge, "from_node_id" | "to_node_id" | "from_end_layer" | "to_start_layer">) {
  return `${edge.from_node_id}:${edge.to_node_id}:${edge.from_end_layer}:${edge.to_start_layer}`;
}

function rangeForEdge(ranges: GraphRange[], edge: RoutingEdge, side: "from" | "to") {
  const id = side === "from" ? edge.from_node_id : edge.to_node_id;
  const boundary = side === "from" ? edge.from_end_layer : edge.to_start_layer;
  return ranges.find((range) => range.node_id === id && (side === "from" ? range.end_layer === boundary : range.start_layer === boundary))
    ?? ranges.find((range) => range.node_id === id);
}

function Meter({ label, used, total, unknown = false, detail }: { label: string; used: number | null; total: number | null; unknown?: boolean; detail?: string }) {
  const { t } = useI18n();
  const percent = used != null && total != null && total > 0 ? Math.min(100, (used / total) * 100) : null;
  const tone = percent != null && percent >= 90 ? "bg-rose" : percent != null && percent >= 70 ? "bg-amber" : "bg-aqua";
  const value = detail ?? (unknown || percent == null ? t("canvas.meterUnknown") : `${used!.toFixed(1)} / ${total!.toFixed(1)} GiB`);
  return <div>
    <div className="mb-1 flex justify-between gap-2 text-[9px] uppercase tracking-[0.12em] text-faint"><span>{label}</span><span className="normal-case tracking-normal text-muted">{value}</span></div>
    <div className={clsx("h-1.5 overflow-hidden rounded-full bg-line", (unknown || percent == null) && "bg-[repeating-linear-gradient(135deg,transparent_0,transparent_4px,rgba(124,135,165,.3)_4px,rgba(124,135,165,.3)_6px)]")} aria-label={`${label}: ${value}`}>
      {percent != null && <div className={clsx("h-full rounded-full", tone)} style={{ width: `${percent}%` }} />}
    </div>
  </div>;
}

/**
 * How much of what this machine serves fit on the GPU.
 *
 * The signal that matters is the inverse of the rest of the card: here **full is good**. A
 * layer on the CPU is orders of magnitude slower than on the GPU, and today this only
 * showed up by opening the machine with a double click - the operator saw dragging
 * inference with nothing on screen saying why.
 *
 * That is why the bar turns amber only when a layer is left on the CPU, and neutral when
 * everything is on the GPU: the good case is the majority, and a permanent alarm on the card
 * teaches people to ignore alarms. Without a report from the engine the row does not appear - see
 * `layerPlacement`.
 */
function LayerPlacementMeter({ placement }: { placement: LayerPlacement }) {
  const { t } = useI18n();
  const { served, gpu, cpu } = placement;
  const onCpu = cpu > 0;
  const percent = Math.min(100, (gpu / served) * 100);
  const value = onCpu
    ? t("canvas.layersSplit", { gpu, cpu })
    : t("canvas.layersAllGpu", { count: gpu });
  return <div>
    <div className="mb-1 flex justify-between gap-2 text-[9px] uppercase tracking-[0.12em] text-faint">
      <span>{t("canvas.layers")}</span>
      <span className={clsx("normal-case tracking-normal", onCpu ? "text-amber" : "text-muted")}>{value}</span>
    </div>
    <div
      className="h-1.5 overflow-hidden rounded-full bg-line"
      role="img"
      aria-label={onCpu ? t("canvas.layersOnCpu", { cpu }) : `${t("canvas.layers")}: ${value}`}
    >
      <div className={clsx("h-full rounded-full", onCpu ? "bg-amber" : "bg-aqua")} style={{ width: `${percent}%` }} />
    </div>
  </div>;
}

/**
 * A click that came from a control is not a click on the card.
 *
 * The Flow card has no buttons today, but the double click is the gesture for "open
 * this machine" and must not become a trap for the first button that someone
 * puts here - nor for those that already exist inside the window's card, if the
 * double click lands near it.
 */
export function isControlTarget(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest("button, a, input, select, textarea, [role='button']") != null;
}

function CompactToggle({ compact, onToggle }: { compact: boolean; onToggle: () => void }) {
  const { t } = useI18n();
  const label = t(compact ? "canvas.cardExpand" : "canvas.cardMinimize");
  return <button
    type="button"
    data-testid="machine-compact-toggle"
    aria-label={label}
    aria-pressed={compact}
    title={label}
    onClick={onToggle}
    className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-faint hover:bg-raised hover:text-fg"
  >
    {compact ? <Maximize2 className="h-3.5 w-3.5" aria-hidden="true" /> : <Minimize2 className="h-3.5 w-3.5" aria-hidden="true" />}
  </button>;
}

type NetworkNodeProps = {
  range: GraphRange;
  position: CanvasPosition;
  dragging: boolean;
  perf?: ChatTelemetryPeerPerf;
  speed?: PeerSpeed;
  compact?: boolean;
  /** Plugin services to hang under the expanded card, which has variable height. */
  services?: PluginService[];
  /** Reports the expanded card's real height, which grows with its content. */
  onMeasure?: (height: number) => void;
  onToggleCompact?: () => void;
  onPointerDown: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerMove: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerUp: (event: React.PointerEvent<HTMLElement>) => void;
  onOpen?: () => void;
  onSelect?: () => void;
};

const UNRATED = { decode: { tone: "unknown", ratio: null }, prefill: { tone: "unknown", ratio: null } } as const satisfies PeerSpeed;

function NetworkNode({ range, position, dragging, perf, speed = UNRATED, compact = false, services, onMeasure, onToggleCompact, onPointerDown, onPointerMove, onPointerUp, onOpen, onSelect }: NetworkNodeProps) {
  const article = useRef<HTMLElement>(null);
  /*
   * The expanded card has a minimum height, not a fixed one: a second GPU, the layer
   * placement row or a long name make it taller. The arrangement stacked cards by the
   * minimum, so a taller card ran over the one below it (and over its service badges).
   * `offsetHeight` ignores the zoom transform, which is the unit the layout works in.
   */
  useLayoutEffect(() => {
    const element = article.current;
    if (compact || !element || !onMeasure) return;
    const report = () => onMeasure(Math.ceil(element.offsetHeight));
    report();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(report);
    observer.observe(element);
    return () => observer.disconnect();
  }, [compact, onMeasure]);
  const system = range.node?.system;
  /*
   * A machine may have more than one card, and the miniature card showed only
   * the first: in a box with two RTXs, half of the VRAM vanished from the screen and the
   * second card could be boiling with nobody seeing. The VRAM here is the sum
   * of the cards - it is the total the machine has to offer - and the temperature
   * gets one line per card right below.
   */
  const gpus = system?.gpus ?? [];
  const vram = sumVram(gpus);
  const vramUsed = vram ? vram.used / 1024 : null;
  const vramTotal = vram ? vram.total / 1024 : null;
  const ramUsed = system ? system.ram.used_mb / 1024 : null;
  const ramTotal = system ? system.ram.total_mb / 1024 : null;
  const hostname = range.node?.hostname || range.short_id;
  const serving = isServing(range.node?.status.state);
  const unreachable = Boolean(range.unreachable);
  const rediscovering = Boolean(range.rediscovering);
  const unread = unreachable || rediscovering;
  const kind = machineKind(system);
  // Reuses what `/api/ui/cluster` already brings: no new call.
  const hasGpu = system != null && system.gpu_kind !== "none";
  const placement = layerPlacement(
    range.start_layer,
    range.end_layer,
    range.node?.status.reported_gpu_layers,
    hasGpu,
  );
  const { t } = useI18n();
  const cpuWarning = warnsLayersOnCpu(placement, system?.gpu_kind, range.engine)
    ? <span data-testid="machine-cpu-warning" className="inline-flex"><AlertTooltip compact label={hostname} message={t("canvas.layersOnCpuWarning", { cpu: placement!.cpu, served: placement!.served })} /></span>
    : null;
  const statusTone = rediscovering ? "amber" : unreachable ? "rose" : serverStateTone(range.node?.status.state);
  const statusLabel = rediscovering ? t("pipeline.rediscovering") : unreachable ? "timeout" : serverStateLabel(range.node?.status.state);
  const cardEvents = {
    onPointerDown,
    onPointerMove,
    onPointerUp,
    onPointerCancel: onPointerUp,
    onDoubleClick: (event: React.MouseEvent) => { if (onOpen && !isControlTarget(event.target)) onOpen(); },
    onClick: (event: React.MouseEvent) => { if (onSelect && !isControlTarget(event.target)) onSelect(); },
  };
  if (compact) {
    /*
     * The minimum that answers "is everything fine with this machine?": who it is, whether it
     * serves, which layers, how fast, and how much of the VRAM. The rest stays one
     * click (expand) or a double click (details) away.
     */
    const vramPercent = vramUsed != null && vramTotal != null && vramTotal > 0 ? Math.min(100, (vramUsed / vramTotal) * 100) : null;
    const vramTone = vramPercent != null && vramPercent >= 90 ? "bg-rose" : vramPercent != null && vramPercent >= 70 ? "bg-amber" : "bg-aqua";
    const vramLabel = vramPercent == null ? `VRAM: ${t("canvas.meterUnknown")}` : `VRAM: ${vramUsed!.toFixed(1)} / ${vramTotal!.toFixed(1)} GiB`;
    return <article
      {...cardEvents}
      data-compact="true"
      title={onOpen ? t("canvas.openDetails") : undefined}
      className={clsx("absolute select-none overflow-hidden rounded-lg border bg-ink/95 px-2.5 pb-2 pt-2 shadow-panel backdrop-blur transition-[box-shadow,border-color]", dragging ? "cursor-grabbing border-iris shadow-lift" : "cursor-grab border-line-strong hover:border-iris/55 hover:shadow-lift", unread && "opacity-75")}
      style={{ left: position.left, top: position.top, width: COMPACT_CARD_WIDTH, height: COMPACT_CARD_HEIGHT, touchAction: "none" }}
    >
      <div className="flex items-center gap-2">
        <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md border border-line bg-raised"><MachineIcon system={system} size="sm" /></span>
        <div className="min-w-0 flex-1">
          <h3 className="truncate text-[13px] font-semibold leading-tight text-fg">{hostname}</h3>
          <p className="flex items-center gap-1.5 text-[10px] leading-tight">
            <span data-testid="machine-layers" className={clsx("font-mono font-semibold", placement && placement.cpu > 0 ? "text-amber" : "text-iris")} title={placement && placement.cpu > 0 ? t("canvas.layersOnCpu", { cpu: placement.cpu }) : undefined}>L{range.start_layer} - L{range.end_layer}</span>
            {cpuWarning}
            {range.role && <span data-testid="machine-role" className="rounded border border-aqua/40 px-1 font-semibold uppercase tracking-[0.08em] text-aqua">{t(range.role === "prefill" ? "canvas.rolePrefill" : "canvas.roleShared")}</span>}
          </p>
        </div>
        <span data-testid="machine-status" title={statusLabel} aria-label={statusLabel} className="flex shrink-0 items-center"><Dot tone={statusTone} pulse={rediscovering || (serving && !unread)} /></span>
        {onToggleCompact && <CompactToggle compact onToggle={onToggleCompact} />}
      </div>
      <div className="mt-1.5"><SpeedBadges perf={perf} rating={speed} compact /></div>
      <div className="absolute inset-x-2.5 bottom-1.5 h-1 overflow-hidden rounded-full bg-line" role="img" aria-label={vramLabel} title={vramLabel}>
        {vramPercent != null && <div className={clsx("h-full rounded-full", vramTone)} style={{ width: `${vramPercent}%` }} />}
      </div>
    </article>;
  }
  return <article
    ref={article}
    onPointerDown={onPointerDown}
    onPointerMove={onPointerMove}
    onPointerUp={onPointerUp}
    onPointerCancel={onPointerUp}
    onDoubleClick={(event) => { if (onOpen && !isControlTarget(event.target)) onOpen(); }}
    onClick={(event) => { if (onSelect && !isControlTarget(event.target)) onSelect(); }}
    /* Without a detail to open, the card does not announce a double click that does not happen. */
    title={onOpen ? t("canvas.openDetails") : undefined}
    className={clsx("absolute select-none rounded-xl border bg-ink/95 p-3 shadow-panel backdrop-blur transition-[box-shadow,border-color]", dragging ? "cursor-grabbing border-iris shadow-lift" : "cursor-grab border-line-strong hover:border-iris/55 hover:shadow-lift", unread && "opacity-75")}
    style={{ left: position.left, top: position.top, width: CARD_WIDTH, minHeight: CARD_HEIGHT, touchAction: "none" }}
  >
    <div className="absolute inset-x-3 top-0 h-px bg-iris/70" />
    <div className="flex items-start justify-between gap-3">
      <div className="flex min-w-0 items-center gap-2">
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised"><MachineIcon system={system} size="sm" /></span>
        <div className="min-w-0"><h3 className="truncate text-sm font-semibold text-fg">{hostname}</h3><p className="truncate text-[10px] text-faint">{kind.label}</p></div>
      </div>
      <div className="flex shrink-0 flex-col items-end gap-0.5">
        <span data-testid="machine-status" className="flex items-center gap-1.5 text-[11px] text-muted"><Dot tone={statusTone} pulse={rediscovering || (serving && !unread)} />{statusLabel}</span>
        <span data-testid="machine-layers" className="font-mono text-[9px] font-semibold text-iris">L{range.start_layer} - L{range.end_layer}</span>
        {cpuWarning}
        {range.role && <span data-testid="machine-role" className="text-[9px] font-semibold uppercase tracking-[0.08em] text-aqua">{t(range.role === "prefill" ? "canvas.rolePrefill" : "canvas.roleShared")}</span>}
      </div>
      {onToggleCompact && <div className="-mr-1.5 -mt-1"><CompactToggle compact={false} onToggle={onToggleCompact} /></div>}
    </div>
    <div data-testid="machine-meter-group" className="mt-2 space-y-1.5">
      <Meter label={gpus.length > 1 ? t("canvas.vramTotal", { count: gpus.length }) : "VRAM"} used={vramUsed} total={vramTotal} unknown={vramTotal == null} />
      {placement && <LayerPlacementMeter placement={placement} />}
      <Meter label={t("canvas.memory")} used={ramUsed} total={ramTotal} unknown={!system} />
      <Meter label={t("canvas.load")} used={range.load} total={8} detail={range.load == null ? t("canvas.meterUnknown") : t("canvas.inFlight", { count: range.load })} />
    </div>
    {/* Measured speed of the slice: what decides the route, together with the RTT. */}
    <div className="mt-2"><SpeedBadges perf={perf} rating={speed} /></div>
    {gpus.length > 0 ? (
      gpus.map((g) => (
        <GpuTemperature
          key={g.index}
          gpuKind={system?.gpu_kind ?? "none"}
          temperature={g.temperature_c}
          policy={range.node?.thermal}
          label={gpus.length > 1 ? gpuDisplayName(g, gpus) : undefined}
          compact
        />
      ))
    ) : (
      <GpuTemperature
        gpuKind={system?.gpu_kind ?? "none"}
        temperature={null}
        policy={range.node?.thermal}
        compact
      />
    )}
    {services && <PluginServiceTether services={services} />}
  </article>;
}

export function ModelNetworkCanvas({
  cluster,
  rttMeasurements = [],
  pipelineCost,
  pipelineRoutes = [],
  peerPerf,
  propertiesOpen = false,
  onToggleProperties,
  onSelectNode,
  model: controlledModel,
  onModelChange,
}: {
  cluster: ClusterView;
  rttMeasurements?: ChatTelemetryRtt[];
  pipelineCost?: PipelineCycleCost;
  pipelineRoutes?: PipelineRouteCost[];
  peerPerf?: Record<string, ChatTelemetryPeerPerf>;
  propertiesOpen?: boolean;
  onToggleProperties?: () => void;
  /** A single click on a machine's card selects it (its configuration shows in the properties panel). */
  onSelectNode?: (nodeId: string) => void;
  /**
   * The model picked in the bar, when the caller owns it: the Dashboard does, because
   * its Add and Undeploy act on the model shown here, not on the first one.
   */
  model?: string;
  onModelChange?: (model: string) => void;
}) {
  const { t } = useI18n();
  const graph = useMemo(() => graphFromCluster(cluster), [cluster]);
  const [ownModel, setOwnModel] = useState<string | undefined>();
  const model = onModelChange ? controlledModel : ownModel;
  const setModel = onModelChange ?? setOwnModel;
  const [zoom, setZoom] = useState(1);
  const [positions, setPositions] = useState<Record<string, CanvasPosition>>({});
  const [dragging, setDragging] = useState<string | null>(null);
  const [cardModes, setCardModes] = useState<CardModes>(readCardModes);
  // Measured height of each expanded card, by layout id; see `NetworkNode`.
  const [cardHeights, setCardHeights] = useState<Record<string, number>>({});
  const measurers = useRef(new Map<string, (height: number) => void>());
  const measurerOf = (id: string) => {
    let measure = measurers.current.get(id);
    if (!measure) {
      measure = (height: number) => setCardHeights((current) => (current[id] === height ? current : { ...current, [id]: height }));
      measurers.current.set(id, measure);
    }
    return measure;
  };
  // Keeps the `node_id`, and not the peer: the list is renewed on every refresh of the
  // cluster, and a copy kept here would freeze the numbers of the open window.
  const [detailNodeId, setDetailNodeId] = useState<string | null>(null);
  // The detail card is the same as the Details tab; whoever does not open the tab also
  // does not open it by clicking on the drawing. The rule is one only, in `lib/access.ts`.
  const { profile } = useProfile();
  const detailed = canSeePeerDetails(profile);
  const showClusterCoverage = profile === "admin";
  const canvas = useRef<HTMLDivElement>(null);
  const dragOffset = useRef({ x: 0, y: 0 });
  const modelIds = useMemo(() => canvasModelIds(cluster), [cluster]);
  const selected = selectedCanvasModel(cluster, model, modelIds);
  const ranges = selected ? (graph.byModel[selected] ?? []) : [];
  const inventory = (cluster.model_inventory ?? []).find((entry) => entry.model_id === selected);
  // How many machines of this model the node could not read in this pass.
  const coverageUnconfirmed = inventory?.unreadable_nodes?.length ?? 0;
  const layoutKey = useMemo(() => deploymentLayoutKey(selected ?? "", ranges.map((range) => ({ node_id: range.role ? `${keyFor(range)}:${range.role}` : keyFor(range), start_layer: range.start_layer, end_layer: range.end_layer, changed_at: range.node?.status.changed_at }))), [ranges, selected]);
  const selectedEdges = selected ? cluster.routing_edges.filter((edge) => edge.model_id === selected) : [];
  const candidates = selected ? cluster.candidate_edges.filter((edge) => edge.model_id === selected) : [];
  const selectedEdgeIds = new Set(selectedEdges.map(edgeKey));
  const candidateIds = new Set(candidates.map(edgeKey));
  const rttByPair = useMemo(
    () => new Map(rttMeasurements.map((measurement) => [
      `${measurement.from_node_id}:${measurement.to_node_id}`,
      measurement,
    ])),
    [rttMeasurements],
  );
  const edges = [...candidates, ...selectedEdges.filter((edge) => !candidates.some((candidate) => candidate.from_node_id === edge.from_node_id && candidate.to_node_id === edge.to_node_id && candidate.from_end_layer === edge.from_end_layer && candidate.to_start_layer === edge.to_start_layer))];

  useEffect(() => { setPositions(readPositions(layoutKey)); }, [layoutKey]);

  const compactOf = (id: string) => isCardCompact(cardModes, id);
  const expandedSize = (id: string): CardSize => ({ width: CARD_WIDTH, height: Math.max(CARD_HEIGHT, cardHeights[id] ?? CARD_HEIGHT) });
  const sizeOf = (id: string): CardSize => (compactOf(id) ? COMPACT_CARD_SIZE : expandedSize(id));
  /*
   * The services a card hangs below itself. A machine with several ranges of this
   * model gets them only on its first card. The engine badge follows what runs the
   * model: llama.cpp gets its own badge and an installed Strata that is not in use is left out.
   */
  const servicesByCard = useMemo(() => {
    const byCard: Record<string, PluginService[]> = {};
    ranges.forEach((range, index) => {
      if (ranges.findIndex((other) => other.node_id === range.node_id) !== index) return;
      const services = servicesInUse(range.node?.plugin_services, range.engine, Boolean(range.model));
      if (services.length > 0) byCard[keyFor(range)] = services;
    });
    return byCard;
  }, [ranges]);
  const changeCardModes = (next: CardModes) => { writeCardModes(next); setCardModes(next); };
  const toggleCard = (id: string) => changeCardModes({ ...cardModes, overrides: { ...cardModes.overrides, [id]: !compactOf(id) } });
  // With everything minimized the button expands; with any card open, it minimizes.
  const allCompact = ranges.length > 0 && ranges.every((range) => compactOf(keyFor(range)));
  const setAllCompact = (compact: boolean) => changeCardModes({ compact, overrides: {} });

  /*
   * Decode and prefill compared across the machines of this model: it is the same
   * pipeline, and the slice that holds the token is the one that needs to jump out at the eye.
   */
  const decodes = ranges.map((range) => peerPerf?.[range.node_id]?.decode_tokens_per_sec);
  const prefills = ranges.map((range) => peerPerf?.[range.node_id]?.prefill_tokens_per_sec);
  const speedOf = (range: GraphRange) => ({
    decode: rateSpeed(peerPerf?.[range.node_id]?.decode_tokens_per_sec, decodes),
    prefill: rateSpeed(peerPerf?.[range.node_id]?.prefill_tokens_per_sec, prefills),
  });
  const nameOf = (nodeId: string) => {
    const peer = cluster.peers.find((candidate) => candidate.node_id === nodeId);
    return peer?.node?.hostname || peer?.short_id || nodeId.slice(0, 8);
  };

  /**
   * The arrangement the network has when nobody dragged anything: one column per range
   * of layers, replicas stacked. Recomputed when the topology changes, not on
   * every poll - otherwise the cards would move on their own while the operator looks.
   */
  const arrangement = useMemo(
    () => autoLayout(
      ranges.map((range) => ({ id: keyFor(range), start_layer: range.start_layer, end_layer: range.end_layer, role: range.role })),
      // The stack leaves room for the real card height and for the service badges
      // hanging under it, so a card never lands on the badges of the one above.
      (id) => {
        const size = isCardCompact(cardModes, id)
          ? COMPACT_CARD_SIZE
          : { width: CARD_WIDTH, height: Math.max(CARD_HEIGHT, cardHeights[id] ?? CARD_HEIGHT) };
        return servicesByCard[id] ? { width: size.width, height: size.height + SERVICE_TETHER_HEIGHT } : size;
      },
    ),
    [ranges, cardModes, cardHeights, servicesByCard],
  );

  // The dragged position beats the arrangement, and the arrangement beats the grid. The grid
  // only remains for a node that `autoLayout` does not know, which should not
  // happen - but vanishing from the screen would be worse than appearing in the wrong place.
  const positionOf = (range: GraphRange, index: number) =>
    positions[keyFor(range)] ?? arrangement[keyFor(range)] ?? fallbackPosition(index);
  const updatePosition = (id: string, next: CanvasPosition) => setPositions((current) => {
    const updated = { ...current, [id]: next };
    writePositions(layoutKey, updated);
    return updated;
  });

  const detailPeer = detailNodeId ? cluster.peers.find((peer) => peer.node_id === detailNodeId) ?? null : null;
  const pipeline = pipelinePeers(cluster);
  const terminalEndLayer = ranges.length > 0 ? Math.max(...ranges.map((range) => range.end_layer)) : null;
  // A prefill machine never answers: the answer leaves from the machines that decode.
  const answering = ranges.some((range) => range.role !== "prefill") ? ranges.filter((range) => range.role !== "prefill") : ranges;
  const terminalRanges = terminalEndLayer == null ? [] : answering.filter((range) => range.end_layer === terminalEndLayer);
  const gatewayPosition = terminalRanges.length === 0
    ? null
    : positions[GATEWAY_LAYOUT_ID] ?? pipelineGatewayPosition(
      terminalRanges.map((range) => positionOf(range, ranges.indexOf(range))),
      terminalRanges.map((range) => sizeOf(keyFor(range))),
    );
  const forwardEdges = edges.filter((edge) => {
    const from = rangeForEdge(ranges, edge, "from");
    const to = rangeForEdge(ranges, edge, "to");
    // a prefill machine hands the prompt's state to a decode machine of the same layers
    const handoff = from?.role === "prefill" && to?.role !== "prefill";
    return Boolean(from && to && (to.start_layer > from.start_layer || handoff));
  });

  /**
   * Tidies the network **without throwing away what the operator dragged**.
   *
   * Whoever was moved by hand stays where it is: that is a decision, and the button that
   * erased decisions would be the button that nobody presses. The automatic arrangement
   * comes in for everything else. To discard the decisions there is `reset`.
   */
  const organize = () => {
    // Only what was dragged is recorded: the rest follows the live arrangement, which
    // recomposes when a card is minimized or expanded.
    setPositions((current) => {
      const dragged = Object.fromEntries(Object.entries(current).filter(([id]) => id in arrangement || id === GATEWAY_LAYOUT_ID));
      writePositions(layoutKey, dragged);
      return dragged;
    });
    setZoom(1);
  };

  const reset = () => { clearPositions(layoutKey); setPositions({}); setZoom(1); };
  const fit = () => setZoom(1);

  /*
   * Machines and gateway drag the same way: the id is the key in the position
   * map and the size limits how far the card goes. A touch that started on a
   * button of the card belongs to the button - capturing the pointer here would steal the click.
   */
  function beginDrag(id: string, current: CanvasPosition, event: React.PointerEvent<HTMLElement>) {
    const surface = canvas.current;
    if (!surface || isControlTarget(event.target)) return;
    const bounds = surface.getBoundingClientRect();
    dragOffset.current = {
      x: (event.clientX - bounds.left) / zoom - current.left,
      y: (event.clientY - bounds.top) / zoom - current.top,
    };
    setDragging(id);
    event.currentTarget.setPointerCapture(event.pointerId);
  }

  function drag(id: string, size: CardSize, event: React.PointerEvent<HTMLElement>) {
    if (dragging !== id || !canvas.current) return;
    const bounds = canvas.current.getBoundingClientRect();
    const maxLeft = Math.max(16, bounds.width / zoom - size.width - 16);
    const maxTop = Math.max(58, bounds.height / zoom - size.height - 16);
    const left = Math.max(16, Math.min(maxLeft, (event.clientX - bounds.left) / zoom - dragOffset.current.x));
    const top = Math.max(58, Math.min(maxTop, (event.clientY - bounds.top) / zoom - dragOffset.current.y));
    updatePosition(id, { left, top });
  }

  function endDrag(event: React.PointerEvent<HTMLElement>) {
    if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
    setDragging(null);
  }

  return <section ref={canvas} className="relative h-full min-h-0 w-full overflow-auto bg-[radial-gradient(circle_at_1px_1px,rgba(91,78,255,.16)_1px,transparent_0)]" style={{ backgroundSize: "18px 18px" }}>
    <div className="absolute left-4 top-3 z-20 flex max-w-[calc(100%-2rem)] flex-wrap items-center gap-2 rounded-xl border border-line bg-ink/90 p-1.5 shadow-panel backdrop-blur-md">
      <div className="flex max-w-[min(46rem,60vw)] overflow-x-auto" role="group" aria-label={t("canvas.modelGroupAria")}>{modelIds.length ? modelIds.map((name) => <button key={name} onClick={() => setModel(name)} aria-pressed={selected === name} className={clsx("h-8 shrink-0 rounded-md px-3 font-mono text-xs", selected === name ? "bg-iris text-white" : "text-muted hover:bg-raised hover:text-fg")}>{name}</button>) : <span className="px-2 text-xs text-muted">{t("canvas.noModel")}</span>}</div>
      {/*
        Full coverage with an unreadable machine is not complete (see Dashboard).
        The badge loses the green and gains the `?` glued to the number, which is what
        is being claimed: "32 of 32 - really?". The whole explanation stays in the
        `title`, so the badge does not grow.
      */}
      {showClusterCoverage && inventory && <span title={coverageUnconfirmed > 0 ? t("dashboard.coverageUnconfirmed", { count: coverageUnconfirmed }) : undefined} className={clsx("shrink-0 rounded-md px-2 py-1 font-mono text-[10px]", inventory.complete && coverageUnconfirmed === 0 ? "bg-aqua/15 text-aqua" : "bg-amber/15 text-amber")}>L{inventory.covered_layers}/{inventory.total_layers}{coverageUnconfirmed > 0 ? "?" : ""}</span>}
      <span className="h-5 w-px bg-line" />
      {ranges.length > 0 && <Button
        size="sm"
        data-testid="cards-compact-all"
        aria-pressed={allCompact}
        aria-label={t(allCompact ? "canvas.cardsExpandAll" : "canvas.cardsMinimizeAll")}
        title={t(allCompact ? "canvas.cardsExpandAll" : "canvas.cardsMinimizeAll")}
        onClick={() => setAllCompact(!allCompact)}
      >
        {allCompact ? <Maximize2 className="h-4 w-4" /> : <Minimize2 className="h-4 w-4" />}
      </Button>}
      <Button size="sm" aria-label={t("canvas.zoomOut")} onClick={() => setZoom((value) => Math.max(.65, value - .1))}><Minus className="h-4 w-4" /></Button>
      <Button size="sm" aria-label={t("canvas.zoomIn")} onClick={() => setZoom((value) => Math.min(1.25, value + .1))}><Plus className="h-4 w-4" /></Button>
      <Button size="sm" aria-label={t("canvas.fit")} onClick={fit}>Fit</Button>
      <Button size="sm" aria-label={t("canvas.autoLayout")} title={t("canvas.autoLayout")} onClick={organize}><LayoutGrid className="h-4 w-4" /></Button>
      <Button size="sm" aria-label={t("canvas.resetLayout")} title={t("canvas.resetLayout")} onClick={reset}><RotateCcw className="h-4 w-4" /></Button>
    </div>
    {onToggleProperties && <div className="absolute right-4 top-3 z-20">
      <Button
        size="sm"
        variant="outline"
        className="h-8 w-8 px-0"
        aria-label={t(propertiesOpen ? "dashboard.hideProperties" : "dashboard.versionProperties")}
        aria-pressed={propertiesOpen}
        title={t(propertiesOpen ? "dashboard.hideProperties" : "dashboard.versionProperties")}
        onClick={onToggleProperties}
        icon={propertiesOpen ? <PanelRightClose className="h-4 w-4" /> : <PanelRightOpen className="h-4 w-4" />}
      />
    </div>}
    {ranges.length === 0 ? <div className="flex h-full items-center justify-center text-sm text-muted">{t("canvas.noCapacity")}</div> : <div className="absolute inset-0 origin-top-left" style={{ transform: `scale(${zoom})`, width: `${100 / zoom}%`, height: `${100 / zoom}%` }}>
      <svg aria-hidden="true" className="pointer-events-none absolute inset-0 h-full w-full overflow-visible">
        <defs><marker id="route-arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 Z" className="fill-aqua" /></marker><marker id="route-arrow-relay" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 Z" className="fill-relay" /></marker></defs>
        {forwardEdges.map((edge) => {
          const from = rangeForEdge(ranges, edge, "from"); const to = rangeForEdge(ranges, edge, "to");
          if (!from || !to) return null;
          const fromIndex = ranges.indexOf(from); const toIndex = ranges.indexOf(to);
          const a = positionOf(from, fromIndex); const b = positionOf(to, toIndex);
          const geometry = routeGeometry(a, b, sizeOf(keyFor(from)), sizeOf(keyFor(to)));
          const id = edgeKey(edge);
          // An edge that is in a `candidate_edges` is a ready path, not
          // hypothetical traffic: with replicas, it is where the network would serve if the
          // primary left.
          const state: EdgeState = selectedEdgeIds.has(id) ? "principal" : candidateIds.has(id) ? "paralela" : "inativa";
          // Edge through a relay: strong orange, whatever the state - it is
          // the cause of the slowness and calls for a route review.
          const style = rttByPair.get(`${edge.from_node_id}:${edge.to_node_id}`)?.relayed ? RELAY_EDGE_STYLE : edgeStyle(state);
          return <path key={id} data-relayed={style === RELAY_EDGE_STYLE ? "true" : undefined} d={`M ${geometry.x1} ${geometry.y1} C ${geometry.c1x} ${geometry.c1y}, ${geometry.c2x} ${geometry.c2y}, ${geometry.x2} ${geometry.y2}`} className={style.className} strokeWidth={style.strokeWidth} strokeDasharray={style.strokeDasharray} markerEnd={style.markerEnd} />;
        })}
        {gatewayPosition && terminalRanges.map((range) => {
          const geometry = gatewayRouteGeometry(positionOf(range, ranges.indexOf(range)), gatewayPosition, sizeOf(keyFor(range)));
          const relayed = rttByPair.get(`${range.node_id}:gateway`)?.relayed === true;
          return <path key={`gateway-${range.node_id}`} data-relayed={relayed ? "true" : undefined} d={`M ${geometry.x1} ${geometry.y1} C ${geometry.c1x} ${geometry.c1y}, ${geometry.c2x} ${geometry.c2y}, ${geometry.x2} ${geometry.y2}`} className={relayed ? RELAY_EDGE_STYLE.className : "fill-none stroke-aqua"} strokeWidth={relayed ? RELAY_EDGE_STYLE.strokeWidth : "3"} markerEnd={relayed ? RELAY_EDGE_STYLE.markerEnd : "url(#route-arrow)"} />;
        })}
      </svg>
      {forwardEdges.map((edge, index) => {
        const from = rangeForEdge(ranges, edge, "from"); const to = rangeForEdge(ranges, edge, "to");
        if (!from || !to) return null;
        const geometry = routeGeometry(positionOf(from, ranges.indexOf(from)), positionOf(to, ranges.indexOf(to)), sizeOf(keyFor(from)), sizeOf(keyFor(to)));
        const measurement = rttByPair.get(`${edge.from_node_id}:${edge.to_node_id}`);
        return <RouteRttLabel key={`rtt-${edgeKey(edge)}-${index}`} rttMs={measurement?.rtt_ms} transport={measurement?.transport} relayed={measurement?.relayed === true} left={geometry.labelX} top={geometry.labelY} />;
      })}
      {gatewayPosition && terminalRanges.map((range) => {
        const geometry = gatewayRouteGeometry(positionOf(range, ranges.indexOf(range)), gatewayPosition, sizeOf(keyFor(range)));
        const measurement = rttByPair.get(`${range.node_id}:gateway`);
        return <RouteRttLabel key={`rtt-gateway-${range.node_id}`} rttMs={measurement?.rtt_ms} transport={measurement?.transport} relayed={measurement?.relayed === true} left={geometry.labelX} top={geometry.labelY} />;
      })}
      {ranges.map((range, index) => {
        const id = keyFor(range);
        return <NetworkNode key={id} range={range} position={positionOf(range, index)} dragging={dragging === id} perf={peerPerf?.[range.node_id]} speed={speedOf(range)} compact={compactOf(id)} services={servicesByCard[id]} onMeasure={measurerOf(id)} onToggleCompact={() => toggleCard(id)} onPointerDown={(event) => beginDrag(id, positionOf(range, index), event)} onPointerMove={(event) => drag(id, sizeOf(id), event)} onPointerUp={endDrag} onOpen={detailed ? () => setDetailNodeId(range.node_id) : undefined} onSelect={onSelectNode ? () => onSelectNode(range.node_id) : undefined} />;
      })}
      {/* What each machine's plugins serve to the network (Qdrant as a RAG
          database, for example), attached to its card. A machine with several
          ranges gets the badges only on the first. */}
      {ranges.map((range, index) => {
        const id = keyFor(range);
        const services = servicesByCard[id];
        if (!services) return null;
        // The expanded card grows with its content and hangs its own badges;
        // the compact one has a fixed height and clips what comes out of it, so they stay here.
        if (!compactOf(id)) return null;
        return <PluginServiceTether key={`services-${id}`} services={services} position={positionOf(range, index)} size={sizeOf(id)} />;
      })}
      {gatewayPosition && <PipelineGatewayNode position={gatewayPosition} cost={pipelineCost} routes={pipelineRoutes} nameOf={nameOf} dragging={dragging === GATEWAY_LAYOUT_ID} onPointerDown={(event) => beginDrag(GATEWAY_LAYOUT_ID, gatewayPosition, event)} onPointerMove={(event) => drag(GATEWAY_LAYOUT_ID, GATEWAY_SIZE, event)} onPointerUp={endDrag} />}
    </div>}
    {/* The same view the Details tab shows, in the same component. */}
    <PeerDetailDialog
      peer={detailed ? detailPeer : null}
      group={cluster.active_model}
      otherRanges={pipeline.filter((peer) => peer.node_id !== detailNodeId && peer.total_layers > 0).map((peer) => [peer.start_layer, peer.end_layer] as [number, number])}
      onClose={() => setDetailNodeId(null)}
      perf={detailNodeId ? peerPerf?.[detailNodeId] : undefined}
      speed={detailNodeId ? {
        decode: rateSpeed(peerPerf?.[detailNodeId]?.decode_tokens_per_sec, decodes),
        prefill: rateSpeed(peerPerf?.[detailNodeId]?.prefill_tokens_per_sec, prefills),
      } : undefined}
    />
    {ranges.length > 0 && <div className="absolute bottom-3 left-4 z-20 flex items-center gap-3 rounded-lg border border-line bg-ink/90 px-3 py-2 text-[10px] text-muted shadow-panel backdrop-blur"><span className="flex items-center gap-1.5"><i className="h-0.5 w-5 bg-aqua" />{t("canvas.legendPrimary")}</span><span className="flex items-center gap-1.5"><i className="h-0.5 w-5 bg-iris" />{t("canvas.legendParallel")}</span><span className="flex items-center gap-1.5"><i className="h-0.5 w-5 border-t-2 border-dashed border-faint" />{t("canvas.legendInactive")}</span></div>}
  </section>;
}
