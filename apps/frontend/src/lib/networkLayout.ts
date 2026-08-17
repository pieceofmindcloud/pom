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
export const CARD_HEIGHT = 202;
const GAP_X = 92;
const GAP_Y = 40;
const ORIGIN_X = 48;
/** Eixo horizontal em torno do qual cada coluna é centrada. */
const AXIS_Y = 300;

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
 * Arruma a rede como o pipeline realmente é: uma coluna por faixa de camadas,
 * da esquerda para a direita, e as réplicas de uma mesma faixa empilhadas
 * dentro da coluna, centradas no mesmo eixo.
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
export function autoLayout(nodes: LayoutNode[]): Record<string, CanvasPosition> {
  if (nodes.length === 0) return {};

  const colunas = new Map<string, LayoutNode[]>();
  for (const node of nodes) {
    const chave = `${node.start_layer}-${node.end_layer}`;
    const atual = colunas.get(chave);
    if (atual) atual.push(node);
    else colunas.set(chave, [node]);
  }

  const ordenadas = [...colunas.entries()].sort(([, a], [, b]) =>
    a[0].start_layer - b[0].start_layer || a[0].end_layer - b[0].end_layer,
  );

  const posicoes: Record<string, CanvasPosition> = {};
  ordenadas.forEach(([, coluna], indice) => {
    const empilhados = [...coluna].sort((a, b) => a.id.localeCompare(b.id));
    const altura = empilhados.length * CARD_HEIGHT + (empilhados.length - 1) * GAP_Y;
    const topo = AXIS_Y - altura / 2;
    const left = ORIGIN_X + indice * (CARD_WIDTH + GAP_X);
    empilhados.forEach((node, linha) => {
      posicoes[node.id] = { left, top: topo + linha * (CARD_HEIGHT + GAP_Y) };
    });
  });
  return posicoes;
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
