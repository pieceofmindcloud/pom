// An image deployment on the Dashboard: its three stages in data-flow order
// (text encoder -> diffusion -> VAE), each with its replicas, their state,
// calls in flight and the RTT from this node (ComfyUI calls each stage from
// the machine it runs on, and the node picks the nearest replica).
import clsx from "clsx";
import { ArrowRight, Image as ImageIcon, Plus, Power, ServerCog } from "lucide-react";
import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useI18n } from "../i18n/provider";
import { ApiError } from "../lib/api";
import { addImageReplica, machinesOf, planImage, undeployImage } from "../lib/imagePipeline";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { ChatTelemetryRtt, ImageDeployment, ImageStageName } from "../lib/types";
import { Confirm, Modal } from "./Confirm";
import { Button, Select } from "./ui";

export interface ChipModel {
  id: string;
  label: string;
  image?: boolean;
}

/** The model chips of the map's bar: the LLMs, then the image deployments. */
export function ModelChips({ models, selected, onChange }: { models: ChipModel[]; selected: string | undefined; onChange: (id: string) => void }) {
  const { t } = useI18n();
  return (
    <div className="flex max-w-[min(46rem,60vw)] overflow-x-auto" role="group" aria-label={t("canvas.modelGroupAria")}>
      {models.map((model) => (
        <button
          key={model.id}
          type="button"
          onClick={() => onChange(model.id)}
          aria-pressed={selected === model.id}
          className={clsx("flex h-8 shrink-0 items-center gap-1 rounded-md px-3 font-mono text-xs", selected === model.id ? "bg-iris text-white" : "text-muted hover:bg-raised hover:text-fg")}
        >
          {model.image && <ImageIcon className="h-3 w-3" aria-label={t("imageMap.badge")} />}
          {model.label}
        </button>
      ))}
    </div>
  );
}

const statusTone: Record<string, string> = {
  ready: "bg-lime",
  loading: "bg-amber",
  error: "bg-rose",
  missing: "bg-faint",
};

export function ImagePipelineMap({
  deployment,
  models,
  onModelChange,
  rtt,
  localNodeId,
}: {
  deployment: ImageDeployment;
  models: ChipModel[];
  onModelChange: (id: string) => void;
  rtt?: ChatTelemetryRtt[];
  localNodeId?: string;
}) {
  const { t } = useI18n();
  const rttTo = (nodeId: string) =>
    nodeId === localNodeId ? 0 : rtt?.find((m) => m.from_node_id === localNodeId && m.to_node_id === nodeId)?.rtt_ms;
  const covered = deployment.stages.filter((s) => s.replicas.some((r) => r.status === "ready")).length;
  return (
    <section className="relative flex h-full min-h-0 w-full flex-col overflow-auto bg-[radial-gradient(circle_at_1px_1px,rgba(91,78,255,.16)_1px,transparent_0)] p-4" style={{ backgroundSize: "18px 18px" }} data-testid="image-pipeline-map">
      <div className="mb-4 flex flex-wrap items-center gap-2 self-start rounded-xl border border-line bg-ink/90 p-1.5 shadow-panel">
        <ModelChips models={models} selected={`image:${deployment.deploy_id}`} onChange={onModelChange} />
        <span className={clsx("rounded-md px-2 py-1 text-[0.68rem]", deployment.serving ? "bg-aqua/15 text-aqua" : "bg-amber/15 text-amber")}>
          {t("imageMap.coverage", { covered, total: deployment.stages.length })}
        </span>
      </div>
      <div className="flex min-w-max flex-1 items-start gap-3">
        {deployment.stages.map((stage, index) => (
          <div key={stage.component} className="flex items-start gap-3">
            {index > 0 && <ArrowRight className="mt-10 h-4 w-4 shrink-0 text-iris" aria-hidden />}
            <div className="w-[15rem] rounded-xl border border-line bg-surface/80 p-3" data-testid={`map-stage-${stage.component}`}>
              <p className="eyebrow">{index + 1}. {t(`imageDeploy.component.${stage.component}`)}</p>
              <p className="mb-2 truncate font-mono text-[0.66rem] text-muted" title={stage.file.name}>{stage.file.name}</p>
              <div className="space-y-2">
                {stage.replicas.map((replica) => {
                  const ms = rttTo(replica.node_id);
                  return (
                    <div key={replica.node_id} className="rounded-lg border border-line bg-ink px-2 py-1.5" data-testid={`map-replica-${stage.component}-${replica.node_id}`}>
                      <div className="flex items-center gap-1.5">
                        <span className={clsx("h-2 w-2 shrink-0 rounded-full", statusTone[replica.status] ?? "bg-faint")} />
                        <ServerCog className="h-3.5 w-3.5 shrink-0 text-faint" />
                        <span className="min-w-0 flex-1 truncate text-[0.72rem] text-fg">{replica.hostname}</span>
                      </div>
                      <p className="font-mono text-[0.58rem] text-faint">
                        {t(`imageDeploy.status.${replica.status}`)} · {ms == null ? "RTT -" : `${ms.toFixed(1)} ms`}
                        {replica.in_flight != null ? ` · ${t("canvas.inFlight", { count: replica.in_flight })}` : ""}
                      </p>
                    </div>
                  );
                })}
                {stage.replicas.length === 0 && <p className="text-[0.66rem] text-rose">{t("imageMap.noReplica")}</p>}
              </div>
            </div>
          </div>
        ))}
      </div>
      <p className="mt-3 text-[0.66rem] text-faint">{t("imageMap.routing")}</p>
    </section>
  );
}

/** Add a replica of one stage: the machines the plan accepts for its file. */
export function ImageAddReplicaDialog({ deployment, open, onClose }: { deployment: ImageDeployment; open: boolean; onClose: () => void }) {
  const { t } = useI18n();
  const toast = useToast();
  const invalidate = useInvalidate();
  const [stage, setStage] = useState<ImageStageName>("diffusion");
  const [nodeId, setNodeId] = useState("");
  const [busy, setBusy] = useState(false);
  const current = deployment.stages.find((s) => s.component === stage);
  const request = useMemo(
    () => ({ name: deployment.name, stages: current ? [{ component: stage, file: current.file, node_ids: [] }] : [] }),
    [deployment.name, current, stage],
  );
  const plan = useQuery({ queryKey: ["image-plan-add", request], queryFn: () => planImage(request), enabled: open && current != null });
  const candidates = (plan.data?.stages[0]?.alternatives ?? []).filter(
    (machine) => machine.ok && machine.path && !current?.replicas.some((r) => r.node_id === machine.node_id),
  );

  async function add() {
    setBusy(true);
    try {
      await addImageReplica(deployment.deploy_id, stage, nodeId);
      toast.success(t("imageMap.added"));
      invalidate();
      onClose();
    } catch (error) {
      toast.error(t("imageMap.addFailed"), error instanceof ApiError ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      open={open}
      title={t("imageMap.addTitle", { name: deployment.name })}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>{t("common.cancel")}</Button>
          <Button onClick={add} disabled={!deployment.recorded || !nodeId || busy} loading={busy}>{t("imageMap.add")}</Button>
        </>
      }
    >
      {!deployment.recorded ? (
        <p className="text-[0.8rem] text-muted">{t("imageMap.notRecorded")}</p>
      ) : (
        <div className="space-y-3">
          <Select aria-label={t("imageMap.stage")} value={stage} onChange={(event) => { setStage(event.target.value as ImageStageName); setNodeId(""); }}>
            {deployment.stages.map((s) => <option key={s.component} value={s.component}>{t(`imageDeploy.component.${s.component}`)}</option>)}
          </Select>
          <Select aria-label={t("imageDeploy.machine")} value={nodeId} onChange={(event) => setNodeId(event.target.value)}>
            <option value="">{t("imageDeploy.chooseMachine")}</option>
            {candidates.map((machine) => <option key={machine.node_id} value={machine.node_id}>{machine.hostname}</option>)}
          </Select>
          {plan.data && candidates.length === 0 && <p className="text-[0.75rem] text-muted">{t("imageDeploy.noHolder")}</p>}
        </div>
      )}
    </Modal>
  );
}

/** Undeploy an image model from every machine that serves it, after naming them. */
export function ImageUndeploy({ deployment }: { deployment: ImageDeployment }) {
  const { t } = useI18n();
  const toast = useToast();
  const invalidate = useInvalidate();
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const machines = machinesOf(deployment);

  async function undeploy() {
    setBusy(true);
    try {
      const answer = await undeployImage(deployment.deploy_id);
      const silent = answer.released.filter((r) => !r.ok);
      if (silent.length) {
        toast.error(t("imageMap.undeployPartial"), silent.map((r) => `${r.node_id.slice(0, 12)}: ${r.error ?? ""}`).join("; "));
      } else {
        toast.success(t("imageDeploy.undeployed", { name: deployment.name }));
      }
      invalidate();
      setAsking(false);
    } catch (error) {
      toast.error(t("imageMap.undeployPartial"), error instanceof ApiError ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <Button size="sm" variant="outline" onClick={() => setAsking(true)}>
        <Power className="h-3.5 w-3.5" />
        {t("imageDeploy.undeploy")}
      </Button>
      <Confirm
        open={asking}
        title={t("imageDeploy.undeployNamed", { name: deployment.name })}
        description={
          <div>
            <p>{t("imageDeploy.undeployConfirm", { name: deployment.name })}</p>
            <ul className="mt-2 list-disc pl-5 font-mono text-[0.75rem]" data-testid="image-undeploy-machines">
              {machines.map((m) => <li key={m.node_id}>{m.hostname}</li>)}
            </ul>
          </div>
        }
        confirmLabel={t("imageDeploy.undeploy")}
        tone="danger"
        busy={busy}
        onConfirm={undeploy}
        onCancel={() => setAsking(false)}
      />
    </>
  );
}

export function ImageAddReplicaButton({ deployment }: { deployment: ImageDeployment }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button size="sm" variant="outline" onClick={() => setOpen(true)}>
        <Plus className="h-3.5 w-3.5" />
        {t("imageMap.addReplica")}
      </Button>
      {open && <ImageAddReplicaDialog deployment={deployment} open={open} onClose={() => setOpen(false)} />}
    </>
  );
}
