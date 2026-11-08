// Image deployments on the Deployments screen: one row per model with an
// Image badge, its stages and replicas, its versions (on the node that
// deployed it) and Redeploy of a version.
import { ChevronDown, ChevronRight, Image as ImageIcon } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../i18n/provider";
import { ApiError } from "../lib/api";
import { redeployImage } from "../lib/imagePipeline";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { ImageDeployment } from "../lib/types";
import { ImageUndeploy } from "./ImagePipelineMap";
import { Badge, Button } from "./ui";

export function ImageDeploymentsTable({ deployments }: { deployments: ImageDeployment[] }) {
  const { t } = useI18n();
  const toast = useToast();
  const invalidate = useInvalidate();
  const [open, setOpen] = useState<string | null>(null);
  if (deployments.length === 0) return null;

  async function redeploy(deployment: ImageDeployment, version: number) {
    try {
      await redeployImage(deployment.deploy_id, version);
      toast.success(t("imageTable.redeployed", { version }));
      invalidate();
    } catch (error) {
      toast.error(t("imageTable.redeployFailed"), error instanceof ApiError ? error.message : String(error));
    }
  }

  return (
    <section className="border-t border-line" data-testid="image-deployments">
      <p className="eyebrow px-5 pt-4">{t("imageTable.title")}</p>
      <table className="w-full min-w-[72rem] text-sm">
        <tbody>
          {deployments.map((deployment) => {
            const expanded = open === deployment.deploy_id;
            return (
              <tr key={deployment.deploy_id} className="border-b border-line align-top">
                <td className="px-2 py-3">
                  <button type="button" aria-label={t("imageTable.versions")} aria-expanded={expanded} onClick={() => setOpen(expanded ? null : deployment.deploy_id)} className="rounded p-1 text-muted hover:text-fg">
                    {expanded ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" />}
                  </button>
                </td>
                <td className="px-5 py-3 font-mono text-[0.7rem] text-faint">{deployment.deploy_id}</td>
                <td className="px-5 py-3">
                  <div className="flex items-center gap-2">
                    <span className="text-fg">{deployment.name}</span>
                    <Badge tone="sky"><ImageIcon className="mr-1 inline h-3 w-3" />{t("imageMap.badge")}</Badge>
                  </div>
                  <ul className="mt-1 space-y-0.5 text-[0.7rem] text-muted">
                    {deployment.stages.map((stage) => (
                      <li key={stage.component}>
                        {t(`imageDeploy.component.${stage.component}`)}: {stage.replicas.map((r) => r.hostname).join(", ") || "-"}
                      </li>
                    ))}
                  </ul>
                  {expanded && (
                    <div className="mt-2 space-y-1" data-testid={`image-versions-${deployment.deploy_id}`}>
                      {(deployment.versions ?? []).slice().reverse().map((version) => (
                        <div key={version.version} className="flex items-center gap-2 rounded border border-line px-2 py-1 text-[0.7rem]">
                          <span className="font-mono">v{version.version}</span>
                          {version.version === deployment.active_version && <Badge tone="aqua">{t("imageTable.active")}</Badge>}
                          <span className="min-w-0 flex-1 truncate text-muted">
                            {version.stages.map((s) => `${t(`imageDeploy.component.${s.component}`)} ${s.replicas.map((r) => r.hostname || r.node_id.slice(0, 8)).join("+")}`).join(" · ")}
                          </span>
                          {version.version !== deployment.active_version && (
                            <Button size="sm" variant="outline" onClick={() => redeploy(deployment, version.version)}>{t("imageTable.redeploy")}</Button>
                          )}
                        </div>
                      ))}
                      {!deployment.recorded && <p className="text-[0.7rem] text-muted">{t("imageMap.notRecorded")}</p>}
                    </div>
                  )}
                </td>
                <td className="px-5 py-3">
                  <Badge tone={deployment.serving ? "aqua" : "amber"}>{deployment.serving ? t("imageTable.serving") : t("imageTable.notServing")}</Badge>
                </td>
                <td className="px-5 py-3"><ImageUndeploy deployment={deployment} /></td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}
