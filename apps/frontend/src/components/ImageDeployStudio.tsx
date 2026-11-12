// The Deploy studio's image mode: an image model is a pipeline of three
// stages in data-flow order (text encoder -> diffusion -> VAE). Machines are
// dragged from the strip onto a stage; several machines on one stage are
// replicas, and one machine may serve several stages. The node's plan says
// which machines can take each stage and why the others cannot.
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { Image as ImageIcon, ServerCog, X } from "lucide-react";
import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { ApiError } from "../lib/api";
import { megabytes } from "../lib/format";
import {
  IMAGE_STAGES,
  addLora,
  deployImage,
  dropOnStage,
  emptyChoices,
  planImage,
  planRequest,
  removeFromStage,
  removeLora,
  setLoraStrength,
  type StageChoices,
} from "../lib/imagePipeline";
import { useCluster, useClusterCatalog, useInvalidate, useJobs } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { ImageFileIdentity, ImageModelFile, ImagePlan, ImageStageName } from "../lib/types";
import { DeployProgressModal } from "./DeployProgressModal";
import { Button, Input, Select } from "./ui";

/** A file as the network has it: one name and size, on one or more machines. */
export interface NetworkImageFile {
  key: string;
  identity: ImageFileIdentity;
  folder: string;
  category: string;
}

const baseName = (name: string) => name.split("/").pop() ?? name;
const folderOf = (name: string) => name.split("/").slice(0, -1).join("/");
const fits = (stage: ImageStageName, category: string) => category === stage || category === "checkpoint";

export function networkImageFiles(peers: Array<{ image_files?: unknown[] }>): NetworkImageFile[] {
  const byKey = new Map<string, NetworkImageFile>();
  for (const peer of peers) {
    for (const file of (peer.image_files ?? []) as ImageModelFile[]) {
      if (file.metadata_error) continue;
      const name = baseName(file.name);
      const key = `${name}|${file.size_bytes}`;
      if (!byKey.has(key)) {
        byKey.set(key, { key, identity: { name, size_bytes: file.size_bytes }, folder: folderOf(file.name), category: file.category });
      }
    }
  }
  return [...byKey.values()].sort((a, b) => a.identity.name.localeCompare(b.identity.name));
}

/** The text encoder and VAE of the diffusion file's folder, when it holds exactly one of each. */
export function prefill(files: NetworkImageFile[], diffusion: NetworkImageFile, choices: StageChoices): StageChoices {
  const next = { ...choices, diffusion: { ...choices.diffusion, file: diffusion.identity } };
  for (const stage of ["text_encoder", "vae"] as const) {
    const same = files.filter((f) => f.folder === diffusion.folder && f.category === stage);
    if (same.length === 1) next[stage] = { ...next[stage], file: same[0].identity };
  }
  return next;
}

/** The diffusion stage's LoRAs: add one of the network's LoRA files, set each strength, remove. */
export function DiffusionLoras({
  loras,
  files,
  onChange,
}: {
  loras: StageChoices;
  files: NetworkImageFile[];
  onChange: (next: StageChoices) => void;
}) {
  const { t } = useI18n();
  const chosen = loras.diffusion.loras ?? [];
  const available = files.filter(
    (f) => f.category === "lora" && !chosen.some((l) => l.file.name === f.identity.name && l.file.size_bytes === f.identity.size_bytes),
  );
  if (chosen.length === 0 && available.length === 0) return null;
  return (
    <div className="mb-2 space-y-1.5 rounded-lg border border-line bg-ink/60 p-2" data-testid="diffusion-loras">
      <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-faint">{t("imageStudio.loras")}</p>
      {chosen.map((lora, index) => (
        <div key={`${lora.file.name}|${lora.file.size_bytes}`} className="flex items-center gap-1.5" data-testid={`lora-${lora.file.name}`}>
          <span className="min-w-0 flex-1 truncate font-mono text-[0.64rem] text-fg" title={lora.file.name}>{lora.file.name}</span>
          <Input
            type="number"
            min={-10}
            max={10}
            step={0.05}
            value={String(lora.strength)}
            aria-label={t("imageStudio.loraStrength", { name: lora.file.name })}
            title={t("imageStudio.loraStrength", { name: lora.file.name })}
            onChange={(event) => {
              const value = Number.parseFloat(event.target.value);
              if (Number.isFinite(value)) onChange(setLoraStrength(loras, index, value));
            }}
            className="h-6 !w-[4.5rem] px-1 text-[0.64rem]"
          />
          <button type="button" aria-label={t("imageStudio.removeLora", { name: lora.file.name })} onClick={() => onChange(removeLora(loras, index))} className="rounded p-0.5 text-muted hover:text-fg">
            <X className="h-3 w-3" />
          </button>
        </div>
      ))}
      {available.length > 0 && (
        <Select
          aria-label={t("imageStudio.addLora")}
          value=""
          onChange={(event) => {
            const file = available.find((f) => f.key === event.target.value);
            if (file) onChange(addLora(loras, file.identity));
          }}
          className="h-7 text-[0.66rem]"
        >
          <option value="">{t("imageStudio.addLora")}</option>
          {available.map((f) => (
            <option key={f.key} value={f.key}>{f.identity.name}</option>
          ))}
        </Select>
      )}
    </div>
  );
}

export function ImageStageCanvas({
  plan,
  choices,
  labelOf,
  onDrop,
  onRemove,
  files = [],
  onChoices,
}: {
  plan: ImagePlan | undefined;
  choices: StageChoices;
  labelOf: (nodeId: string) => string;
  onDrop: (stage: ImageStageName, nodeId: string) => void;
  onRemove: (stage: ImageStageName, nodeId: string) => void;
  /** The network's image files, for the diffusion stage's LoRA picker. */
  files?: NetworkImageFile[];
  onChoices?: (next: StageChoices) => void;
}) {
  const { t } = useI18n();
  const [over, setOver] = useState<ImageStageName | null>(null);
  return (
    <div className="grid min-h-0 flex-1 grid-cols-1 gap-3 md:grid-cols-3" data-testid="image-stages">
      {IMAGE_STAGES.map((stage, index) => {
        const stagePlan = plan?.stages.find((s) => s.component === stage);
        const file = choices[stage].file;
        return (
          <div
            key={stage}
            data-testid={`stage-${stage}`}
            onDragOver={(event) => { event.preventDefault(); event.stopPropagation(); setOver(stage); }}
            onDragLeave={() => setOver(null)}
            onDrop={(event) => {
              event.preventDefault();
              event.stopPropagation();
              setOver(null);
              const nodeId = event.dataTransfer.getData("application/x-pom-node");
              if (nodeId) onDrop(stage, nodeId);
            }}
            className={`relative flex min-h-[14rem] flex-col rounded-xl border bg-surface/60 p-3 ${over === stage ? "border-iris" : "border-line"}`}
          >
            <p className="eyebrow">{index + 1}. {t(`imageDeploy.component.${stage}`)}</p>
            <p className="mb-2 truncate font-mono text-[0.68rem] text-muted" title={file?.name}>
              {file ? `${file.name} · ${megabytes(file.size_bytes / (1024 * 1024))}` : t("imageDeploy.chooseFile")}
            </p>
            {stage === "diffusion" && file && onChoices && (
              <DiffusionLoras loras={choices} files={files} onChange={onChoices} />
            )}
            <div className="flex flex-1 flex-col gap-2">
              {choices[stage].nodeIds.map((nodeId) => {
                const verdict = stagePlan?.chosen.find((v) => v.node_id === nodeId);
                const memory = plan?.memory.find((m) => m.node_id === nodeId);
                return (
                  <div key={nodeId} data-testid={`replica-${stage}-${nodeId}`} className={`rounded-lg border bg-ink px-2 py-1.5 ${verdict && !verdict.ok ? "border-rose/60" : "border-line"}`}>
                    <div className="flex items-center gap-1.5">
                      <ServerCog className="h-3.5 w-3.5 shrink-0 text-faint" />
                      <span className="min-w-0 flex-1 truncate text-[0.72rem] text-fg">{labelOf(nodeId)}</span>
                      <button type="button" aria-label={t("imageStudio.remove", { host: labelOf(nodeId) })} onClick={() => onRemove(stage, nodeId)} className="rounded p-0.5 text-muted hover:text-fg">
                        <X className="h-3 w-3" />
                      </button>
                    </div>
                    {memory && (
                      <p className="font-mono text-[0.58rem] text-faint">
                        {t("imageStudio.memory", { gpu: megabytes(memory.gpu_free_mb), ram: megabytes(memory.ram_free_mb), need: megabytes(memory.need_mb) })}
                      </p>
                    )}
                    {verdict?.reason && <p className={`text-[0.6rem] ${verdict.ok ? "text-faint" : "text-rose"}`}>{verdict.reason}</p>}
                  </div>
                );
              })}
              {choices[stage].nodeIds.length === 0 && (
                <p className="m-auto text-center text-[0.68rem] text-faint">{t("imageStudio.dropHere")}</p>
              )}
              {choices[stage].nodeIds.length > 1 && (
                <p className="text-[0.6rem] text-iris">{t("imageStudio.replicas", { count: choices[stage].nodeIds.length })}</p>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

export function ImageDeployStudio({ modeBar }: { modeBar: React.ReactNode }) {
  const { t } = useI18n();
  const toast = useToast();
  const navigate = useNavigate();
  const invalidate = useInvalidate();
  const cluster = useCluster();
  const catalog = useClusterCatalog();
  const jobs = useJobs(true);
  const [name, setName] = useState("");
  const [choices, setChoices] = useState<StageChoices>(emptyChoices);
  const [jobId, setJobId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const files = useMemo(() => networkImageFiles(catalog.data?.peers ?? []), [catalog.data]);
  const machines = useMemo(
    () => (cluster.data?.peers ?? []).map((peer) => ({ node_id: peer.node_id, label: peer.node?.hostname || peer.short_id || peer.node_id.slice(0, 12) })),
    [cluster.data],
  );
  const labelOf = (nodeId: string) => machines.find((m) => m.node_id === nodeId)?.label ?? nodeId.slice(0, 12);
  const request = planRequest(name.trim(), choices);
  const plan = useQuery({
    queryKey: ["image-plan", request],
    queryFn: () => planImage(request),
    enabled: request.stages.length > 0,
    placeholderData: keepPreviousData,
  });
  const verdict = request.stages.length === 0
    ? t("imageStudio.chooseDiffusion")
    : plan.data?.blocked[0] ?? null;
  const canDeploy = !busy && plan.data != null && plan.data.blocked.length === 0
    && choices.diffusion.file != null && plan.data.stages.every((s) => s.chosen.length > 0);
  const job = jobs.data?.jobs.find((j) => j.id === jobId);

  function chooseFile(stage: ImageStageName, key: string) {
    const file = files.find((f) => f.key === key);
    if (!file) {
      setChoices({ ...choices, [stage]: { ...choices[stage], file: null } });
      return;
    }
    if (stage === "diffusion") {
      setChoices(prefill(files, file, choices));
      if (!name.trim()) setName(file.identity.name.replace(/\.[^.]+$/, ""));
    } else {
      setChoices({ ...choices, [stage]: { ...choices[stage], file: file.identity } });
    }
  }

  async function deploy() {
    setBusy(true);
    try {
      const answer = await deployImage(request);
      setJobId(answer.job_id);
      invalidate();
    } catch (error) {
      const message = error instanceof ApiError ? error.message : String(error);
      toast.error(t("imageDeploy.failed"), message);
    } finally {
      setBusy(false);
    }
  }

  const keyOf = (identity: ImageFileIdentity | null) =>
    identity ? `${identity.name}|${identity.size_bytes}` : "";

  return (
    <section className="-mx-1 flex h-full min-h-0 flex-col overflow-hidden rounded-2xl border border-line bg-ink/80">
      <div className="border-b border-line bg-surface/90 px-4 py-3 sm:px-5">
        <div className="flex flex-wrap items-center gap-2">
          {modeBar}
          {IMAGE_STAGES.map((stage) => (
            <Select
              key={stage}
              aria-label={t(`imageDeploy.component.${stage}`)}
              value={keyOf(choices[stage].file)}
              onChange={(event) => chooseFile(stage, event.target.value)}
              className="h-9 !w-[12rem] text-[0.78rem]"
            >
              <option value="">{t(`imageDeploy.component.${stage}`)}</option>
              {files.filter((f) => fits(stage, f.category)).map((f) => (
                <option key={f.key} value={f.key}>{f.identity.name}</option>
              ))}
            </Select>
          ))}
          <Input aria-label={t("imageDeploy.name")} placeholder={t("imageDeploy.name")} value={name} onChange={(event) => setName(event.target.value)} className="h-9 !w-[11rem] text-[0.78rem]" />
          <Button onClick={deploy} disabled={!canDeploy} loading={busy}>{t("imageDeploy.deploy")}</Button>
        </div>
        {verdict && <p className="mt-2 text-[0.72rem] text-rose" role="status" data-testid="image-verdict">{verdict}</p>}
        {plan.data && plan.data.copies.length > 0 && (
          <p className="mt-1 text-[0.68rem] text-muted">{t("imageStudio.copiesUnsupported")}</p>
        )}
      </div>

      <DeployProgressModal
        open={jobId != null}
        job={job}
        onClose={() => setJobId(null)}
        onDashboard={() => { setJobId(null); navigate("/"); }}
      />

      <div className="flex min-h-0 flex-1 flex-col p-3 sm:p-4">
        <div className="mb-2 flex items-center gap-2 text-[0.7rem] text-muted">
          <ImageIcon className="h-3.5 w-3.5" />
          {t("imageStudio.hint")}
        </div>
        <ImageStageCanvas
          plan={plan.data}
          choices={choices}
          labelOf={labelOf}
          onDrop={(stage, nodeId) => setChoices((current) => dropOnStage(current, stage, nodeId))}
          onRemove={(stage, nodeId) => setChoices((current) => removeFromStage(current, stage, nodeId))}
          files={files}
          onChoices={setChoices}
        />
      </div>

      <div className="shrink-0 border-t border-line bg-surface/75 px-3 py-1.5 sm:px-4">
        <div className="flex min-w-0 items-center gap-1.5 overflow-x-auto">
          <span className="mr-1 shrink-0 text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-faint">Peers</span>
          {machines.map((machine) => {
            const reasons = (plan.data?.stages ?? [])
              .map((s) => s.alternatives.find((a) => a.node_id === machine.node_id))
              .filter((a) => a && !a.ok)
              .map((a) => a?.reason);
            return (
              <button
                key={machine.node_id}
                type="button"
                data-testid={`strip-${machine.node_id}`}
                draggable
                title={reasons.length ? reasons.join(" · ") : undefined}
                onDragStart={(event) => event.dataTransfer.setData("application/x-pom-node", machine.node_id)}
                className="flex min-w-[8rem] shrink-0 items-center gap-1.5 rounded-md border border-line bg-ink px-2 py-1 text-left hover:border-iris/60"
              >
                <ServerCog className="h-3 w-3 shrink-0 text-faint" />
                <span className="truncate text-[0.62rem] text-fg">{machine.label}</span>
              </button>
            );
          })}
        </div>
      </div>
    </section>
  );
}
