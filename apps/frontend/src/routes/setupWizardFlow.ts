import type { WizardStage, WizardStep } from "./setupWizardTypes";

export type SetupProfile = "admin" | "node" | "infer";

const PROFILE_STEPS: WizardStep[] = [{ id: "setup-profile", labelKey: "setupWizard.stepHardware" }];

/**
 * The first-access path is deliberately role-shaped:
 *
 * - infer and node import their network identity from the trust document;
 * - admin configures the network and creates the invitation.
 *
 * Keeping this decision in one pure function makes it harder for a screen to
 * accidentally leak a model, folder, or resource question into another role.
 */
export function setupStepsFor(profile: SetupProfile | null): WizardStep[] {
  if (!profile) return PROFILE_STEPS;
  const steps: WizardStep[] = [
    { id: "setup-profile", labelKey: "setupWizard.stepHardware" },
  ];
  if (profile === "admin") {
    steps.push({ id: "setup-location", labelKey: "setupWizard.stepLocation" });
  }
  if (profile === "infer") {
    steps.push({ id: "setup-trust", labelKey: "setupWizard.stepTrust" });
  }
  if (profile === "admin") {
    steps.push(
      { id: "setup-network", labelKey: "setupWizard.stepNetwork" },
      { id: "setup-invite", labelKey: "setupWizard.stepInvite" },
    );
  } else if (profile === "node") {
    steps.push({ id: "setup-trust", labelKey: "setupWizard.stepTrust" });
  }
  steps.push({ id: "setup-review", labelKey: "setupWizard.stepReview" });
  return steps;
}

export function setupProfileHasResources(profile: SetupProfile | null): boolean {
  return profile === "admin" || profile === "node";
}

export function setupProfileHasModelAndFolders(_profile: SetupProfile | null): boolean {
  return false;
}

export function setupProfileHasShardSlider(profile: SetupProfile | null): boolean {
  return profile === "admin" || profile === "node";
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
