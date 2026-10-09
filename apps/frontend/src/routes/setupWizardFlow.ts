import type { WizardStage, WizardStep } from "./setupWizardTypes";

export type SetupProfile = "admin" | "node" | "infer";

/**
 * First access asks only what cannot be decided later:
 *
 * - **admin** creates the network: start (role and machine name), the network
 *   (name and whether it is private or public; the city only when it is public,
 *   for pkarr) and the invite, with optional local discovery on the same screen;
 * - **node** and **infer** join a network: start and entry (automatic
 *   discovery, or the `trust.json` and the key pasted by hand).
 *
 * Resources, folders, disk, password and exposure have defaults and are adjusted
 * later, inside POM.
 */
export function setupStepsFor(profile: SetupProfile | null): WizardStep[] {
  const start: WizardStep = { id: "setup-profile", labelKey: "firstRun.stepStart" };
  if (profile === "admin") {
    return [
      start,
      { id: "setup-network", labelKey: "firstRun.stepNetwork" },
      { id: "setup-invite", labelKey: "firstRun.stepInvite" },
    ];
  }
  if (profile === "node" || profile === "infer") {
    return [start, { id: "setup-join", labelKey: "firstRun.stepJoin" }];
  }
  return [start];
}

export function setupProfileUsesOnlyTrust(profile: SetupProfile | null): boolean {
  return profile === "node" || profile === "infer";
}

export function setupNextStage(stage: WizardStage, steps: WizardStep[]): WizardStage | null {
  const index = steps.findIndex((step) => step.id === stage);
  return steps[index + 1]?.id ?? null;
}

export function setupPreviousStage(stage: WizardStage, steps: WizardStep[]): WizardStage | null {
  const index = steps.findIndex((step) => step.id === stage);
  return steps[index - 1]?.id ?? null;
}
