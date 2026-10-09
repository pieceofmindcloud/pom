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
 * Identifica a implantação que este navegador está desenhando, para um arranjo
 * feito à mão não vazar para outra rede.
 *
 * O modelo e o conjunto de faixas entram na impressão digital, porque mudar
 * qualquer um deles desenha outra rede. O **`changed_at` não entra**, e isso é
 * a correção de um defeito real: ele muda a cada reinício de nó, o que mudava a
 * chave, o que fazia `readPositions` devolver vazio - o arranjo do operador não
 * era reorganizado, era perdido, e a tela voltava para a grade toda vez que uma
 * máquina reiniciava. O campo continua no tipo porque quem chama já o tem em
 * mãos e um dia pode voltar a precisar dele.
 */
export function deploymentLayoutKey(model: string, peers: DeploymentPeer[]) {
  const fingerprint = peers
    .map((peer) => `${peer.node_id}:${peer.start_layer}-${peer.end_layer}`)
    .sort()
    .join("|");
  return `${STORAGE_PREFIX}:${model}:${fingerprint}`;
}

/** Medidas do card em `ModelNetworkCanvas`. Mudou lá, muda aqui. */
export const CARD_WIDTH = 264;
export const CARD_HEIGHT = 204;
/**
 * O card minimizado: nome, estado, faixa, velocidade e uma linha de VRAM.
 * Altura fixa, e não mínima, porque a aresta se prende no meio dela - um card
 * que crescesse com o conteúdo soltaria a seta da borda.
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
 * A chave do gateway no mesmo mapa de posições das máquinas. Arrastado, ele
 * fica onde foi posto; sem arrasto, segue o último estágio como antes.
 */
export const GATEWAY_LAYOUT_ID = "__gateway__";
const GAP_X = 148;
const GAP_Y = 40;
const ORIGIN_X = 48;
/** Eixo horizontal em torno do qual cada coluna é centrada. */
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

/** Põe o gateway virtual depois do último estágio, alinhado ao seu centro. */
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
 * Geometria da perna final do ciclo. Para a direita quando o gateway está
 * depois do último estágio, que é o arranjo de sempre; arrastado para trás,
 * a curva sai e chega pelos lados que se olham, como `routeGeometry`.
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
 * Posição de último recurso, quando não há como calcular nada melhor.
 *
 * Continua sendo uma grade: ela não conta a história do pipeline, e é por isso
 * que `autoLayout` existe. Fica só para quem não aparece no arranjo calculado.
 */
export function fallbackPosition(index: number): CanvasPosition {
  return { left: 48 + (index % 3) * 310, top: 96 + Math.floor(index / 3) * 240 };
}

export interface LayoutNode {
  id: string;
  start_layer: number;
  end_layer: number;
}

/**
 * Arruma a rede como o pipeline realmente é: uma coluna por estágio, da
 * esquerda para a direita. Réplicas de uma faixa e faixas alternativas que
 * começam no mesmo ponto ficam empilhadas na mesma coluna.
 *
 * A grade que existia antes ignorava a topologia, então uma rede de quatro
 * máquinas saía com três em fila e a quarta na linha de baixo, e a aresta que
 * ligava as duas pontas cruzava o canvas inteiro. O desenho aqui é o que faz a
 * bifurcação parecer uma bifurcação: quem se ramifica fica lado a lado no
 * mesmo x, e quem recebe de volta fica no eixo dos dois.
 *
 * **A ordem de entrada não influi.** A lista de peers se reordena a cada
 * refresh do cluster, e um arranjo que dependesse dela tremeria na tela a cada
 * poll - o desempate é por `start_layer`, depois `end_layer`, depois `id`.
 */
export function autoLayout(
  nodes: LayoutNode[],
  sizeOf: (id: string) => CardSize = () => CARD_SIZE,
): Record<string, CanvasPosition> {
  if (nodes.length === 0) return {};

  // A coluna é a profundidade no pipeline, não a faixa exata: 16-30 e 16-40
  // começam no mesmo ponto e são caminhos alternativos, então ficam lado a
  // lado. A profundidade é a cadeia mais longa de faixas anteriores.
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
  const columns = new Map<number, LayoutNode[]>();
  for (const node of byStart) {
    const level = depth.get(node.id) ?? 0;
    const current = columns.get(level);
    if (current) current.push(node);
    else columns.set(level, [node]);
  }
  const sorted = [...columns.entries()].sort(([a], [b]) => a - b);

  // Cards minimizados ocupam menos: a coluna tem a largura do seu card mais
  // largo e a pilha soma as alturas reais, então uma rede minimizada cabe
  // inteira numa tela que a rede aberta não cabia. Numa coluna, as réplicas
  // da primeira faixa ficam centradas no eixo - é o caminho que segue reto -
  // e as faixas alternativas descem abaixo dela.
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
    const gap = sizes.every((size) => size.height < CARD_HEIGHT) ? GAP_Y / 2 : GAP_Y;
    const heightOf = (group: LayoutNode[]) =>
      group.reduce((sum, node) => sum + sizeOf(node.id).height, 0) + (group.length - 1) * gap;
    let top = AXIS_Y - heightOf(groups[0]) / 2;
    for (const group of groups) {
      for (const node of group) {
        positions[node.id] = { left, top };
        top += sizeOf(node.id).height + gap;
      }
    }
    left += Math.max(...sizes.map((size) => size.width)) + GAP_X;
  });
  return positions;
}

/**
 * Reidrata um canvas sem mover quem já estava desenhado.
 *
 * A entrada de um peer é uma mudança de topologia, mas não é um pedido para
 * reorganizar a composição inteira. Posições da sessão têm precedência sobre
 * as persistidas; o arranjo automático só fornece lugares para nós novos.
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
 * Quais cards do Fluxo estão minimizados. Um padrão para todos (o botão da
 * barra de modelos) e exceções por card (o botão de cada um); o botão global
 * apaga as exceções, porque "minimizar todos" que deixasse um aberto não
 * seria "todos". Fica no navegador, como o arranjo: é preferência de quem
 * olha, não estado da rede.
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
