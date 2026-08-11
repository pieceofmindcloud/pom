import { Minus, Plus, RotateCcw } from "lucide-react";
import { useMemo, useState } from "react";
import { graphFromCluster, type GraphRange } from "../lib/networkGraph";
import type { ClusterView, RoutingEdge } from "../lib/types";
import { Button, Panel, PanelHeader } from "./ui";

type Position = { left: number; top: number };

const CARD_WIDTH = 220;
const CARD_HEIGHT = 206;
const CANVAS_WIDTH = 1120;
const CANVAS_HEIGHT = 660;

function positionsFor(ranges: GraphRange[]): Map<string, Position> {
  const starts = [...new Set(ranges.map((range) => range.start_layer))].sort((a, b) => a - b);
  const rows = new Map<number, number>();
  const positions = new Map<string, Position>();

  for (const range of ranges) {
    const column = starts.indexOf(range.start_layer);
    const row = rows.get(range.start_layer) ?? 0;
    rows.set(range.start_layer, row + 1);
    positions.set(keyFor(range), { left: 48 + column * 290, top: 52 + row * 236 });
  }
  return positions;
}

function keyFor(range: Pick<GraphRange, "node_id" | "model_id" | "start_layer" | "end_layer">) {
  return `${range.node_id}:${range.model_id}:${range.start_layer}:${range.end_layer}`;
}

function rangeForEdge(ranges: GraphRange[], edge: RoutingEdge, side: "from" | "to") {
  const id = side === "from" ? edge.from_node_id : edge.to_node_id;
  const boundary = side === "from" ? edge.from_end_layer : edge.to_start_layer;
  return ranges.find((range) =>
    range.node_id === id && (side === "from" ? range.end_layer === boundary : range.start_layer === boundary),
  ) ?? ranges.find((range) => range.node_id === id);
}

function Meter({ label, used, total, unknown = false }: { label: string; used: number | null; total: number | null; unknown?: boolean }) {
  const percent = used != null && total != null && total > 0 ? Math.min(100, (used / total) * 100) : null;
  const tone = percent != null && percent >= 90 ? "bg-rose" : percent != null && percent >= 70 ? "bg-amber" : "bg-aqua";
  const text = unknown || percent == null ? "desconhecido" : `${used!.toFixed(1)} / ${total!.toFixed(1)} GiB`;
  return <div className="mt-2">
    <div className="mb-1 flex justify-between gap-2 text-[10px] uppercase tracking-wide text-muted"><span>{label}</span><span className="normal-case tracking-normal">{text}</span></div>
    <div className={`h-1.5 overflow-hidden rounded-full bg-line ${unknown || percent == null ? "bg-[repeating-linear-gradient(135deg,transparent_0,transparent_4px,rgba(124,135,165,.28)_4px,rgba(124,135,165,.28)_6px)]" : ""}`} aria-label={`${label}: ${text}`}>
      {percent != null && <div className={`h-full rounded-full ${tone}`} style={{ width: `${percent}%` }} />}
    </div>
  </div>;
}

function NodeCard({ range, position }: { range: GraphRange; position: Position }) {
  const system = range.node?.system;
  const gpu = system?.gpus[0];
  const vramUsed = gpu?.vram_used_mb != null ? gpu.vram_used_mb / 1024 : null;
  const vramTotal = gpu?.vram_total_mb != null ? gpu.vram_total_mb / 1024 : null;
  const ramUsed = system ? system.ram.used_mb / 1024 : null;
  const ramTotal = system ? system.ram.total_mb / 1024 : null;
  return <article className="absolute w-[220px] rounded-xl border border-line bg-surface p-3 shadow-panel" style={{ left: position.left, top: position.top }}>
    <div className="flex items-center justify-between gap-2"><strong className="font-mono text-sm text-iris">{range.short_id}</strong><span className="text-xs text-aqua">{range.unreachable ? "indisponível" : "pronto"}</span></div>
    <div className="mt-2 rounded bg-iris/10 px-2 py-1 font-mono text-xs text-ink">{range.model_id} · L{range.start_layer}-L{range.end_layer}</div>
    <Meter label="VRAM" used={vramUsed} total={vramTotal} unknown={!gpu || vramTotal == null} />
    <Meter label="Memória" used={ramUsed} total={ramTotal} unknown={!system} />
    <Meter label="Carga" used={range.load} total={8} />
  </article>;
}

export function ModelNetworkCanvas({ cluster }: { cluster: ClusterView }) {
  const graph = useMemo(() => graphFromCluster(cluster), [cluster]);
  const [model, setModel] = useState<string | undefined>();
  const [zoom, setZoom] = useState(1);
  const selected = model && graph.byModel[model] ? model : graph.models[0];
  const ranges = selected ? graph.byModel[selected] : [];
  const positions = useMemo(() => positionsFor(ranges), [ranges]);
  const edges = selected ? cluster.routing_edges.filter((edge) => edge.model_id === selected) : [];

  return <Panel className="overflow-hidden">
    <PanelHeader eyebrow="INVENTÁRIO REAL" title="Rede por modelo" subtitle="Capacidades anunciadas e handoffs escolhidos pelo roteador. Esta tela observa, não controla o balanceamento." />
    <div className="flex flex-wrap items-center justify-between gap-3 border-y border-line bg-raised/50 px-5 py-3">
      <div className="flex flex-wrap gap-1" role="group" aria-label="Modelo">{graph.models.map((name) => <button key={name} onClick={() => setModel(name)} aria-pressed={selected === name} className={`h-8 rounded-md border px-3 font-mono text-xs ${selected === name ? "border-iris/50 bg-iris/12 text-iris" : "border-line text-muted"}`}>{name}</button>)}</div>
      <div className="flex gap-1"><Button size="sm" aria-label="Diminuir zoom" onClick={() => setZoom((value) => Math.max(.7, value - .15))}><Minus className="h-4 w-4" /></Button><Button size="sm" aria-label="Aumentar zoom" onClick={() => setZoom((value) => Math.min(1.3, value + .15))}><Plus className="h-4 w-4" /></Button><Button size="sm" aria-label="Ajustar à rede" onClick={() => setZoom(1)}><RotateCcw className="h-4 w-4" /></Button></div>
    </div>
    {ranges.length === 0 ? <p className="p-6 text-sm text-muted">Nenhuma capacidade de inferência anunciada ainda.</p> : <>
      <div className="overflow-auto bg-[radial-gradient(circle_at_1px_1px,rgba(91,78,255,.18)_1px,transparent_0)] p-6" style={{ backgroundSize: "18px 18px" }}>
        <div className="relative origin-top-left" style={{ width: CANVAS_WIDTH, height: CANVAS_HEIGHT, transform: `scale(${zoom})` }}>
          <svg aria-hidden="true" className="pointer-events-none absolute inset-0" width={CANVAS_WIDTH} height={CANVAS_HEIGHT} viewBox={`0 0 ${CANVAS_WIDTH} ${CANVAS_HEIGHT}`}>
            <defs><marker id="route-arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 Z" className="fill-iris" /></marker></defs>
            {edges.map((edge) => {
              const from = rangeForEdge(ranges, edge, "from");
              const to = rangeForEdge(ranges, edge, "to");
              if (!from || !to) return null;
              const fromPosition = positions.get(keyFor(from));
              const toPosition = positions.get(keyFor(to));
              if (!fromPosition || !toPosition) return null;
              const x1 = fromPosition.left + CARD_WIDTH;
              const y1 = fromPosition.top + CARD_HEIGHT / 2;
              const x2 = toPosition.left;
              const y2 = toPosition.top + CARD_HEIGHT / 2;
              const bend = Math.max(56, Math.abs(x2 - x1) * .45);
              return <path key={`${edge.from_node_id}:${edge.to_node_id}:${edge.from_end_layer}`} d={`M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2} ${y2}`} className="fill-none stroke-iris" strokeWidth="3" markerEnd="url(#route-arrow)" />;
            })}
          </svg>
          {ranges.map((range) => <NodeCard key={keyFor(range)} range={range} position={positions.get(keyFor(range))!} />)}
        </div>
      </div>
      <div className="border-t border-line px-5 py-3 text-xs text-muted" aria-live="polite">
        {edges.length > 0 ? <span>Arestas selecionadas: {edges.map((edge) => `${edge.from_node_id.slice(0, 8)} L${edge.from_end_layer} → ${edge.to_node_id.slice(0, 8)} L${edge.to_start_layer}`).join(" · ")}</span> : <span>Nenhuma aresta ativa para este modelo. O roteador ainda não encontrou uma rota completa entre os peers.</span>}
      </div>
    </>}
  </Panel>;
}
