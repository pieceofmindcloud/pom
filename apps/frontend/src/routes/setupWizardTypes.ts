export type WizardIntent =
  | "quick"
  | "personal"
  | "organization"
  | "contributor"
  | "inference"
  | "advanced";

export type RoleName = "admin" | "worker" | "visitor";
export type NetworkMode = "local" | "private" | "public";
export type DiscoveryMode = "automatic" | "mdns" | "manual";
export type WizardStage =
  | "setup-profile"
  | "setup-location"
  | "setup-network"
  | "setup-trust"
  | "setup-invite"
  | "setup-discovery"
  | "setup-review"
  | "setup-join"
  | "setup-saved"
  | "welcome"
  | "intent"
  | "hardware"
  | "network"
  | "discovery"
  | "model"
  | "pipeline"
  | "organization"
  | "security"
  | "invite"
  | "users"
  | "api"
  | "resources"
  | "network-browser"
  | "join"
  | "connection"
  | "advanced"
  | "health"
  | "summary";

export interface WizardStep {
  id: WizardStage;
  labelKey: string;
}

export interface WizardHardware {
  cpu: { name: string; cores: number; threads: number };
  gpu: { name: string; vramGb: number; backend: string };
  memoryGb: number;
}

export interface WizardNode {
  id: string;
  name: string;
  address: string;
  gpu: string;
  vramGb: number;
  status: "searching" | "found" | "connecting" | "connected" | "failed";
}

export interface WizardModel {
  id: string;
  name: string;
  sizeGb: number;
  quantization: string;
  memoryGb: number;
  compatibility: "recommended" | "compatible" | "limited";
  downloadProgress: number;
}

export interface WizardNetwork {
  id: string;
  name: string;
  nodes: number;
  vramGb: number;
  reputation: number;
  latencyMs: number;
  models: string[];
}

export interface WizardCapability {
  id: string;
  labelKey: string;
  enabled: boolean;
}

export interface WizardHealthItem {
  id: string;
  labelKey: string;
  state: "pending" | "running" | "passed" | "failed";
}

export interface WizardRoles {
  admin: boolean;
  worker: boolean;
  visitor: boolean;
}

export interface PomSetup {
  intent: WizardIntent | null;
  roles: WizardRoles;
  network: {
    name: string;
    mode: NetworkMode;
    discovery: DiscoveryMode;
  };
  hardware: WizardHardware | null;
  nodes: WizardNode[];
  model: WizardModel | null;
  organization: {
    name: string;
    networkName: string;
  };
  capabilities: string[];
  resources: {
    gpu: number;
    ram: number;
    cpu: number;
    availability: "idle" | "always";
    scheduleEnabled: boolean;
  };
  selectedNetwork: WizardNetwork | null;
  advanced: {
    discovery: DiscoveryMode;
    backend: "auto" | "cuda" | "cpu";
    api: boolean;
    security: "local" | "authorized" | "devices";
  };
}
