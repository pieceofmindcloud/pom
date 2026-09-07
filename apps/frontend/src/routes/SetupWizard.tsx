import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { WizardLayout } from "../components/setup-wizard/WizardLayout";
import {
  AdvancedScreen,
  ApiScreen,
  ConnectionScreen,
  DiscoveryScreen,
  HardwareScreen,
  HealthScreen,
  InviteScreen,
  JoinScreen,
  ModelScreen,
  NetworkBrowserScreen,
  NetworkScreen,
  ObjectiveScreen,
  OrganizationScreen,
  PipelineScreen,
  ResourcesScreen,
  SecurityScreen,
  SummaryScreen,
  UsersScreen,
  WelcomeScreen,
} from "../components/setup-wizard/WizardScreens";
import { INITIAL_SETUP, MOCK_CAPABILITIES } from "./setupWizardMocks";
import type { PomSetup, WizardIntent, WizardStage, WizardStep } from "./setupWizardTypes";

const FLOW_STEPS: Record<WizardIntent, WizardStep[]> = {
  quick: [
    { id: "hardware", labelKey: "wizard.stepHardware" },
    { id: "network", labelKey: "wizard.stepNetwork" },
    { id: "model", labelKey: "wizard.stepModel" },
    { id: "pipeline", labelKey: "wizard.stepPipeline" },
    { id: "health", labelKey: "wizard.stepHealth" },
    { id: "summary", labelKey: "wizard.stepSummary" },
  ],
  personal: [
    { id: "hardware", labelKey: "wizard.stepHardware" },
    { id: "network", labelKey: "wizard.stepNetwork" },
    { id: "discovery", labelKey: "wizard.stepComputers" },
    { id: "model", labelKey: "wizard.stepModel" },
    { id: "pipeline", labelKey: "wizard.stepPipeline" },
    { id: "health", labelKey: "wizard.stepHealth" },
    { id: "summary", labelKey: "wizard.stepSummary" },
  ],
  organization: [
    { id: "organization", labelKey: "wizard.stepOrganization" },
    { id: "security", labelKey: "wizard.stepSecurity" },
    { id: "invite", labelKey: "wizard.stepInvite" },
    { id: "hardware", labelKey: "wizard.stepHardware" },
    { id: "model", labelKey: "wizard.stepModel" },
    { id: "users", labelKey: "wizard.stepUsers" },
    { id: "api", labelKey: "wizard.stepApi" },
    { id: "health", labelKey: "wizard.stepHealth" },
    { id: "summary", labelKey: "wizard.stepSummary" },
  ],
  contributor: [
    { id: "resources", labelKey: "wizard.stepResources" },
    { id: "network", labelKey: "wizard.stepNetwork" },
    { id: "network-browser", labelKey: "wizard.stepNetworkBrowser" },
    { id: "summary", labelKey: "wizard.stepSummary" },
  ],
  inference: [
    { id: "join", labelKey: "wizard.stepJoin" },
    { id: "connection", labelKey: "wizard.stepConnection" },
    { id: "resources", labelKey: "wizard.stepResources" },
    { id: "summary", labelKey: "wizard.stepSummary" },
  ],
  advanced: [
    { id: "advanced", labelKey: "wizard.stepAdvanced" },
    { id: "summary", labelKey: "wizard.stepSummary" },
  ],
};

const ROLE_PRESETS: Record<WizardIntent, PomSetup["roles"]> = {
  quick: { admin: true, node: true, infer: true },
  personal: { admin: true, node: true, infer: true },
  organization: { admin: true, node: false, infer: false },
  contributor: { admin: false, node: true, infer: false },
  inference: { admin: false, node: false, infer: true },
  advanced: { admin: true, node: true, infer: true },
};

export function SetupWizard() {
  const navigate = useNavigate();
  const [setup, setSetup] = useState<PomSetup>(INITIAL_SETUP);
  const [history, setHistory] = useState<WizardStage[]>(["welcome"]);
  const stage = history[history.length - 1];
  const steps = useMemo(() => setup.intent ? FLOW_STEPS[setup.intent] : [{ id: stage, labelKey: stage === "intent" ? "wizard.stepObjective" : "wizard.welcome" }], [setup.intent, stage]);

  function updateSetup(patch: Partial<PomSetup>) {
    setSetup((current) => ({ ...current, ...patch }));
  }

  function startIntent(intent: WizardIntent) {
    const firstStage = FLOW_STEPS[intent][0].id;
    setSetup({ ...INITIAL_SETUP, intent, roles: ROLE_PRESETS[intent], capabilities: MOCK_CAPABILITIES });
    setHistory([firstStage]);
  }

  function openObjective() {
    setHistory((current) => [...current, "intent"]);
  }

  function goNext() {
    if (!setup.intent) return;
    const flow = FLOW_STEPS[setup.intent];
    const currentIndex = flow.findIndex((step) => step.id === stage);
    const next = flow[currentIndex + 1];
    if (next) setHistory((current) => [...current, next.id]);
  }

  function goBack() {
    setHistory((current) => current.length > 1 ? current.slice(0, -1) : current);
  }

  function screen() {
    switch (stage) {
      case "welcome": return <WelcomeScreen onIntent={startIntent} onObjective={openObjective} />;
      case "intent": return <ObjectiveScreen onIntent={startIntent} />;
      case "hardware": return <HardwareScreen onContinue={goNext} />;
      case "network": return <NetworkScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "discovery": return <DiscoveryScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "model": return <ModelScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "pipeline": return <PipelineScreen setup={setup} onContinue={goNext} />;
      case "health": return <HealthScreen onContinue={goNext} />;
      case "organization": return <OrganizationScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "security": return <SecurityScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "invite": return <InviteScreen onContinue={goNext} />;
      case "users": return <UsersScreen onContinue={goNext} />;
      case "api": return <ApiScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "resources": return <ResourcesScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "network-browser": return <NetworkBrowserScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "join": return <JoinScreen onContinue={goNext} />;
      case "connection": return <ConnectionScreen onContinue={goNext} />;
      case "advanced": return <AdvancedScreen setup={setup} onUpdate={updateSetup} onContinue={goNext} />;
      case "summary": return <SummaryScreen setup={setup} onExit={() => navigate("/")} />;
    }
  }

  return <WizardLayout stage={stage} steps={steps} canGoBack={history.length > 1} onBack={goBack} onExit={() => navigate("/")}>{screen()}</WizardLayout>;
}

export { FLOW_STEPS };
