import clsx from "clsx";
import { LayoutGrid, Maximize2, Minimize2, Minus, PanelRightClose, PanelRightOpen, Plus, RotateCcw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { gpuDisplayName, machineKind, MachineIcon } from "./MachineIcon";
import { PeerDetailDialog } from "./PeerDetailCard";
import { GpuTemperature } from "./GpuTemperature";
import { Button, Dot } from "./ui";
import { canSeePeerDetails } from "../lib/access";
import { graphFromCluster, type GraphRange } from "../lib/networkGraph";
import { pipelinePeers } from "../lib/networkPeers";
import { useProfile } from "../lib/profile";
import { useI18n } from "../i18n/provider";
import {
  autoLayout,
  CARD_HEIGHT,
  CARD_SIZE,
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
import { layerPlacement, type LayerPlacement } from "../lib/layerPlacement";
import type { ChatTelemetryPeerPerf, ChatTelemetryRtt, ClusterView, RoutingEdge } from "../lib/types";
import { RouteRttLabel } from "./RouteRttLabel";
import { PipelineGatewayNode, type PipelineCycleCost, type PipelineRouteCost } from "./PipelineGatewayNode";

export type EdgeState = "principal" | "paralela" | "inativa";

/** Três estados, porque com réplicas "escolhida ou não" deixou de bastar:
 *  uma aresta pode não estar em uso e ainda assim ser um caminho pronto. */
/** Aresta que passa por um circuito num relé libp2p. */
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
 * Quanto do que esta máquina serve coube na GPU.
 *
 * O sinal que interessa é o inverso do resto do card: aqui **cheio é bom**. Uma
 * camada na CPU é ordens de grandeza mais lenta que na GPU, e hoje isso só
 * aparecia abrindo a máquina no duplo clique - o operador via a inferência
 * arrastada sem nada na tela dizendo por quê.
 *
 * Por isso a barra fica âmbar só quando sobra camada na CPU, e neutra quando
 * está tudo na GPU: o caso bom é a maioria, e um alarme permanente no card
 * ensina a ignorar alarme. Sem relato do motor a linha não aparece - ver
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
 * Um clique que veio de um controle não é um clique no card.
 *
 * O card do Fluxo não tem botões hoje, mas o duplo clique é o gesto de "abrir
 * esta máquina" e não pode virar armadilha para o primeiro botão que alguém
 * colocar aqui — nem para os que já existem dentro do card da janela, se o
 * duplo clique cair perto dela.
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
  onToggleCompact?: () => void;
  onPointerDown: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerMove: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerUp: (event: React.PointerEvent<HTMLElement>) => void;
  onOpen?: () => void;
};

const UNRATED = { decode: { tone: "unknown", ratio: null }, prefill: { tone: "unknown", ratio: null } } as const satisfies PeerSpeed;

function NetworkNode({ range, position, dragging, perf, speed = UNRATED, compact = false, onToggleCompact, onPointerDown, onPointerMove, onPointerUp, onOpen }: NetworkNodeProps) {
  const system = range.node?.system;
  /*
   * Uma máquina pode ter mais de uma placa, e o card em miniatura mostrava só
   * a primeira: numa caixa com duas RTX, metade da VRAM sumia da tela e a
   * segunda placa podia estar fervendo sem ninguém ver. A VRAM aqui é a soma
   * das placas - é o total que a máquina tem a oferecer - e a temperatura
   * ganha uma linha por placa logo abaixo.
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
  // Reaproveita o que o `/api/ui/cluster` já traz: nenhuma chamada nova.
  const hasGpu = system != null && system.gpu_kind !== "none";
  const placement = layerPlacement(
    range.start_layer,
    range.end_layer,
    range.node?.status.reported_gpu_layers,
    hasGpu,
  );
  const { t } = useI18n();
  const statusTone = rediscovering ? "amber" : unreachable ? "rose" : serverStateTone(range.node?.status.state);
  const statusLabel = rediscovering ? t("pipeline.rediscovering") : unreachable ? "timeout" : serverStateLabel(range.node?.status.state);
  const cardEvents = {
    onPointerDown,
    onPointerMove,
    onPointerUp,
    onPointerCancel: onPointerUp,
    onDoubleClick: (event: React.MouseEvent) => { if (onOpen && !isControlTarget(event.target)) onOpen(); },
  };
  if (compact) {
    /*
     * O mínimo que responde "está tudo bem com esta máquina?": quem é, se
     * serve, que camadas, quão rápida, e quanto da VRAM. O resto continua a um
     * clique (expandir) ou a um duplo clique (detalhes).
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
    onPointerDown={onPointerDown}
    onPointerMove={onPointerMove}
    onPointerUp={onPointerUp}
    onPointerCancel={onPointerUp}
    onDoubleClick={(event) => { if (onOpen && !isControlTarget(event.target)) onOpen(); }}
    /* Sem detalhe a abrir, o card não anuncia um duplo clique que não acontece. */
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
      </div>
      {onToggleCompact && <div className="-mr-1.5 -mt-1"><CompactToggle compact={false} onToggle={onToggleCompact} /></div>}
    </div>
    <div data-testid="machine-meter-group" className="mt-2 space-y-1.5">
      <Meter label={gpus.length > 1 ? t("canvas.vramTotal", { count: gpus.length }) : "VRAM"} used={vramUsed} total={vramTotal} unknown={vramTotal == null} />
      {placement && <LayerPlacementMeter placement={placement} />}
      <Meter label={t("canvas.memory")} used={ramUsed} total={ramTotal} unknown={!system} />
      <Meter label={t("canvas.load")} used={range.load} total={8} detail={range.load == null ? t("canvas.meterUnknown") : t("canvas.inFlight", { count: range.load })} />
    </div>
    {/* Velocidade medida da fatia: o que decide a rota, junto com o RTT. */}
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
}: {
  cluster: ClusterView;
  rttMeasurements?: ChatTelemetryRtt[];
  pipelineCost?: PipelineCycleCost;
  pipelineRoutes?: PipelineRouteCost[];
  peerPerf?: Record<string, ChatTelemetryPeerPerf>;
  propertiesOpen?: boolean;
  onToggleProperties?: () => void;
}) {
  const { t } = useI18n();
  const graph = useMemo(() => graphFromCluster(cluster), [cluster]);
  const [model, setModel] = useState<string | undefined>();
  const [zoom, setZoom] = useState(1);
  const [positions, setPositions] = useState<Record<string, CanvasPosition>>({});
  const [dragging, setDragging] = useState<string | null>(null);
  const [cardModes, setCardModes] = useState<CardModes>(readCardModes);
  // Guarda o `node_id`, e não o peer: a lista se renova a cada refresh do
  // cluster, e uma cópia guardada aqui congelaria os números da janela aberta.
  const [detailNodeId, setDetailNodeId] = useState<string | null>(null);
  // O card de detalhe é o mesmo da aba Detalhes; quem não abre a aba também
  // não o abre pelo clique no desenho. A regra é uma só, em `lib/access.ts`.
  const { profile } = useProfile();
  const detailed = canSeePeerDetails(profile);
  const showClusterCoverage = profile === "admin";
  const canvas = useRef<HTMLDivElement>(null);
  const dragOffset = useRef({ x: 0, y: 0 });
  const modelIds = useMemo(() => Array.from(new Set([
    ...(cluster.model_inventory ?? []).map((inventory) => inventory.model_id),
    ...graph.models,
    ...(cluster.active_model ? [cluster.active_model] : []),
  ])).sort(), [cluster.active_model, cluster.model_inventory, graph.models]);
  const selected = model && modelIds.includes(model) ? model : cluster.active_model || modelIds[0];
  const ranges = selected ? (graph.byModel[selected] ?? []) : [];
  const inventory = (cluster.model_inventory ?? []).find((entry) => entry.model_id === selected);
  // Quantas máquinas deste modelo o nó não conseguiu ler nesta passagem.
  const coverageUnconfirmed = inventory?.unreadable_nodes?.length ?? 0;
  const layoutKey = useMemo(() => deploymentLayoutKey(selected ?? "", ranges.map((range) => ({ node_id: keyFor(range), start_layer: range.start_layer, end_layer: range.end_layer, changed_at: range.node?.status.changed_at }))), [ranges, selected]);
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
  const sizeOf = (id: string): CardSize => (compactOf(id) ? COMPACT_CARD_SIZE : CARD_SIZE);
  const changeCardModes = (next: CardModes) => { writeCardModes(next); setCardModes(next); };
  const toggleCard = (id: string) => changeCardModes({ ...cardModes, overrides: { ...cardModes.overrides, [id]: !compactOf(id) } });
  // Com tudo minimizado o botão expande; com qualquer card aberto, minimiza.
  const allCompact = ranges.length > 0 && ranges.every((range) => compactOf(keyFor(range)));
  const setAllCompact = (compact: boolean) => changeCardModes({ compact, overrides: {} });

  /*
   * Decode e prefill comparados entre as máquinas deste modelo: é a mesma
   * pipeline, e a fatia que segura o token é a que precisa saltar aos olhos.
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
   * O arranjo que a rede tem quando ninguém arrastou nada: uma coluna por faixa
   * de camadas, réplicas empilhadas. Recalculado quando a topologia muda, não a
   * cada poll - senão os cards andariam sozinhos enquanto o operador olha.
   */
  const arranjo = useMemo(
    () => autoLayout(
      ranges.map((range) => ({ id: keyFor(range), start_layer: range.start_layer, end_layer: range.end_layer })),
      (id) => (isCardCompact(cardModes, id) ? COMPACT_CARD_SIZE : CARD_SIZE),
    ),
    [ranges, cardModes],
  );

  // A posição arrastada vence o arranjo, e o arranjo vence a grade. A grade só
  // sobra para um nó que o `autoLayout` não conheça, o que não deveria
  // acontecer - mas sumir da tela seria pior que aparecer no lugar errado.
  const positionOf = (range: GraphRange, index: number) =>
    positions[keyFor(range)] ?? arranjo[keyFor(range)] ?? fallbackPosition(index);
  const updatePosition = (id: string, next: CanvasPosition) => setPositions((current) => {
    const updated = { ...current, [id]: next };
    writePositions(layoutKey, updated);
    return updated;
  });

  const detailPeer = detailNodeId ? cluster.peers.find((peer) => peer.node_id === detailNodeId) ?? null : null;
  const pipeline = pipelinePeers(cluster);
  const terminalEndLayer = ranges.length > 0 ? Math.max(...ranges.map((range) => range.end_layer)) : null;
  const terminalRanges = terminalEndLayer == null ? [] : ranges.filter((range) => range.end_layer === terminalEndLayer);
  const gatewayPosition = terminalRanges.length === 0
    ? null
    : positions[GATEWAY_LAYOUT_ID] ?? pipelineGatewayPosition(
      terminalRanges.map((range) => positionOf(range, ranges.indexOf(range))),
      terminalRanges.map((range) => sizeOf(keyFor(range))),
    );
  const forwardEdges = edges.filter((edge) => {
    const from = rangeForEdge(ranges, edge, "from");
    const to = rangeForEdge(ranges, edge, "to");
    return Boolean(from && to && to.start_layer > from.start_layer);
  });

  /**
   * Arruma a rede **sem jogar fora o que o operador arrastou**.
   *
   * Quem foi movido à mão fica onde está: aquilo é uma decisão, e o botão que
   * apagasse decisões seria o botão que ninguém aperta. O arranjo automático
   * entra para todo o resto. Para descartar as decisões existe `redefinir`.
   */
  const organizar = () => {
    // Só o que foi arrastado é gravado: o resto segue o arranjo vivo, que
    // se recompõe quando um card é minimizado ou expandido.
    setPositions((current) => {
      const arrastados = Object.fromEntries(Object.entries(current).filter(([id]) => id in arranjo || id === GATEWAY_LAYOUT_ID));
      writePositions(layoutKey, arrastados);
      return arrastados;
    });
    setZoom(1);
  };

  const redefinir = () => { clearPositions(layoutKey); setPositions({}); setZoom(1); };
  const fit = () => setZoom(1);

  /*
   * Máquinas e gateway arrastam do mesmo jeito: o id é a chave no mapa de
   * posições e o tamanho limita até onde o card vai. Um toque que começou num
   * botão do card é do botão - capturar o ponteiro aqui roubaria o clique.
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
      <div className="flex max-w-[min(46rem,60vw)] overflow-x-auto" role="group" aria-label="Modelo">{modelIds.length ? modelIds.map((name) => <button key={name} onClick={() => setModel(name)} aria-pressed={selected === name} className={clsx("h-8 shrink-0 rounded-md px-3 font-mono text-xs", selected === name ? "bg-iris text-white" : "text-muted hover:bg-raised hover:text-fg")}>{name}</button>) : <span className="px-2 text-xs text-muted">{t("canvas.noModel")}</span>}</div>
      {/*
        Cobertura completa com máquina ilegível não é completa (ver Dashboard).
        O distintivo perde o verde e ganha o `?` colado ao número, que é o que
        se está a afirmar: "32 de 32 - será?". A explicação inteira fica no
        `title`, para o distintivo não crescer.
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
      <Button size="sm" aria-label={t("canvas.autoLayout")} title={t("canvas.autoLayout")} onClick={organizar}><LayoutGrid className="h-4 w-4" /></Button>
      <Button size="sm" aria-label={t("canvas.resetLayout")} title={t("canvas.resetLayout")} onClick={redefinir}><RotateCcw className="h-4 w-4" /></Button>
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
          // Uma aresta que está num `candidate_edges` é caminho pronto, não
          // tráfego hipotético: com réplicas, é por onde a rede serviria se a
          // principal saísse.
          const state: EdgeState = selectedEdgeIds.has(id) ? "principal" : candidateIds.has(id) ? "paralela" : "inativa";
          // Aresta por relé: laranja forte, qualquer que seja o estado - é
          // o motivo da lentidão e pede revisão da rota.
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
        return <NetworkNode key={id} range={range} position={positionOf(range, index)} dragging={dragging === id} perf={peerPerf?.[range.node_id]} speed={speedOf(range)} compact={compactOf(id)} onToggleCompact={() => toggleCard(id)} onPointerDown={(event) => beginDrag(id, positionOf(range, index), event)} onPointerMove={(event) => drag(id, sizeOf(id), event)} onPointerUp={endDrag} onOpen={detailed ? () => setDetailNodeId(range.node_id) : undefined} />;
      })}
      {gatewayPosition && <PipelineGatewayNode position={gatewayPosition} cost={pipelineCost} routes={pipelineRoutes} nameOf={nameOf} dragging={dragging === GATEWAY_LAYOUT_ID} onPointerDown={(event) => beginDrag(GATEWAY_LAYOUT_ID, gatewayPosition, event)} onPointerMove={(event) => drag(GATEWAY_LAYOUT_ID, GATEWAY_SIZE, event)} onPointerUp={endDrag} />}
    </div>}
    {/* A mesma visão que a aba Detalhes mostra, no mesmo componente. */}
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
