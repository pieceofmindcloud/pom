import type { WizardStage, WizardStep } from "./setupWizardTypes";

export type SetupProfile = "admin" | "node" | "infer";

/**
 * O primeiro acesso pergunta só o que não dá para decidir depois:
 *
 * - **admin** cria a rede: início (papel e nome da máquina), a rede (nome e se
 *   é privada ou pública; a cidade só quando é pública, para o pkarr) e o
 *   convite, com a descoberta local opcional na mesma tela;
 * - **node** e **infer** entram numa rede: início e entrada (descoberta
 *   automática, ou o `trust.json` e a chave colados à mão).
 *
 * Recursos, pastas, disco, senha e exposição têm padrões e são ajustados
 * depois, dentro do POM.
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
