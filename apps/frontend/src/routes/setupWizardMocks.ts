import type {
  PomSetup,
  WizardCapability,
  WizardHealthItem,
  WizardHardware,
  WizardModel,
  WizardNetwork,
  WizardNode,
} from "./setupWizardTypes";

export const MOCK_HARDWARE: WizardHardware = {
  cpu: { name: "AMD Ryzen 7 5700X", cores: 8, threads: 16 },
  gpu: { name: "NVIDIA RTX 5060 Ti", vramGb: 16, backend: "CUDA" },
  memoryGb: 64,
};

export const MOCK_NODES: WizardNode[] = [
  {
    id: "desktop",
    name: "POM-DESKTOP",
    address: "192.168.1.31",
    gpu: "RTX 5060 Ti",
    vramGb: 16,
    status: "found",
  },
  {
    id: "server",
    name: "POM-SERVER",
    address: "192.168.1.40",
    gpu: "RTX 3060",
    vramGb: 12,
    status: "found",
  },
];

export const MOCK_MODELS: WizardModel[] = [
  {
    id: "qwen",
    name: "Qwen",
    sizeGb: 19.8,
    quantization: "Q5_K_M",
    memoryGb: 22,
    compatibility: "recommended",
    downloadProgress: 0,
  },
  {
    id: "gpt-oss",
    name: "GPT-OSS",
    sizeGb: 12.4,
    quantization: "Q4_K_M",
    memoryGb: 15,
    compatibility: "compatible",
    downloadProgress: 0,
  },
  {
    id: "gemma",
    name: "Gemma",
    sizeGb: 8.7,
    quantization: "Q5_K_S",
    memoryGb: 11,
    compatibility: "compatible",
    downloadProgress: 0,
  },
];

export const MOCK_PUBLIC_NETWORKS: WizardNetwork[] = [
  {
    id: "brasil",
    name: "POM Community Brazil",
    nodes: 47,
    vramGb: 182,
    reputation: 4.8,
    latencyMs: 21,
    models: ["Qwen", "GPT-OSS", "Gemma"],
  },
  {
    id: "lab",
    name: "Open Compute Lab",
    nodes: 24,
    vramGb: 96,
    reputation: 4.6,
    latencyMs: 34,
    models: ["Qwen", "Gemma"],
  },
  {
    id: "latam",
    name: "LatAm Inference Mesh",
    nodes: 68,
    vramGb: 244,
    reputation: 4.7,
    latencyMs: 48,
    models: ["GPT-OSS", "Gemma"],
  },
];

export const MOCK_CAPABILITIES: string[] = [
  "inference.chat",
  "inference.api",
  "apikey.create",
];

export const MOCK_CAPABILITY_OPTIONS: WizardCapability[] = [
  { id: "inference.chat", labelKey: "wizard.capabilityChat", enabled: true },
  { id: "inference.api", labelKey: "wizard.capabilityApi", enabled: true },
  { id: "apikey.create", labelKey: "wizard.capabilityKeys", enabled: true },
];

export const MOCK_HEALTH: WizardHealthItem[] = [
  { id: "cpu", labelKey: "wizard.healthCpu", state: "pending" },
  { id: "gpu", labelKey: "wizard.healthGpu", state: "pending" },
  { id: "cuda", labelKey: "wizard.healthCuda", state: "pending" },
  { id: "backend", labelKey: "wizard.healthBackend", state: "pending" },
  { id: "model", labelKey: "wizard.healthModel", state: "pending" },
  { id: "discovery", labelKey: "wizard.healthDiscovery", state: "pending" },
  { id: "node", labelKey: "wizard.healthNode", state: "pending" },
  { id: "pipeline", labelKey: "wizard.healthPipeline", state: "pending" },
  { id: "inference", labelKey: "wizard.healthInference", state: "pending" },
];

export const MOCK_TEST_INFERENCE = {
  prompt: "Olá",
  result: "Olá! Como posso ajudar?",
  ttft: "0.8s",
  generation: "21.7 tok/s",
  latency: "42 ms",
};

export const INITIAL_SETUP: PomSetup = {
  intent: null,
  roles: { admin: false, worker: false, visitor: false },
  network: { name: "Minha rede", mode: "local", discovery: "automatic" },
  hardware: null,
  nodes: [],
  model: null,
  organization: { name: "", networkName: "" },
  capabilities: MOCK_CAPABILITIES,
  resources: {
    gpu: 80,
    ram: 50,
    cpu: 50,
    availability: "idle",
    scheduleEnabled: false,
  },
  selectedNetwork: null,
  advanced: { discovery: "automatic", backend: "auto", api: true, security: "local" },
};
