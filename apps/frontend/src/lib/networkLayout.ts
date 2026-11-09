import { storageKey } from "./brand";

export type CanvasPosition = { left: number; top: number };

export interface DeploymentPeer {
  node_id: string;
  start_layer: number;
  end_layer: number;
  changed_at?: string;
}

const STORAGE_PREFIX = storageKey("network-layout");

/**
 * Identifies the deployment this browser is drawing, so a hand-made arrangement
 * does not leak into another network.
 *
 * The model and the set of ranges go into the fingerprint, because changing
 * either draws another network. **`changed_at` does not**, and that is the fix
 * for a real defect: it changes on every node restart, which changed the key,
 * which made `readPositions` return empty - the operator's arrangement was not
 * reorganized, it was lost, and the screen went back to the grid every time a
 * machine restarted. The field stays in the type because the caller already has
 * it at hand and may need it again one day.
 */
export function deploymentLayoutKey(model: string, peers: DeploymentPeer[]) {
  const fingerprint = peers
    .map((peer) => `${peer.node_id}:${peer.start_layer}-${peer.end_layer}`)
    .sort()
    .join("|");
  return `${STORAGE_PREFIX}:${model}:${fingerprint}`;
}

/** Card measurements in `ModelNetworkCanvas`. Changed there, change here. */
export const CARD_WIDTH = 264;
export const CARD_HEIGHT = 204;
/**
 * The minimized card: name, state, range, speed and one VRAM line.
 * Fixed height, not minimum, because the edge attaches to its middle - a card
 * that grew with its content would pull the arrow off the border.
 */
export const COMPACT_CARD_WIDTH = 228;
export const COMPACT_CARD_HEIGHT = 84;
export const GATEWAY_WIDTH = 224;
export const GATEWAY_HEIGHT = 140;

export type CardSize = { width: number; height: number };
export const CARD_SIZE: CardSize = { width: CARD_WIDTH, height: CARD_HEIGHT };
export const COMPACT_CARD_SIZE: CardSize = { width: COMPACT_CARD_WIDTH, height: COMPACT_CARD_HEIGHT };
export const GATEWAY_SIZE: CardSize = { width: GATEWAY_WIDTH, height: GATEWAY_HEIGHT };

/**
 * The gateway's key in the same position map as the machines. Dragged, it
 * stays where it was put; without dragging, it follows the last stage as before.
 */
export const GATEWAY_LAYOUT_ID = "__gateway__";
const GAP_X = 148;
const GAP_Y = 40;
const DEPLOY_GAP_X = 220;
const DEPLOY_GAP_Y = 72;
const ORIGIN_X = 48;
/** Horizontal axis around which each column is centered. */
const AXIS_Y = 300;

export type RouteGeometry = {
  x1: number;
  y1: number;
  c1x: number;
  c1y: number;
  c2x: number;
  c2y: number;
  x2: number;
  y2: number;
  labelX: number;
  labelY: number;
};

/** Curve endpoints face each other, including for a manually reversed layout. */
export function routeGeometry(
  from: CanvasPosition,
  to: CanvasPosition,
  fromSize: CardSize = CARD_SIZE,
  toSize: CardSize = CARD_SIZE,
): RouteGeometry {
  const leftToRight = to.left >= from.left;
  const x1 = leftToRight ? from.left + fromSize.width : from.left;
  const y1 = from.top + fromSize.height / 2;
  const x2 = leftToRight ? to.left : to.left + toSize.width;
  const y2 = to.top + toSize.height / 2;
  const direction = x2 >= x1 ? 1 : -1;
  const bend = Math.max(60, Math.abs(x2 - x1) * 0.42);
  const c1x = x1 + direction * bend;
  const c2x = x2 - direction * bend;
  const labelX = (x1 + 3 * c1x + 3 * c2x + x2) / 8;
  const labelY = (y1 + 3 * y1 + 3 * y2 + y2) / 8;
  return { x1, y1, c1x, c1y: y1, c2x, c2y: y2, x2, y2, labelX, labelY };
}

/** Puts the virtual gateway after the last stage, aligned with its center. */
export function pipelineGatewayPosition(
  peers: CanvasPosition[],
  sizes: CardSize[] = [],
): CanvasPosition | null {
  if (peers.length === 0) return null;
  const sizeAt = (index: number) => sizes[index] ?? CARD_SIZE;
  const right = Math.max(...peers.map((peer, index) => peer.left + sizeAt(index).width));
  const center = peers.reduce((sum, peer, index) => sum + peer.top + sizeAt(index).height / 2, 0) / peers.length;
  return { left: right + GAP_X, top: center - GATEWAY_HEIGHT / 2 };
}

/**
 * Geometry of the cycle's final leg. To the right when the gateway is after the
 * last stage, which is the usual arrangement; dragged backwards, the curve
 * leaves and arrives through the sides that face each other, like `routeGeometry`.
 */
export function gatewayRouteGeometry(
  from: CanvasPosition,
  gateway: CanvasPosition,
  fromSize: CardSize = CARD_SIZE,
): RouteGeometry {
  const leftToRight = gateway.left >= from.left + fromSize.width / 2;
  const x1 = leftToRight ? from.left + fromSize.width : from.left;
  const y1 = from.top + fromSize.height / 2;
  const x2 = leftToRight ? gateway.left : gateway.left + GATEWAY_WIDTH;
  const y2 = gateway.top + GATEWAY_HEIGHT / 2;
  const direction = leftToRight ? 1 : -1;
  const bend = Math.max(60, Math.abs(x2 - x1) * 0.42);
  const c1x = x1 + direction * bend;
  const c2x = x2 - direction * bend;
  return {
    x1,
    y1,
    c1x,
    c1y: y1,
    c2x,
    c2y: y2,
    x2,
    y2,
    labelX: (x1 + 3 * c1x + 3 * c2x + x2) / 8,
    labelY: (y1 + 3 * y1 + 3 * y2 + y2) / 8,
  };
}

/**
 * Last-resort position, when there is no way to compute anything better.
 *
 * It is still a grid: it does not tell the pipeline's story, and that is why
 * `autoLayout` exists. It is kept only for whoever is absent from the computed arrangement.
 */
export function fallbackPosition(index: number): CanvasPosition {
  return { left: 48 + (index % 3) * 310, top: 96 + Math.floor(index / 3) * 240 };
}

export interface LayoutNode {
  id: string;
  start_layer: number;
  end_layer: number;
  /** A plugin engine's part in a prefill/decode split: a prefill machine heads its range, the others follow it. */
  role?: "prefill" | "shared" | null;
}

/**
 * Arranges the network as the pipeline really is: one column per stage, from
 * left to right. Replicas of a range and alternative ranges that start at the
 * same point are stacked in the same column.
 *
 * The grid that existed before ignored the topology, so a four-machine network
 * came out with three in a row and the fourth on the line below, and the edge
 * linking the two ends crossed the whole canvas. The layout here is what makes
 * a fork look like a fork: whatever branches sits side by side at the same x,
 * and whatever receives back sits on the axis between the two.
 *
 * **The input order does not matter.** The peer list reorders itself on every
 * cluster refresh, and an arrangement that depended on it would jitter on the
 * screen on every poll - ties are broken by `start_layer`, then `end_layer`, then `id`.
 */
export function autoLayout(
  nodes: LayoutNode[],
  sizeOf: (id: string) => CardSize = () => CARD_SIZE,
  gaps: { x: number; y: number } = { x: GAP_X, y: GAP_Y },
): Record<string, CanvasPosition> {
  if (nodes.length === 0) return {};

  // The column is the depth in the pipeline, not the exact range: 16-30 and 16-40
  // start at the same point and are alternative paths, so they sit side by
  // side. The depth is the longest chain of earlier ranges.
  const byStart = [...nodes].sort(
    (a, b) => a.start_layer - b.start_layer || a.end_layer - b.end_layer || a.id.localeCompare(b.id),
  );
  const depth = new Map<string, number>();
  for (const node of byStart) {
    let level = 0;
    for (const anterior of byStart) {
      if (anterior.end_layer < node.start_layer && anterior.end_layer >= anterior.start_layer) {
        level = Math.max(level, (depth.get(anterior.id) ?? 0) + 1);
      }
    }
    depth.set(node.id, level);
  }
  // A prefill/decode split: the prefill machines of a range are its head, and the
  // machines that decode it take the next column, after them, instead of being
  // stacked with them as replicas.
  for (const node of byStart) {
    if (node.role === "prefill") continue;
    const headed = byStart.some((other) =>
      other.role === "prefill" && other.start_layer === node.start_layer && other.end_layer === node.end_layer);
    if (headed) depth.set(node.id, (depth.get(node.id) ?? 0) + 1);
  }
  const columns = new Map<number, LayoutNode[]>();
  for (const node of byStart) {
    const level = depth.get(node.id) ?? 0;
    const current = columns.get(level);
    if (current) current.push(node);
    else columns.set(level, [node]);
  }
  const sorted = [...columns.entries()].sort(([a], [b]) => a - b);

  // Minimized cards take less room: the column has the width of its widest card
  // and the stack sums the real heights, so a minimized network fits on a
  // screen that the open network did not fit. In a column, the replicas
  // of the first range are centered on the axis - it is the path that goes straight -
  // and the alternative ranges go below it.
  const positions: Record<string, CanvasPosition> = {};
  let left = ORIGIN_X;
  sorted.forEach(([, column]) => {
    const ranges = new Map<string, LayoutNode[]>();
    for (const node of column) {
      const key = `${node.start_layer}-${node.end_layer}`;
      const current = ranges.get(key);
      if (current) current.push(node);
      else ranges.set(key, [node]);
    }
    const groups = [...ranges.values()].map((group) => [...group].sort((a, b) => a.id.localeCompare(b.id)));
    const sizes = column.map((node) => sizeOf(node.id));
    const gap = sizes.every((size) => size.height < CARD_HEIGHT) ? gaps.y / 2 : gaps.y;
    const heightOf = (group: LayoutNode[]) =>
      group.reduce((sum, node) => sum + sizeOf(node.id).height, 0) + (group.length - 1) * gap;
    let top = AXIS_Y - heightOf(groups[0]) / 2;
    for (const group of groups) {
      for (const node of group) {
        positions[node.id] = { left, top };
        top += sizeOf(node.id).height + gap;
      }
    }
    left += Math.max(...sizes.map((size) => size.width)) + gaps.x;
  });
  return positions;
}

/**
 * The Deploy Studio's arrangement, the Flow's (`autoLayout`) with more room: one
 * column per depth in the layer chain, so the heads (whoever serves layer 0) are
 * the first column, stacked one under the other, and each later range follows
 * the ranges it continues. Machines on the map without a range yet are not part
 * of any stage: they sit side by side in a row under the plan (on the axis when
 * nothing is assigned).
 */
export function deployLayout(
  assigned: LayoutNode[],
  unassignedIds: string[],
  size: CardSize = CARD_SIZE,
): Record<string, CanvasPosition> {
  // Wider than the Flow's: the deploy cards carry a footer (the model a machine
  // lacks, its download) and the RTT label sits on the edge between columns.
  const rowGap = DEPLOY_GAP_Y + 16;
  const positions = autoLayout(assigned, () => size, { x: DEPLOY_GAP_X, y: DEPLOY_GAP_Y });
  const bottom = assigned.length > 0
    ? Math.max(...Object.values(positions).map((position) => position.top + size.height))
    : null;
  const rowTop = bottom == null ? AXIS_Y - size.height / 2 : bottom + rowGap * 2;
  [...unassignedIds].sort().forEach((id, index) => {
    positions[id] = { left: ORIGIN_X + index * (size.width + rowGap), top: rowTop };
  });
  return positions;
}

/**
 * Rehydrates a canvas without moving whoever was already drawn.
 *
 * A peer joining is a topology change, but it is not a request to reorganize
 * the whole composition. Session positions take precedence over the persisted
 * ones; the automatic arrangement only provides places for new nodes.
 */
export function mergePositions(
  current: Record<string, CanvasPosition>,
  persisted: Record<string, CanvasPosition>,
  arrangement: Record<string, CanvasPosition>,
  nodeIds: string[],
): Record<string, CanvasPosition> {
  const next: Record<string, CanvasPosition> = {};
  for (const nodeId of nodeIds) {
    next[nodeId] = current[nodeId] ?? persisted[nodeId] ?? arrangement[nodeId] ?? fallbackPosition(nodeIds.indexOf(nodeId));
  }
  return next;
}

export function readPositions(key: string): Record<string, CanvasPosition> {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(key) ?? "{}");
    if (!parsed || typeof parsed !== "object") return {};
    return Object.fromEntries(
      Object.entries(parsed).filter(([, value]) => {
        if (!value || typeof value !== "object") return false;
        const position = value as Partial<CanvasPosition>;
        return Number.isFinite(position.left) && Number.isFinite(position.top);
      }),
    ) as Record<string, CanvasPosition>;
  } catch {
    return {};
  }
}

export function writePositions(key: string, positions: Record<string, CanvasPosition>) {
  try {
    localStorage.setItem(key, JSON.stringify(positions));
  } catch {
    /* quota or security: the drag still works for this session */
  }
}

export function clearPositions(key: string) {
  try {
    localStorage.removeItem(key);
  } catch {
    /* storage blocked; the canvas resets visually regardless */
  }
}

/**
 * Which Flow cards are minimized. One default for all (the model bar's button)
 * and per-card exceptions (each card's button); the global button erases the
 * exceptions, because a "minimize all" that left one open would not be "all".
 * It stays in the browser, like the arrangement: it is the viewer's preference,
 * not network state.
 */
export type CardModes = { compact: boolean; overrides: Record<string, boolean> };

const CARD_MODES_KEY = storageKey("network-card-modes");

export function readCardModes(): CardModes {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(CARD_MODES_KEY) ?? "{}");
    if (!parsed || typeof parsed !== "object") return { compact: false, overrides: {} };
    const value = parsed as Partial<CardModes>;
    const overrides = value.overrides && typeof value.overrides === "object"
      ? Object.fromEntries(Object.entries(value.overrides).filter(([, compact]) => typeof compact === "boolean"))
      : {};
    return { compact: value.compact === true, overrides };
  } catch {
    return { compact: false, overrides: {} };
  }
}

export function writeCardModes(modes: CardModes) {
  try {
    localStorage.setItem(CARD_MODES_KEY, JSON.stringify(modes));
  } catch {
    /* storage blocked: the toggle still works for this session */
  }
}

export function isCardCompact(modes: CardModes, id: string) {
  return modes.overrides[id] ?? modes.compact;
}
