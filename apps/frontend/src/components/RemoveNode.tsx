/**
 * Tirar uma máquina de um modelo que está no ar, sem tocar nas outras.
 *
 * A confirmação distingue os dois casos, porque as consequências não se
 * parecem: se outra máquina serve a mesma faixa, a rede só perde uma rota
 * paralela e continua atendendo; se esta é a última, as camadas dela ficam
 * descobertas e o pipeline para de planejar até alguém assumir. Um aviso único
 * para os dois é um aviso que o operador aprende a clicar sem ler.
 *
 * Qual dos dois é vem da mesma conta que o nó faz (`uncovered_after_removal`),
 * não de um palpite da tela: `existingRanges` são as faixas que os peers
 * anunciam agora.
 */
import { useState } from "react";
import { Unplug } from "lucide-react";
import { removeNode } from "../lib/api";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Confirm } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";

export function RemoveNode({
  nodeId,
  group,
  deployId,
  range,
  existingRanges,
}: {
  nodeId: string;
  group: string;
  deployId?: string;
  range: [number, number];
  /** Faixas dos *outros* peers que servem este modelo. */
  existingRanges: [number, number][];
}) {
  const { t } = useI18n();
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  const uncovered = layersLeftUncovered(existingRanges, range);

  async function remove() {
    setBusy(true);
    try {
      const r = await removeNode(nodeId, group, deployId);
      if (r.uncovered_layers.length > 0) {
        toast.warning(
          t("deploy.removeNodeTitle"),
          t("deploy.removeLeavesGap", { layers: rangeText(r.uncovered_layers) }),
        );
      } else {
        toast.success(t("deploy.removeNodeTitle"), t("deploy.removeReplica"));
      }
      invalidate();
    } catch (e) {
      toast.fromError(t("deploy.removeNode"), e);
    } finally {
      setBusy(false);
      setAsking(false);
    }
  }

  return (
    <>
      <Button
        size="sm"
        variant="outline"
        onClick={() => setAsking(true)}
        className="shrink-0"
      >
        <Unplug className="h-3.5 w-3.5" />
        {t("deploy.removeNode")}
      </Button>

      <Confirm
        open={asking}
        busy={busy}
        tone={uncovered.length > 0 ? "danger" : "primary"}
        title={t("deploy.removeNodeTitle")}
        confirmLabel={t("deploy.removeNode")}
        onCancel={() => setAsking(false)}
        onConfirm={() => void remove()}
        description={
          uncovered.length > 0
            ? t("deploy.removeLeavesGap", { layers: rangeText(uncovered) })
            : t("deploy.removeReplica")
        }
      />
    </>
  );
}

/** As camadas que ninguém mais cobre depois de tirar esta faixa. */
export function layersLeftUncovered(
  remaining: [number, number][],
  removed: [number, number],
): number[] {
  const lost: number[] = [];
  for (let layer = removed[0]; layer <= removed[1]; layer++) {
    if (!remaining.some(([start, end]) => layer >= start && layer <= end)) {
      lost.push(layer);
    }
  }
  return lost;
}

/** "0-19" em vez de "0, 1, 2, …": a faixa é o que o operador reconhece. */
export function rangeText(layers: number[]): string {
  if (layers.length === 0) return "";
  return `${layers[0]}-${layers[layers.length - 1]}`;
}
