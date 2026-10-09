/**
 * Giving this machine's memory back, on the machine's own terms.
 *
 * The decision belongs to the node, not to a coordinator: the operator sitting
 * at a machine that suddenly needs its RAM says so here, and the cluster deals
 * with the consequence. So the release is never refused — the only thing this
 * screen owes the operator is an honest account of what happens next.
 *
 * That account is fetched, not guessed. Asking the node who *would* take the
 * slice over means the confirmation can say "another machine picks it up" or
 * "these layers go down" truthfully; a blanket warning on both paths is one the
 * operator learns to click through without reading.
 */
import { useState } from "react";
import { HandCoins, TriangleAlert } from "lucide-react";
import { api } from "../lib/api";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { layerRange } from "../lib/format";
import { Confirm } from "./Confirm";
import { Button, Footnote } from "./ui";
import { useI18n } from "../i18n/provider";

interface Candidate {
  node_id: string;
  hostname: string;
}

interface CandidateProbe {
  serving: boolean;
  slice?: {
    model: string;
    start_layer: number;
    end_layer: number;
    total_layers: number;
  };
  candidate: Candidate | null;
}

interface ReleaseResult {
  released: boolean;
  reassigned_to: Candidate | null;
  uncovered: boolean;
  reason?: string;
}

export function ReleaseSlice({
  startLayer,
  endLayer,
  /**
   * Only the button, for the machine card's toolbar. The note that the
   * wide version shows beside it is already said, and better, by the confirmation -
   * repeating it on a five-button bar was a line wasted on what nobody reads
   * before clicking.
   */
  compact = false,
}: {
  startLayer: number;
  endLayer: number;
  compact?: boolean;
}) {
  const { t } = useI18n();
  const [probe, setProbe] = useState<CandidateProbe | null>(null);
  const [probing, setProbing] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  const range = layerRange(startLayer, endLayer);

  async function ask() {
    setProbing(true);
    try {
      setProbe(await api.get<CandidateProbe>("/api/ui/release/candidate"));
    } catch {
      // Losing the probe must not block the release — the machine may need its
      // memory precisely because things are going wrong. Fall back to the
      // pessimistic wording, which is the safe thing to show when unsure.
      setProbe({ serving: true, candidate: null });
    } finally {
      setProbing(false);
    }
  }

  async function release() {
    setBusy(true);
    try {
      const r = await api.post<ReleaseResult>("/api/ui/release", {});
      if (r.reassigned_to) {
        toast.success(
          t("release.reassigned", { range, hostname: r.reassigned_to.hostname }),
          t("release.reassignedDetail"),
        );
      } else {
        toast.warning(
          t("release.uncovered", { range }),
          r.reason ??
            t("release.uncoveredDetail"),
        );
      }
      invalidate();
    } catch (e) {
      toast.fromError(t("release.failed"), e);
    } finally {
      setBusy(false);
      setProbe(null);
    }
  }

  const successor = probe?.candidate;

  const trigger = (
    <Button
      size="sm"
      variant="outline"
      loading={probing}
      onClick={() => void ask()}
      className="shrink-0"
      icon={compact ? <HandCoins className="h-3.5 w-3.5" /> : undefined}
    >
      {t("release.button")}
    </Button>
  );

  return (
    <>
      {compact ? (
        trigger
      ) : (
        <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
          <Footnote icon={<HandCoins className="h-3.5 w-3.5" />} className="min-w-0 flex-1">
            {t("release.note", { range })}
          </Footnote>
          {trigger}
        </div>
      )}

      <Confirm
        open={probe != null}
        busy={busy}
        tone={successor ? "primary" : "danger"}
        title={t("release.title", { range })}
        confirmLabel={successor ? t("release.withSuccessor") : t("release.alone")}
        onCancel={() => setProbe(null)}
        onConfirm={() => void release()}
        description={
          successor ? (
            <>
              {t("release.successorDetail", { hostname: successor.hostname })}
            </>
          ) : (
            <span className="flex items-start gap-2">
              <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
              <span>
                {t("release.noSuccessor", { range })}
              </span>
            </span>
          )
        }
      />
    </>
  );
}
