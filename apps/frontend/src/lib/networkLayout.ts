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
 * Identifies the deployment displayed by this browser. Reconfiguring a node
 * changes its status timestamp, and a different model or range changes the
 * fingerprint too, so old hand-arranged layouts do not leak into a new deploy.
 */
export function deploymentLayoutKey(model: string, peers: DeploymentPeer[]) {
  const fingerprint = peers
    .map((peer) => `${peer.node_id}:${peer.start_layer}-${peer.end_layer}:${peer.changed_at ?? ""}`)
    .sort()
    .join("|");
  return `${STORAGE_PREFIX}:${model}:${fingerprint}`;
}

export function fallbackPosition(index: number): CanvasPosition {
  return { left: 48 + (index % 3) * 310, top: 96 + Math.floor(index / 3) * 240 };
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
