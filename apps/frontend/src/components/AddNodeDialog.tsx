/**
 * Somar uma máquina a um modelo que já está no ar.
 *
 * Toca um nó só: `/api/ui/deploy/node` aplica no alvo e não encosta em quem
 * está servindo — reimplantar o conjunto inteiro para acrescentar uma máquina
 * derrubaria a inferência que a rede está fazendo agora.
 *
 * Faixa sobreposta aqui não é erro, é o objetivo: é assim que se cria uma rota
 * paralela. Por isso o aviso é afirmativo, e não bloqueia o botão.
 */
import { useEffect, useRef, useState } from "react";
import { Route } from "lucide-react";
import { api } from "../lib/api";
import { Confirm } from "./Confirm";
import { useI18n } from "../i18n/provider";

export interface AddNodePeer {
  node_id: string;
  short_id: string;
  hostname: string;
}

export interface AddNodeSlice {
  start_layer: number;
  end_layer: number;
  label: string;
}

export function AddNodeDialog({
  open,
  group,
  peers,
  slices,
  existingRanges,
  onClose,
  onDone,
}: {
  open: boolean;
  /** Conjunto de fatias, nomeado como o catálogo o agrupa. */
  group?: string;
  peers: AddNodePeer[];
  slices: AddNodeSlice[];
  /** Faixas que já estão no ar, para saber o que a escolha sobrepõe. */
  existingRanges: [number, number][];
  onClose: () => void;
  onDone?: (overlapping: number[]) => void;
}) {
  const { t } = useI18n();
  const [nodeId, setNodeId] = useState(peers[0]?.node_id ?? "");
  const [sliceKey, setSliceKey] = useState(() => keyOf(slices[0]));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Re-semear é ato de *abrir* o diálogo, não de receber props novas: o painel
  // repolla o cluster, `peers`/`slices` chegam como arrays novos a cada
  // resposta, e um efeito que dependa deles joga fora a escolha do operador no
  // meio da edição - era isso que "voltava sozinho para outra máquina".
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open && !wasOpen.current) {
      setNodeId(peers[0]?.node_id ?? "");
      setSliceKey(keyOf(slices[0]));
      setError(null);
    }
    wasOpen.current = open;
  }, [open, peers, slices]);

  // A escolha só é trocada quando ela some da lista (peer que saiu da rede,
  // fatia que deixou de existir) - aí ficar com um valor fantasma submeteria
  // algo que não está mais no dropdown.
  useEffect(() => {
    if (!open) return;
    if (peers.length > 0 && !peers.some((p) => p.node_id === nodeId)) {
      setNodeId(peers[0].node_id);
    }
    if (slices.length > 0 && !slices.some((s) => keyOf(s) === sliceKey)) {
      setSliceKey(keyOf(slices[0]));
    }
  }, [open, peers, slices, nodeId, sliceKey]);

  const slice = slices.find((s) => keyOf(s) === sliceKey);
  const overlapping = slice
    ? rangeOverlap(existingRanges, [slice.start_layer, slice.end_layer])
    : [];

  async function addNode() {
    if (!slice || !nodeId) return;
    setBusy(true);
    setError(null);
    try {
      const r = await api.post<{ overlapping_layers: number[] }>(
        "/api/ui/deploy/node",
        {
          group: group ?? "",
          target_node_id: nodeId,
          start_layer: slice.start_layer,
          end_layer: slice.end_layer,
        },
      );
      onDone?.(r.overlapping_layers ?? []);
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Confirm
      open={open}
      busy={busy}
      title={t("deploy.addNodeTitle")}
      confirmLabel={t("deploy.addNode")}
      onCancel={onClose}
      onConfirm={() => void addNode()}
      description={
        <div className="flex flex-col gap-3 text-sm">
          <label className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("deploy.addNodeMachine")}</span>
            <select
              className="rounded-md border border-line bg-raised px-2 py-1.5 text-sm"
              value={nodeId}
              onChange={(e) => setNodeId(e.target.value)}
            >
              {peers.map((p) => (
                <option key={p.node_id} value={p.node_id}>
                  {p.hostname || p.short_id}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("deploy.addNodeSlice")}</span>
            <select
              className="rounded-md border border-line bg-raised px-2 py-1.5 font-mono text-sm"
              value={sliceKey}
              onChange={(e) => setSliceKey(e.target.value)}
            >
              {slices.map((s) => (
                <option key={keyOf(s)} value={keyOf(s)}>
                  {s.label}
                </option>
              ))}
            </select>
          </label>

          {overlapping.length > 0 && (
            <span className="flex items-start gap-2 text-xs text-iris">
              <Route className="mt-0.5 h-4 w-4 shrink-0" />
              <span>{t("deploy.parallelRoute")}</span>
            </span>
          )}
          {error && <span className="text-xs text-amber">{error}</span>}
        </div>
      }
    />
  );
}

/**
 * A fatia é identificada pela faixa, que é o que a requisição carrega - por
 * índice, uma lista reordenada entre dois pollings apontaria para outra fatia.
 */
function keyOf(slice?: AddNodeSlice): string {
  return slice ? `${slice.start_layer}-${slice.end_layer}` : "";
}

/** As camadas que a faixa escolhida compartilha com as que já estão no ar. */
export function rangeOverlap(
  existing: [number, number][],
  range: [number, number],
): number[] {
  const shared: number[] = [];
  for (let layer = range[0]; layer <= range[1]; layer++) {
    if (existing.some(([start, end]) => layer >= start && layer <= end)) {
      shared.push(layer);
    }
  }
  return shared;
}
