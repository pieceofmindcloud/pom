/**
 * Take the whole pipeline offline, at once.
 *
 * The opposite of "Deploy to the network". "Unload model" already existed, but it applies
 * to a single machine: undoing a four-box deployment was four
 * trips to each one's panel, and the one that got forgotten would keep its slice
 * taking RAM and VRAM while serving nothing - the pipeline only plans with
 * closed coverage, so a machine left out is not half a pipeline, it is none.
 *
 * Two things this screen owes the operator:
 *
 * * **Ask first**, naming the machines that will be freed. The action
 *   reaches the whole cluster and is not undone by clicking again.
 * * **Report machine by machine.** A peer that did not answer still has its
 *   memory held, and that is exactly what the operator needs to know to go
 *   to it. A "done!" that swallows the failure sends the person away thinking they
 *   freed memory that is still occupied.
 *
 * Nothing is erased from disk - see `undeploy_endpoint` in `deploy.rs`.
 */
import { useState } from "react";
import { CircleCheck, CircleX, PowerOff } from "lucide-react";
import { undeployPipeline, type UndeployReport } from "../lib/api";
import { layerRange } from "../lib/format";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Confirm, Modal } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";

export interface UndeployTarget {
  node_id: string;
  hostname: string;
  start_layer: number;
  end_layer: number;
}

export function UndeployPipeline({
  group,
  targets,
  deployId,
  label,
  inProgress = false,
}: {
  /** Model that is live. */
  group: string;
  /** Machines that serve this model now, in pipeline order. */
  targets: UndeployTarget[];
  /**
   * Which deployment goes offline. Without it, whoever is serving the label goes -
   * the panel's behavior, which shows one pipeline at a time. With it the
   * request is exact, which is what the deployments screen needs when the same
   * model is live twice.
   */
  deployId?: string;
  /** What the title names, when it is not `group` (a deploy still starting). */
  label?: string;
  /**
   * A deploy of this pipeline is still copying or loading: the undeploy stops
   * it too, so the operator who gave up does not wait for the machines.
   */
  inProgress?: boolean;
}) {
  const { t } = useI18n();
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<UndeployReport | null>(null);
  const toast = useToast();
  const invalidate = useInvalidate();

  async function undeploy() {
    setBusy(true);
    try {
      const r = await undeployPipeline(group, deployId);
      setReport(r);
      if (r.cancelled && r.results.length === 0) {
        toast.success(t("undeploy.button"), t("undeploy.cancelled"));
      } else if (r.failed === 0) {
        toast.success(t("undeploy.button"), t("undeploy.allReleased", { count: r.released }));
      } else {
        toast.warning(
          t("undeploy.failedTitle"),
          r.released === 0
            ? t("undeploy.noneReleased")
            : t("undeploy.partial", { released: r.released, failed: r.failed }),
        );
      }
      invalidate();
    } catch (e) {
      toast.error(
        t("undeploy.failedTitle"),
        e instanceof Error ? e.message : String(e),
      );
    } finally {
      setBusy(false);
      setAsking(false);
    }
  }

  return (
    <>
      <Button size="sm" variant="outline" onClick={() => setAsking(true)}>
        <PowerOff className="h-3.5 w-3.5" />
        {t("undeploy.button")}
      </Button>

      <Confirm
        open={asking}
        busy={busy}
        tone="danger"
        title={t("undeploy.title", { group: label ?? group })}
        confirmLabel={t("undeploy.confirm")}
        onCancel={() => setAsking(false)}
        onConfirm={() => void undeploy()}
        description={
          <>
            {inProgress && <p className="mb-2">{t("undeploy.inProgress")}</p>}
            {targets.length > 0 && <p>{t("undeploy.description", { count: targets.length })}</p>}
            <p className="mt-2 text-xs text-faint">{t("undeploy.keepsFiles")}</p>
            {targets.length > 0 && <div className="mt-3 overflow-hidden rounded-lg border border-line">
              <div className="eyebrow border-b border-line bg-raised px-3 py-1.5">
                {t("undeploy.machines")}
              </div>
              <ul className="divide-y divide-line">
                {targets.map((m) => (
                  <li
                    key={m.node_id}
                    className="flex items-baseline justify-between gap-3 px-3 py-2"
                  >
                    <span className="truncate text-[0.83rem] text-fg">{m.hostname}</span>
                    <span className="shrink-0 font-mono text-xs text-muted">
                      {layerRange(m.start_layer, m.end_layer)}
                    </span>
                  </li>
                ))}
              </ul>
            </div>}
          </>
        }
      />

      <Modal
        open={report != null}
        title={t("undeploy.reportTitle")}
        onClose={() => setReport(null)}
        footer={
          <Button variant="primary" onClick={() => setReport(null)}>
            {t("common.close")}
          </Button>
        }
      >
        {report && (
          <>
            {report.cancelled && <p className="mb-2">{t("undeploy.cancelled")}</p>}
            <p>
              {report.failed === 0
                ? t("undeploy.allReleased", { count: report.released })
                : report.released === 0
                  ? t("undeploy.noneReleased")
                  : t("undeploy.partial", {
                      released: report.released,
                      failed: report.failed,
                    })}
            </p>
            <ul className="mt-3 divide-y divide-line overflow-hidden rounded-lg border border-line">
              {report.results.map((r) => (
                <li key={r.node_id} className="flex gap-3 px-3 py-2.5">
                  {r.released ? (
                    <CircleCheck className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />
                  ) : (
                    <CircleX className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
                  )}
                  <div className="min-w-0 flex-1">
                    <div className="flex items-baseline justify-between gap-3">
                      <span className="truncate text-[0.83rem] font-medium text-fg">
                        {r.hostname}
                      </span>
                      <span className="shrink-0 font-mono text-xs text-muted">
                        {layerRange(r.start_layer, r.end_layer)}
                      </span>
                    </div>
                    <p className="break-path mt-0.5 text-xs leading-relaxed text-muted">
                      {r.released
                        ? t("undeploy.released")
                        : (r.error ?? t("undeploy.stillLoaded"))}
                    </p>
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </Modal>
    </>
  );
}
