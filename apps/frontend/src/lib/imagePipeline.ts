// The image pipeline routes and the small rules the screens share: stage
// order, the plan request built from what was dropped on each stage.
import { api } from "./api";
import type {
  ImageDeployment,
  ImageFileIdentity,
  ImageLoraChoice,
  ImagePlan,
  ImagePlanRequest,
  ImageStageName,
} from "./types";

/** The order the data flows through an image model. */
export const IMAGE_STAGES: ImageStageName[] = ["text_encoder", "diffusion", "vae"];

export interface StageChoice {
  file: ImageFileIdentity | null;
  nodeIds: string[];
  /** The diffusion stage's LoRAs, in the order they apply. */
  loras?: ImageLoraChoice[];
}

/** Adds a LoRA to the diffusion stage at full strength; one already there stays once. */
export function addLora(choices: StageChoices, file: ImageFileIdentity): StageChoices {
  const loras = choices.diffusion.loras ?? [];
  if (loras.some((l) => l.file.name === file.name && l.file.size_bytes === file.size_bytes)) return choices;
  return { ...choices, diffusion: { ...choices.diffusion, loras: [...loras, { file, strength: 1 }] } };
}

export function setLoraStrength(choices: StageChoices, index: number, strength: number): StageChoices {
  const loras = (choices.diffusion.loras ?? []).map((l, i) => (i === index ? { ...l, strength } : l));
  return { ...choices, diffusion: { ...choices.diffusion, loras } };
}

export function removeLora(choices: StageChoices, index: number): StageChoices {
  const loras = (choices.diffusion.loras ?? []).filter((_, i) => i !== index);
  return { ...choices, diffusion: { ...choices.diffusion, loras } };
}

export type StageChoices = Record<ImageStageName, StageChoice>;

export function emptyChoices(): StageChoices {
  return {
    text_encoder: { file: null, nodeIds: [] },
    diffusion: { file: null, nodeIds: [] },
    vae: { file: null, nodeIds: [] },
  };
}

/** Adds a machine to a stage (a replica); a machine already there stays once. */
export function dropOnStage(choices: StageChoices, stage: ImageStageName, nodeId: string): StageChoices {
  const current = choices[stage];
  if (current.nodeIds.includes(nodeId)) return choices;
  return { ...choices, [stage]: { ...current, nodeIds: [...current.nodeIds, nodeId] } };
}

export function removeFromStage(choices: StageChoices, stage: ImageStageName, nodeId: string): StageChoices {
  const current = choices[stage];
  return { ...choices, [stage]: { ...current, nodeIds: current.nodeIds.filter((id) => id !== nodeId) } };
}

/** The plan request: the stages that have a file, in flow order. */
export function planRequest(name: string, choices: StageChoices, allowTransfers = false): ImagePlanRequest {
  return {
    name,
    stages: IMAGE_STAGES.filter((stage) => choices[stage].file).map((stage) => ({
      component: stage,
      file: choices[stage].file as ImageFileIdentity,
      node_ids: choices[stage].nodeIds,
      ...(stage === "diffusion" && (choices[stage].loras?.length ?? 0) > 0 ? { loras: choices[stage].loras } : {}),
    })),
    ...(allowTransfers ? { allow_transfers: true } : {}),
  };
}

export const planImage = (request: ImagePlanRequest) =>
  api.post<ImagePlan>("/api/ui/image-deploy/plan", request);

export const deployImage = (request: ImagePlanRequest) =>
  api.post<{ deploy_id: string; job_id: string }>("/api/ui/image-deploy", request);

export const addImageReplica = (deployId: string, component: ImageStageName, nodeId: string) =>
  api.post<{ version: number }>(`/api/ui/image-deploy/${deployId}/add`, { component, node_id: nodeId });

export const removeImageReplica = (deployId: string, component: ImageStageName, nodeId: string, confirmGap = false) =>
  api.post<{ version: number; serving: boolean }>(`/api/ui/image-deploy/${deployId}/remove`, {
    component,
    node_id: nodeId,
    confirm_gap: confirmGap,
  });

export const redeployImage = (deployId: string, version: number) =>
  api.post<{ version: number }>(`/api/ui/image-deploy/${deployId}/redeploy`, { version });

export const undeployImage = (deployId: string) =>
  api.post<{ released: Array<{ node_id: string; ok: boolean; error?: string }> }>("/api/ui/image-undeploy", {
    deploy_id: deployId,
  });

/** The Dashboard's id for an image deployment, beside the LLM model names. */
export const imageModelId = (deployId: string) => `image:${deployId}`;
export const deployIdOfImageModel = (id: string | null | undefined) =>
  id?.startsWith("image:") ? id.slice("image:".length) : null;

/** Every machine of a deployment, each once, in stage order. */
export function machinesOf(deployment: ImageDeployment): Array<{ node_id: string; hostname: string }> {
  const seen = new Map<string, string>();
  for (const stage of deployment.stages) {
    for (const replica of stage.replicas) {
      if (!seen.has(replica.node_id)) seen.set(replica.node_id, replica.hostname);
    }
  }
  return [...seen].map(([node_id, hostname]) => ({ node_id, hostname }));
}
