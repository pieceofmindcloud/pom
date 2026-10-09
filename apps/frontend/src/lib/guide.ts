import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./api";

/**
 * The first run guide: the node keeps which steps a newly created local
 * network has been walked through, so the hints follow the machine and not
 * the browser (`webui/guide.rs`).
 */
export type GuideStepId =
  | "names"
  | "bootstrap"
  | "download_model"
  | "deploy_model"
  | "chat"
  | "join";

/** The small model the guide has a new machine download, deploy and talk to. */
export const GUIDE_MODEL_REPO = "Qwen/Qwen2.5-0.5B-Instruct-GGUF";

export interface GuideState {
  /** False once the guide ended or was hidden; it never comes back. */
  active: boolean;
  steps: GuideStepId[];
  done: GuideStepId[];
}

/**
 * The steps the first run wizard takes, on a screen of its own before the
 * panel opens. The hints only cover what comes after them.
 */
export const WIZARD_STEPS = ["names", "bootstrap"] as const;
export type WizardStepId = (typeof WIZARD_STEPS)[number];

export function isWizardStep(step: GuideStepId | null): step is WizardStepId {
  return step !== null && (WIZARD_STEPS as readonly string[]).includes(step);
}

export const guideKey = ["first-run-guide"] as const;

export function useGuide(enabled = true) {
  return useQuery({
    queryKey: guideKey,
    queryFn: () => api.get<GuideState>("/api/ui/guide"),
    enabled,
    staleTime: 30_000,
  });
}

/** The step the operator is on, or null when there is nothing to show. */
export function currentGuideStep(state: GuideState | undefined): GuideStepId | null {
  if (!state?.active) return null;
  return state.steps.find((step) => !state.done.includes(step)) ?? null;
}

/**
 * Reports a step as done (or skipped), or hides the guide. A screen calls
 * `complete` after its own save succeeded; it is a no-op once the guide ended,
 * and a failure never disturbs the save that triggered it.
 */
export function useGuideActions() {
  const client = useQueryClient();
  const send = async (body: { done?: GuideStepId; dismiss?: boolean }) => {
    const known = client.getQueryData<GuideState>(guideKey);
    if (known && !known.active) return;
    try {
      client.setQueryData(guideKey, await api.post<GuideState>("/api/ui/guide", body));
    } catch {
      void client.invalidateQueries({ queryKey: guideKey });
    }
  };
  return {
    complete: (step: GuideStepId) => send({ done: step }),
    dismiss: () => send({ dismiss: true }),
  };
}
