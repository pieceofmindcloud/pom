/**
 * Take a machine out of a model that is live, without touching the others.
 *
 * The confirmation distinguishes the two cases, because the consequences are not
 * alike: if another machine serves the same range, the network only loses a
 * parallel route and keeps answering; if this is the last one, its layers are left
 * uncovered and the pipeline stops planning until someone takes over. A single warning
 * for both is a warning that the operator learns to click without reading.
 *
 * Which of the two it is comes from the same calculation the node makes
 * (`uncovered_after_removal`), not from a guess by the screen: `existingRanges` are the ranges
 * announce now.
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
  /** Ranges of the *other* peers that serve this model. */
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

/** The layers that nobody else covers after taking out this range. */
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

/** "0-19" instead of "0, 1, 2, ...": the range is what the operator recognizes. */
export function rangeText(layers: number[]): string {
  if (layers.length === 0) return "";
  return `${layers[0]}-${layers[layers.length - 1]}`;
}
