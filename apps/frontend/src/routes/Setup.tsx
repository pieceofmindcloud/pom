/**
 * Primeiro acesso real.
 *
 * Esta tela usa o mesmo layout do wizard de produto, mas não usa os mocks de
 * `SetupWizard.tsx`: cada leitura e cada gravação conversa com `/api/setup/*`.
 * O percurso começa pela máquina, porque essa é a primeira resposta útil para
 * quem ainda não sabe o que o POM pode compartilhar.
 */
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Check, Copy, Cpu, Database, Landmark, Monitor, Server, ShieldCheck, Sparkles } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { WizardLayout, ContinueButton } from "../components/setup-wizard/WizardLayout";
import type { WizardStage } from "./setupWizardTypes";
import {
  setupNextStage,
  setupPreviousStage,
  setupStepsFor,
  type SetupProfile,
} from "./setupWizardFlow";
import { api } from "../lib/api";
import { useToast } from "../lib/toast";
import { LanguageSelect, useI18n } from "../i18n/provider";
import { Badge, Button, Field, Input, Panel, PanelHeader, Select } from "../components/ui";
import { fingerprintFromTrustDocument, groupFingerprint } from "../lib/trustFingerprint";

type NetworkVisibility = "local" | "private" | "public";
type AccessPolicy = "authorized" | "anyone_with_anchor" | "local_only";
type AdminExposure = "localhost" | "network";

interface GpuStat {
  name: string;
  vendor: string;
  vram_total_mb?: number;
}

interface HardwareResponse {
  system: {
    hostname: string;
    os: string;
    arch: string;
    cpu_count: number;
    ram: { total_mb: number; available_mb: number };
    gpu_kind: string;
    gpus: GpuStat[];
    disk: { total_mb: number; free_mb: number };
  };
  gpu_detected: boolean;
  cuda_available: boolean;
  vulkan_available: boolean;
  backend: string;
  p2p_discovery_available: boolean;
  public_ipv6_available: boolean;
  suggested_advertise_addr?: string | null;
  thermal_limit_c: number;
  default_model_dir: string;
  default_shard_dir: string;
  shard_max_gb: number;
}

interface SetupModel {
  id: string;
  name: string;
  source: "local" | "huggingface";
  repo_id?: string;
  path?: string;
  size_gb?: number;
  quantization?: string;
  memory_gb?: number;
  installed: boolean;
  recommended: boolean;
}

interface ModelsResponse {
  models: SetupModel[];
  model_dirs: string[];
  default_model_dir: string;
  default_shard_dir: string;
}

interface AnchorPreview {
  cluster_id: string;
  document: string;
  fingerprint: string;
  public_key: string;
  trust_filename: string;
  private_connection_key?: string;
  admin_access_token?: string;
}

interface HealthResponse {
  gpu_detected: boolean;
  cuda_working: boolean;
  vulkan_working: boolean;
  configuration_ready: boolean;
  p2p_discovery_working: boolean;
  public_ipv6_enabled: boolean;
}

interface SetupState {
  profile: SetupProfile | null;
  bootstrap: string;
  connectionKey: string;
  advertiseAddr: string;
  trustDocument: string;
  trustFingerprint: string;
  trustConfirmed: boolean;
  adminAccessToken: string;
  adminExposure: AdminExposure;
  tlsRequired: boolean;
  network: {
    public_name: string;
    organization: string;
    description: string;
    rules: string;
    channels: string;
    visibility: NetworkVisibility;
    access_policy: AccessPolicy;
    location: { latitude: number; longitude: number; accuracy_m?: number } | null;
  };
  ramReservePct: number;
  cpuReservePct: number;
  modelDirs: string[];
  shardDir: string;
  shardMaxGb: number;
  thermalLimitC: number;
  setupModel: string;
}

const INITIAL: SetupState = {
  profile: null,
  bootstrap: "",
  connectionKey: "",
  advertiseAddr: "",
  trustDocument: "",
  trustFingerprint: "",
  trustConfirmed: false,
  network: {
    public_name: "",
    organization: "",
    description: "",
    rules: "",
    channels: "",
    visibility: "private",
    access_policy: "authorized",
    location: null,
  },
  ramReservePct: 20,
  cpuReservePct: 20,
  modelDirs: [],
  shardDir: "",
  shardMaxGb: 20,
  thermalLimitC: 85,
  setupModel: "",
  adminAccessToken: "",
  adminExposure: "localhost",
  tlsRequired: true,
};

export function Setup() {
  const { t } = useI18n();
  const navigate = useNavigate();
  const toast = useToast();
  const [setup, setSetup] = useState<SetupState>(INITIAL);
  const [stage, setStage] = useState<WizardStage>("setup-profile");
  const [hardware, setHardware] = useState<HardwareResponse | null>(null);
  const [models, setModels] = useState<ModelsResponse | null>(null);
  const [anchor, setAnchor] = useState<AnchorPreview | null>(null);
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const steps = useMemo(() => setupStepsFor(setup.profile), [setup.profile]);

  useEffect(() => {
    let cancelled = false;
    Promise.all([
      api.get<HardwareResponse>("/api/setup/hardware", { auth: false }),
      api.get<ModelsResponse>("/api/setup/models", { auth: false }),
    ])
      .then(([detected, available]) => {
        if (cancelled) return;
        setHardware(detected);
        setModels(available);
        setSetup((current) => ({
          ...current,
          // Existing libraries are still scanned and shown as local choices,
          // but new files belong in POM's stable first-access folder.
          modelDirs: [available.default_model_dir],
          shardDir: available.default_shard_dir,
          shardMaxGb: detected.shard_max_gb || 20,
          thermalLimitC: detected.thermal_limit_c || 85,
          advertiseAddr: current.advertiseAddr || detected.suggested_advertise_addr || "",
        }));
      })
      .catch((error) => {
        if (!cancelled) toast.fromError(t("setupWizard.loadFailed"), error);
      })
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
  }, [t, toast]);

  function update(patch: Partial<SetupState>) {
    setSetup((current) => ({ ...current, ...patch }));
  }

  function updateNetwork(patch: Partial<SetupState["network"]>) {
    setSetup((current) => ({ ...current, network: { ...current.network, ...patch } }));
  }

  async function prepareAnchor() {
    if (setup.profile !== "admin") return;
    const clusterId = setup.network.public_name.trim();
    if (!clusterId) return;
    setBusy(true);
    try {
      const [preview, token] = await Promise.all([
        api.post<AnchorPreview>("/api/setup/admin-anchor/preview", { cluster_id: clusterId }, { auth: false }),
        api.post<{ token: string }>("/api/setup/admin-token", {}, { auth: false }),
      ]);
      setAnchor(preview);
      update({
        trustDocument: preview.document,
        trustFingerprint: preview.fingerprint,
        trustConfirmed: true,
        adminAccessToken: token.token,
        connectionKey: setup.network.visibility === "public" ? "" : preview.private_connection_key ?? "",
      });
      setStage("setup-security");
    } catch (error) {
      toast.fromError(t("setupWizard.anchorFailed"), error);
    } finally {
      setBusy(false);
    }
  }

  async function runHealth() {
    setBusy(true);
    try {
      const result = await api.post<HealthResponse>("/api/setup/health", { configuration_ready: isReadyToSave(setup) }, { auth: false });
      setHealth(result);
      setStage("setup-health");
    } catch (error) {
      toast.fromError(t("setupWizard.healthFailed"), error);
    } finally {
      setBusy(false);
    }
  }

  async function save() {
    setBusy(true);
    try {
      const selectedLocalModel = models?.models.find(
        (model) => model.id === setup.setupModel && model.source === "local" && model.path,
      );
      await api.post("/api/setup/save", {
        profile: setup.profile,
        bootstrap: setup.bootstrap.trim(),
        connection_key: setup.connectionKey.trim(),
        advertise_addr: setup.advertiseAddr.trim(),
        cluster_id: clusterIdFromDocument(setup.trustDocument) || setup.network.public_name.trim(),
        trust_document: setup.trustDocument.trim(),
        trust_fingerprint: setup.trustFingerprint.trim(),
        network: setup.network,
        resources: { ram_reserve_pct: setup.ramReservePct, cpu_reserve_pct: setup.cpuReservePct },
        thermal_limit_c: setup.thermalLimitC,
        admin_exposure: setup.adminExposure,
        admin_access_token: setup.adminAccessToken,
        tls_required: setup.tlsRequired,
        model_dirs: modelDirsForSave(setup.modelDirs, selectedLocalModel?.path),
        shard_dir: setup.shardDir.trim(),
        shard_max_gb: setup.shardMaxGb,
        setup_model: setup.setupModel || null,
      }, { auth: false });
      setStage("setup-saved");
      window.setTimeout(() => window.location.reload(), 4200);
    } catch (error) {
      toast.fromError(t("setupWizard.saveFailed"), error);
      setBusy(false);
    }
  }

  function continueFrom(stageNow: WizardStage) {
    if (stageNow === "setup-profile" && !setup.profile) return;
    if (stageNow === "setup-network") {
      void prepareAnchor();
      return;
    }
    if (stageNow === "setup-invite" || stageNow === "setup-trust") {
      void runHealth();
      return;
    }
    if (stageNow === "setup-health") {
      setStage("setup-review");
      return;
    }
    const next = setupNextStage(stageNow, steps);
    if (next) setStage(next);
  }

  function back() {
    const previous = setupPreviousStage(stage, steps);
    if (previous) setStage(previous);
  }

  if (stage === "setup-saved") return <SavedScreen />;

  return (
    <div className="relative h-full min-h-0">
      <div className="absolute right-4 top-3 z-10 sm:right-6 sm:top-4"><LanguageSelect /></div>
        <WizardLayout
        stage={stage}
        steps={steps}
        canGoBack={stage !== "setup-profile" && !busy}
        onBack={back}
        onExit={() => navigate("/")}
        footer={
          stage === "setup-review" ? (
            <Button variant="primary" loading={busy} disabled={!isReadyToSave(setup)} onClick={() => void save()} icon={<ShieldCheck className="h-4 w-4" />}>
              {t("setupWizard.finish")}
            </Button>
          ) : stage === "setup-profile" ? (
            <Button variant="primary" disabled={!setup.profile} onClick={() => continueFrom(stage)}>
              {t("wizard.continue")}
            </Button>
          ) : (
            <ContinueButton
              onClick={() => continueFrom(stage)}
              disabled={busy || !canContinue(stage, setup)}
              loading={busy}
            />
          )
        }
      >
        {loading ? <LoadingScreen /> : <Screen stage={stage} setup={setup} hardware={hardware} models={models} anchor={anchor} health={health} update={update} updateNetwork={updateNetwork} setStage={setStage} t={t} />}
      </WizardLayout>
    </div>
  );
}

function Screen({
  stage,
  setup,
  hardware,
  models,
  anchor,
  health,
  update,
  updateNetwork,
  setStage,
  t,
}: {
  stage: WizardStage;
  setup: SetupState;
  hardware: HardwareResponse | null;
  models: ModelsResponse | null;
  anchor: AnchorPreview | null;
  health: HealthResponse | null;
  update: (patch: Partial<SetupState>) => void;
  updateNetwork: (patch: Partial<SetupState["network"]>) => void;
  setStage: (stage: WizardStage) => void;
  t: (key: string, values?: Record<string, string | number>) => string;
}) {
  switch (stage) {
    case "setup-profile": return <ProfileScreen profile={setup.profile} onSelect={(profile) => update({ profile })} t={t} />;
    case "setup-hardware": return <HardwareScreen hardware={hardware} t={t} />;
    case "setup-resources": return <ResourcesScreen setup={setup} hardware={hardware} profile={setup.profile} update={update} t={t} />;
    case "setup-model": return <ModelScreen models={models} selected={setup.setupModel} onSelect={(setupModel) => update({ setupModel })} t={t} />;
    case "setup-storage": return <StorageScreen setup={setup} update={update} models={models} t={t} />;
    case "setup-network": return <NetworkScreen setup={setup} updateNetwork={updateNetwork} update={update} t={t} />;
    case "setup-security": return <SecurityScreen setup={setup} update={update} t={t} />;
    case "setup-trust": return <TrustScreen setup={setup} update={update} updateNetwork={updateNetwork} t={t} />;
    case "setup-invite": return <InviteScreen anchor={anchor} t={t} />;
    case "setup-health": return <HealthScreen health={health} hardware={hardware} t={t} />;
    case "setup-review": return <ReviewScreen setup={setup} models={models} setStage={setStage} t={t} />;
    default: return null;
  }
}

function Intro({ eyebrow, title, detail, children }: { eyebrow: string; title: string; detail: string; children?: ReactNode }) {
  return <div className="mx-auto w-full max-w-[900px]"><p className="eyebrow">{eyebrow}</p><h1 className="mt-2 max-w-2xl font-display text-2xl font-bold tracking-tight text-fg sm:text-3xl">{title}</h1><p className="mt-3 max-w-2xl text-sm leading-relaxed text-muted">{detail}</p>{children && <div className="mt-7">{children}</div>}</div>;
}

function ProfileScreen({ profile, onSelect, t }: { profile: SetupProfile | null; onSelect: (profile: SetupProfile) => void; t: (key: string) => string }) {
  const roles: { id: SetupProfile; icon: ReactNode; title: string; detail: string }[] = [
    { id: "admin", icon: <Landmark className="h-5 w-5" />, title: t("setupWizard.admin"), detail: t("setupWizard.adminDetail") },
    { id: "node", icon: <Server className="h-5 w-5" />, title: t("setupWizard.node"), detail: t("setupWizard.nodeDetail") },
    { id: "infer", icon: <Sparkles className="h-5 w-5" />, title: t("setupWizard.infer"), detail: t("setupWizard.inferDetail") },
  ];
  return <Intro eyebrow={t("setupWizard.welcomeEyebrow")} title={t("setupWizard.welcomeTitle")} detail={t("setupWizard.welcomeDetail")}><div className="grid gap-3 md:grid-cols-3">{roles.map((role) => <button key={role.id} type="button" aria-pressed={profile === role.id} onClick={() => onSelect(role.id)} className={["min-w-0 rounded-xl border bg-surface p-5 text-left transition-colors hover:border-line-strong", profile === role.id ? "border-iris/55 bg-iris/[0.07]" : "border-line"].join(" ")}><span className={["flex h-10 w-10 items-center justify-center rounded-lg border", profile === role.id ? "border-iris/40 bg-iris/12 text-iris" : "border-line bg-raised text-muted"].join(" ")}>{role.icon}</span><span className="mt-4 block font-display text-base font-semibold text-fg">{role.title}</span><span className="mt-2 block text-sm leading-relaxed text-muted">{role.detail}</span></button>)}</div><p className="mt-4 text-xs text-faint">{t("setupWizard.roleHint")}</p></Intro>;
}

function HardwareScreen({ hardware, t }: { hardware: HardwareResponse | null; t: (key: string, values?: Record<string, string | number>) => string }) {
  if (!hardware) return <Intro eyebrow={t("setupWizard.stepHardware")} title={t("setupWizard.hardwareTitle")} detail={t("setupWizard.hardwareUnavailable")} />;
  const ram = hardware.system.ram.total_mb / 1024;
  const gpu = hardware.system.gpus[0];
  const hasAmd = hardware.system.gpus.some((candidate) => candidate.vendor.toLowerCase() === "amd");
  const acceleratorCheck = hardware.system.os === "macos"
    ? null
    : hasAmd
      ? { ok: hardware.vulkan_available, label: t("setupWizard.vulkanAvailable") }
      : { ok: hardware.cuda_available, label: t("setupWizard.cudaAvailable") };
  return <Intro eyebrow={t("setupWizard.stepHardware")} title={t("setupWizard.hardwareTitle")} detail={t("setupWizard.hardwareDetail")}><Panel><div className="grid gap-px bg-line sm:grid-cols-3"><HardwareStat icon={<Cpu className="h-4 w-4" />} label={t("setupWizard.cpu")} value={t("setupWizard.cpuValue", { count: hardware.system.cpu_count })} detail={`${hardware.system.hostname} · ${hardware.system.os}`} /><HardwareStat icon={<Database className="h-4 w-4" />} label={t("setupWizard.memory")} value={`${ram.toFixed(1)} GB`} detail={`${(hardware.system.ram.available_mb / 1024).toFixed(1)} GB ${t("setupWizard.available")}`} /><HardwareStat icon={<Monitor className="h-4 w-4" />} label={t("setupWizard.gpu")} value={gpu?.name ?? t("setupWizard.noGpu")} detail={gpu?.vram_total_mb ? `${(gpu.vram_total_mb / 1024).toFixed(1)} GB VRAM · ${hardware.backend}` : hardware.backend} /></div><div className="flex flex-wrap gap-2 border-t border-line px-4 py-3"><StatusBadge ok={hardware.gpu_detected} label={t("setupWizard.gpuDetected")} />{acceleratorCheck && <StatusBadge ok={acceleratorCheck.ok} label={acceleratorCheck.label} />}<StatusBadge ok={hardware.p2p_discovery_available} label={t("setupWizard.discoveryAvailable")} /></div></Panel><p className="mt-4 text-xs text-faint">{t("setupWizard.gpuMaxNote")}</p></Intro>;
}

function ResourcesScreen({ setup, hardware, profile, update, t }: { setup: SetupState; hardware: HardwareResponse | null; profile: SetupProfile | null; update: (patch: Partial<SetupState>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const diskTotalGb = Math.max(1, Math.floor((hardware?.system.disk.total_mb ?? 0) / 1024));
  const diskFreeGb = Math.floor((hardware?.system.disk.free_mb ?? 0) / 1024);
  const diskValue = Math.min(Math.max(1, Math.round(setup.shardMaxGb)), diskTotalGb);
  return <Intro eyebrow={t("setupWizard.stepResources")} title={t("setupWizard.resourcesTitle")} detail={t("setupWizard.resourcesDetail")}><Panel className="space-y-6 px-5 py-5"><RangeSetting label={t("setupWizard.ramReserve")} detail={t("setupWizard.ramReserveDetail")} value={setup.ramReservePct} onChange={(value) => update({ ramReservePct: value })} /><RangeSetting label={t("setupWizard.cpuReserve")} detail={t("setupWizard.cpuReserveDetail")} value={setup.cpuReservePct} onChange={(value) => update({ cpuReservePct: value })} />{profile === "node" && <div><div className="flex items-end justify-between gap-4"><div className="min-w-0"><p className="text-sm font-medium text-fg">{t("setupWizard.shardSpace")}</p><p className="mt-1 text-xs leading-relaxed text-faint">{t("setupWizard.shardSpaceHint")}</p></div><span className="shrink-0 font-mono text-sm text-iris">{diskValue} GB</span></div><progress max={diskTotalGb} value={diskValue} aria-label={t("setupWizard.shardSpace")} className="mt-4 h-2 w-full accent-iris" /><div className="mt-2 flex justify-between gap-3 text-xs text-faint"><span>{t("setupWizard.diskAvailable", { value: diskFreeGb })}</span><span>{t("setupWizard.diskTotal", { value: diskTotalGb })}</span></div><input aria-label={t("setupWizard.shardSpace")} type="range" min={1} max={diskTotalGb} step={1} value={diskValue} onChange={(event) => update({ shardMaxGb: Number(event.target.value) })} className="mt-3 w-full accent-iris" /></div>}{hardware?.gpu_detected && <><ThermalSetting enabled value={setup.thermalLimitC} onChange={(value) => update({ thermalLimitC: value })} t={t} /><div className="flex items-start gap-3 rounded-lg border border-aqua/30 bg-aqua/8 px-3 py-3 text-sm text-muted"><Monitor className="mt-0.5 h-4 w-4 shrink-0 text-aqua" /><span>{t("setupWizard.gpuMaxNote")}</span></div></>}</Panel></Intro>;
}

function ThermalSetting({ enabled, value, onChange, t }: { enabled: boolean; value: number; onChange: (value: number) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  if (!enabled) return null;
  return <Field label={t("setupWizard.thermalLimit")} hint={t("setupWizard.thermalDetail")}><div className="flex items-center gap-3"><Input type="number" min={50} max={120} step={1} value={value} onChange={(event) => onChange(Number(event.target.value) || 85)} /><span className="text-sm text-muted">°C</span></div></Field>;
}

function ModelScreen({ models, selected, onSelect, t }: { models: ModelsResponse | null; selected: string; onSelect: (model: string) => void; t: (key: string) => string }) {
  return <Intro eyebrow={t("setupWizard.stepModel")} title={t("setupWizard.modelTitle")} detail={t("setupWizard.modelDetail")}><div className="grid gap-3 md:grid-cols-2">{(models?.models ?? []).map((model) => <button type="button" key={model.id} aria-pressed={selected === model.id} onClick={() => onSelect(model.id)} className={["min-w-0 rounded-xl border bg-surface p-4 text-left transition-colors", selected === model.id ? "border-iris/55 bg-iris/[0.06]" : "border-line hover:border-line-strong"].join(" ")}><div className="flex items-start justify-between gap-3"><span className="min-w-0 font-display text-sm font-semibold text-fg">{model.name}</span>{model.recommended && <Badge tone="aqua">{t("setupWizard.recommended")}</Badge>}</div><p className="mt-1 break-words text-xs text-muted">{model.source === "local" ? t("setupWizard.installedHere") : t("setupWizard.referenceHuggingFace")}</p><p className="mt-3 text-xs text-faint">{model.size_gb?.toFixed(1) ?? "-"} GB · {model.quantization ?? "GGUF"}{model.path ? ` · ${model.path}` : ""}</p></button>)}</div>{!models?.models.length && <Panel className="px-5 py-5 text-sm text-muted">{t("setupWizard.noModels")}</Panel>}<div className="mt-4 flex flex-wrap items-center gap-3"><Button type="button" variant={selected ? "ghost" : "outline"} onClick={() => onSelect("")}>{t("setupWizard.modelLater")}</Button><span className="text-xs text-faint">{t("setupWizard.modelSelectionOptional")}</span></div></Intro>;
}

function StorageScreen({ setup, update, models, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; models: ModelsResponse | null; t: (key: string) => string }) {
  return <Intro eyebrow={t("setupWizard.stepStorage")} title={t("setupWizard.storageTitle")} detail={t("setupWizard.storageDetail")}><Panel><PanelHeader title={t("setupWizard.storagePaths")} subtitle={t("setupWizard.storagePathsDetail")} /><div className="space-y-4 px-5 py-5"><Field label={t("setupWizard.modelsFolder")} hint={t("setupWizard.modelsFolderHint")}><Input value={setup.modelDirs[0] ?? models?.default_model_dir ?? ""} onChange={(event) => update({ modelDirs: [event.target.value] })} /></Field><Field label={t("setupWizard.shardsFolder")} hint={t("setupWizard.shardsFolderHint")}><Input value={setup.shardDir} onChange={(event) => update({ shardDir: event.target.value })} /></Field><Field label={t("setupWizard.shardSpace")} hint={t("setupWizard.shardSpaceHint")}><div className="flex items-center gap-3"><Input type="number" min={1} max={100000} step={1} value={setup.shardMaxGb} onChange={(event) => update({ shardMaxGb: Number(event.target.value) || 1 })} /><span className="text-sm text-muted">GB</span></div></Field></div></Panel></Intro>;
}

function NetworkScreen({ setup, update, updateNetwork, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; updateNetwork: (patch: Partial<SetupState["network"]>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const publicNetwork = setup.network.visibility === "public";
  const [locationStatus, setLocationStatus] = useState<"idle" | "asking" | "saved" | "denied" | "unavailable">("idle");
  const locationAsked = useRef(false);

  useEffect(() => {
    if (!publicNetwork) {
      locationAsked.current = false;
      if (setup.network.location) updateNetwork({ location: null });
      setLocationStatus("idle");
      return;
    }
    if (setup.network.location) {
      setLocationStatus("saved");
      return;
    }
    if (locationAsked.current) return;
    locationAsked.current = true;
    if (!navigator.geolocation) {
      setLocationStatus("unavailable");
      return;
    }
    setLocationStatus("asking");
    navigator.geolocation.getCurrentPosition(
      (position) => {
        updateNetwork({ location: { latitude: Number(position.coords.latitude.toFixed(2)), longitude: Number(position.coords.longitude.toFixed(2)), accuracy_m: position.coords.accuracy } });
        setLocationStatus("saved");
      },
      (error) => setLocationStatus(error.code === error.PERMISSION_DENIED ? "denied" : "unavailable"),
      { enableHighAccuracy: false, maximumAge: 300000, timeout: 10000 },
    );
  }, [publicNetwork, setup.network.location, updateNetwork]);

  return <Intro eyebrow={t("setupWizard.stepNetwork")} title={t("setupWizard.networkTitle")} detail={t("setupWizard.networkDetail")}><Panel><PanelHeader title={t("setupWizard.networkIdentity")} subtitle={t("setupWizard.networkIdentityDetail")} /><div className="grid gap-4 px-5 py-5 md:grid-cols-2"><Field label={t("setupWizard.publicName")} hint={t("setupWizard.publicNameHint")}><Input autoFocus value={setup.network.public_name} onChange={(event) => updateNetwork({ public_name: event.target.value })} /></Field><Field label={t("setupWizard.organization")}><Input value={setup.network.organization} onChange={(event) => updateNetwork({ organization: event.target.value })} /></Field><TextAreaField label={t("setupWizard.description")} value={setup.network.description} onChange={(value) => updateNetwork({ description: value })} /><TextAreaField label={t("setupWizard.rules")} value={setup.network.rules} onChange={(value) => updateNetwork({ rules: value })} /><TextAreaField label={t("setupWizard.channels")} value={setup.network.channels} onChange={(value) => updateNetwork({ channels: value })} /><Field label={t("setupWizard.visibility")} hint={t("setupWizard.visibilityHint")}><Select value={setup.network.visibility} onChange={(event) => updateNetwork({ visibility: event.target.value as NetworkVisibility, access_policy: event.target.value === "public" ? "anyone_with_anchor" : setup.network.access_policy })}><option value="private">{t("setupWizard.privateInternet")}</option><option value="public">{t("setupWizard.publicInternet")}</option><option value="local">{t("setupWizard.localOnly")}</option></Select></Field><Field label={t("setupWizard.accessPolicy")} hint={t("setupWizard.accessPolicyHint")}><Select value={setup.network.access_policy} onChange={(event) => updateNetwork({ access_policy: event.target.value as AccessPolicy })}><option value="authorized">{t("setupWizard.authorizedPeople")}</option><option value="anyone_with_anchor">{t("setupWizard.anyoneWithAnchor")}</option><option value="local_only">{t("setupWizard.localOnly")}</option></Select></Field><Field label={t("setupWizard.advertiseAddress")} hint={t("setupWizard.advertiseAddressHint")}><Input value={setup.advertiseAddr} placeholder={t("setupWizard.advertiseAddressPlaceholder")} onChange={(event) => update({ advertiseAddr: event.target.value })} /></Field>{publicNetwork && <div className="md:col-span-2 rounded-lg border border-aqua/25 bg-aqua/7 px-3 py-3 text-sm text-muted"><p>{t("setupWizard.locationPrompt")}</p><p className="mt-1 text-xs text-faint">{t(`setupWizard.location${locationStatus[0].toUpperCase()}${locationStatus.slice(1)}`)}</p></div>}</div></Panel></Intro>;
}

function SecurityScreen({ setup, update, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; t: (key: string) => string }) {
  return <Intro eyebrow={t("setupWizard.stepSecurity")} title={t("setupWizard.securityTitle")} detail={t("setupWizard.securityDetail")}><Panel><PanelHeader title={t("setupWizard.securityTitle")} subtitle={t("setupWizard.securityDetail")} /><div className="space-y-5 px-5 py-5"><Field label={t("setupWizard.adminExposure")} hint={t("setupWizard.adminExposureHint")}><Select value={setup.adminExposure} onChange={(event) => update({ adminExposure: event.target.value as AdminExposure })}><option value="localhost">{t("setupWizard.adminExposureLocalhost")}</option><option value="network">{t("setupWizard.adminExposureNetwork")}</option></Select></Field><Field label={t("setupWizard.adminToken")} hint={t("setupWizard.adminTokenDetail")}><Input readOnly value={setup.adminAccessToken} placeholder={t("setupWizard.adminTokenMissing")} className="font-mono" /></Field><label className="flex items-start gap-3 rounded-lg border border-line bg-raised px-3 py-3 text-sm text-fg"><input type="checkbox" checked={setup.tlsRequired} onChange={(event) => update({ tlsRequired: event.target.checked })} className="mt-0.5 h-4 w-4 accent-iris" /><span><span className="block font-medium">{t("setupWizard.tlsRequired")}</span><span className="mt-1 block text-xs leading-relaxed text-faint">{t("setupWizard.tlsDetail")}</span>{setup.network.visibility === "public" && <span className="mt-2 block text-xs font-medium text-aqua">{t("setupWizard.tlsPublicRequired")}</span>}</span></label></div></Panel></Intro>;
}

function TrustScreen({ setup, update, updateNetwork, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; updateNetwork: (patch: Partial<SetupState["network"]>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const publicNetwork = setup.network.visibility === "public";
  const [fingerprintPending, setFingerprintPending] = useState(false);
  useEffect(() => {
    let cancelled = false;
    if (!setup.trustDocument.trim()) {
      update({ trustFingerprint: "", trustConfirmed: false });
      return;
    }
    setFingerprintPending(true);
    void fingerprintFromTrustDocument(setup.trustDocument).then((fingerprint) => {
      if (cancelled) return;
      update({ trustFingerprint: fingerprint ?? "", trustConfirmed: false });
      setFingerprintPending(false);
    });
    return () => {
      cancelled = true;
    };
  }, [setup.trustDocument]);

  return <Intro eyebrow={t("setupWizard.stepTrust")} title={t("setupWizard.trustTitle")} detail={t(publicNetwork ? "setupWizard.trustPublicDetail" : "setupWizard.trustPrivateDetail")}><Panel><PanelHeader title={t("setupWizard.pasteTrust")} subtitle={t("setupWizard.pasteTrustDetail")} /><div className="space-y-4 px-5 py-5"><Field label={t("setupWizard.networkKind")} hint={t("setupWizard.networkKindHint")}><Select value={setup.network.visibility} onChange={(event) => updateNetwork({ visibility: event.target.value as NetworkVisibility, access_policy: event.target.value === "public" ? "anyone_with_anchor" : "authorized" })}><option value="private">{t("setupWizard.privateInternet")}</option><option value="public">{t("setupWizard.publicInternet")}</option></Select></Field><Field label={t("setupWizard.trustDocument")} hint={t("setupWizard.trustDocumentHint")}><textarea autoFocus value={setup.trustDocument} onChange={(event) => update({ trustDocument: event.target.value, trustConfirmed: false })} className="min-h-44 w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 font-mono text-xs text-fg outline-none transition-colors focus:border-iris focus:ring-2 focus:ring-iris/25" /></Field><div className="rounded-lg border border-line bg-raised px-3 py-3"><p className="text-xs font-medium uppercase tracking-[0.14em] text-faint">{t("setupWizard.fingerprintCalculated")}</p>{fingerprintPending ? <p className="mt-2 text-sm text-muted">{t("setupWizard.fingerprintWaiting")}</p> : setup.trustFingerprint ? <code className="mt-2 block break-words font-mono text-sm leading-6 text-fg">{groupFingerprint(setup.trustFingerprint)}</code> : <p className="mt-2 text-sm text-muted">{t("setupWizard.fingerprintWaiting")}</p>}<label className="mt-3 flex items-start gap-3 text-sm text-fg"><input type="checkbox" checked={setup.trustConfirmed} disabled={!setup.trustFingerprint || fingerprintPending} onChange={(event) => update({ trustConfirmed: event.target.checked })} className="mt-0.5 h-4 w-4 accent-iris" /><span>{t("setupWizard.fingerprintConfirm")}</span></label></div>{!publicNetwork && <Field label={t("setupWizard.connectionSecret")} hint={t("setupWizard.connectionSecretHint")}><Input type="password" value={setup.connectionKey} onChange={(event) => update({ connectionKey: event.target.value })} /></Field>}<Field label={t("setupWizard.bootstrapAddress")} hint={t("setupWizard.bootstrapAddressHint")}><Input value={setup.bootstrap} onChange={(event) => update({ bootstrap: event.target.value })} placeholder={t("setupWizard.bootstrapPlaceholder")} /></Field></div></Panel></Intro>;
}

function InviteScreen({ anchor, t }: { anchor: AnchorPreview | null; t: (key: string, values?: Record<string, string | number>) => string }) {
  if (!anchor) return <Intro eyebrow={t("setupWizard.stepInvite")} title={t("setupWizard.inviteTitle")} detail={t("setupWizard.anchorFailed")} />;
  return <Intro eyebrow={t("setupWizard.stepInvite")} title={t("setupWizard.inviteTitle")} detail={t("setupWizard.inviteDetail")}><div className="space-y-4"><CopyBlock label={t("setupWizard.trustDocument")} value={anchor.document} copyLabel={t("setupWizard.copy")} copied={t("setupWizard.copied")} /><CopyBlock label={t("setupWizard.publicKey")} value={anchor.public_key} copyLabel={t("setupWizard.copy")} copied={t("setupWizard.copied")} /><div className="rounded-xl border border-aqua/35 bg-aqua/8 px-4 py-4"><p className="text-xs font-medium uppercase tracking-[0.14em] text-aqua">{t("setupWizard.fingerprint")}</p><code className="mt-2 block break-words font-mono text-sm leading-6 text-fg">{anchor.fingerprint}</code><p className="mt-2 text-xs leading-relaxed text-muted">{t("setupWizard.fingerprintInvite")}</p></div><p className="text-xs text-faint">{t("setupWizard.trustFilename", { filename: anchor.trust_filename })}</p></div></Intro>;
}

function HealthScreen({ health, hardware, t }: { health: HealthResponse | null; hardware: HardwareResponse | null; t: (key: string) => string }) {
  const hasAmd = hardware?.system.gpus.some((gpu) => gpu.vendor.toLowerCase() === "amd") ?? false;
  const acceleratorCheck = hardware?.system.os === "macos" || !hardware?.gpu_detected ? null : hasAmd
    ? { id: "vulkan", ok: Boolean(health?.vulkan_working), label: t("setupWizard.checkVulkan") }
    : { id: "cuda", ok: Boolean(health?.cuda_working), label: t("setupWizard.checkCuda") };
  const checks = health ? [{ id: "gpu", ok: health.gpu_detected, label: t("setupWizard.checkGpu") }, ...(acceleratorCheck ? [acceleratorCheck] : []), { id: "config", ok: health.configuration_ready, label: t("setupWizard.checkConfig") }, { id: "p2p", ok: health.p2p_discovery_working, label: t("setupWizard.checkP2p") }, { id: "ipv6", ok: health.public_ipv6_enabled, label: t("setupWizard.checkIpv6") }] : [];
  return <Intro eyebrow={t("setupWizard.stepHealth")} title={t("setupWizard.healthTitle")} detail={t("setupWizard.healthDetail")}><Panel className="divide-y divide-line">{checks.map((check) => <div key={check.id} className="flex items-center justify-between gap-4 px-5 py-4"><span className="text-sm text-fg">{check.label}</span><StatusBadge ok={check.ok} label={check.ok ? t("setupWizard.ok") : t("setupWizard.notAvailable")} /></div>)}</Panel><p className="mt-4 text-xs text-faint">{t("setupWizard.healthModelLater")}</p></Intro>;
}

function ReviewScreen({ setup, models, setStage, t }: { setup: SetupState; models: ModelsResponse | null; setStage: (stage: WizardStage) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const selected = models?.models.find((model) => model.id === setup.setupModel);
  return <Intro eyebrow={t("setupWizard.stepReview")} title={t("setupWizard.reviewTitle")} detail={t("setupWizard.reviewDetail")}><Panel><SummaryRow label={t("setupWizard.profileLabel")} value={setup.profile ? t(`setupWizard.${setup.profile}`) : "-"} onEdit={() => setStage("setup-profile")} />{setup.profile !== "infer" && <SummaryRow label={t("setupWizard.resourcesLabel")} value={`${setup.ramReservePct}% RAM · ${setup.cpuReservePct}% CPU${setup.profile === "node" ? ` · ${setup.shardMaxGb} GB` : ""}`} onEdit={() => setStage("setup-resources")} />}{setup.profile === "admin" && <><SummaryRow label={t("setupWizard.modelLabel")} value={selected?.name ?? t("setupWizard.noModelSelected")} onEdit={() => setStage("setup-model")} /><SummaryRow label={t("setupWizard.storageLabel")} value={`${setup.shardMaxGb} GB`} onEdit={() => setStage("setup-storage")} /><SummaryRow label={t("setupWizard.networkLabel")} value={setup.network.public_name || "-"} onEdit={() => setStage("setup-network")} /><SummaryRow label={t("setupWizard.adminToken")} value={setup.adminAccessToken ? t("setupWizard.generated") : t("setupWizard.adminTokenMissing")} onEdit={() => setStage("setup-security")} /></>}{setup.profile !== "admin" && <SummaryRow label={t("setupWizard.trustLabel")} value={setup.trustFingerprint ? t("setupWizard.fingerprintEntered") : t("setupWizard.fingerprintMissing")} onEdit={() => setStage("setup-trust")} />}</Panel><p className="mt-4 text-xs text-faint">{t("setupWizard.reviewNote")}</p></Intro>;
}

function SavedScreen() {
  const { t } = useI18n();
  return <div className="flex h-full min-h-0 items-center justify-center bg-ink px-5 py-10"><Panel className="w-full max-w-lg"><div className="flex flex-col items-center px-6 py-12 text-center"><span className="flex h-12 w-12 items-center justify-center rounded-xl border border-aqua/35 bg-aqua/12 text-aqua"><Check className="h-6 w-6" /></span><h1 className="mt-5 font-display text-lg font-semibold text-fg">{t("setupWizard.savedTitle")}</h1><p className="mt-2 max-w-sm text-sm leading-relaxed text-muted">{t("setupWizard.savedDetail")}</p><div className="mt-6 h-1.5 w-40 overflow-hidden rounded-full bg-line"><div className="h-full w-2/3 animate-pulse rounded-full bg-aqua" /></div></div></Panel></div>;
}

function LoadingScreen() {
  return <div className="mx-auto w-full max-w-[900px] animate-pulse"><div className="h-3 w-24 rounded bg-line" /><div className="mt-4 h-9 w-2/3 rounded bg-line" /><div className="mt-3 h-4 w-full max-w-xl rounded bg-line" /><div className="mt-8 h-48 rounded-xl border border-line bg-surface" /></div>;
}

function HardwareStat({ icon, label, value, detail }: { icon: ReactNode; label: string; value: string; detail: string }) {
  return <div className="min-w-0 bg-surface px-4 py-4"><div className="flex items-center gap-2 text-xs font-medium text-muted">{icon}<span>{label}</span></div><p className="mt-3 truncate font-display text-[0.95rem] font-semibold text-fg">{value}</p><p className="mt-1 text-xs text-faint">{detail}</p></div>;
}

function RangeSetting({ label, detail, value, onChange }: { label: string; detail: string; value: number; onChange: (value: number) => void }) {
  return <div><div className="flex items-center justify-between gap-4"><div className="min-w-0"><p className="text-sm font-medium text-fg">{label}</p><p className="mt-1 text-xs leading-relaxed text-faint">{detail}</p></div><span className="shrink-0 font-mono text-sm text-iris">{Math.round(value)}%</span></div><input aria-label={label} type="range" min={0} max={80} step={5} value={value} onChange={(event) => onChange(Number(event.target.value))} className="mt-4 w-full accent-iris" /></div>;
}

function TextAreaField({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  return <Field label={label}><textarea value={value} onChange={(event) => onChange(event.target.value)} className="min-h-20 w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 text-sm text-fg outline-none transition-colors focus:border-iris focus:ring-2 focus:ring-iris/25" /></Field>;
}

function CopyBlock({ label, value, copyLabel, copied }: { label: string; value: string; copyLabel: string; copied: string }) {
  const [isCopied, setIsCopied] = useState(false);
  async function copy() { try { await navigator.clipboard.writeText(value); setIsCopied(true); window.setTimeout(() => setIsCopied(false), 1800); } catch { /* o conteúdo continua selecionável */ } }
  return <Panel><PanelHeader title={label} actions={<Button size="sm" onClick={() => void copy()} icon={isCopied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}>{isCopied ? copied : copyLabel}</Button>} /><pre className="max-h-72 overflow-auto whitespace-pre-wrap break-words px-5 py-4 font-mono text-[0.7rem] leading-5 text-muted">{value}</pre></Panel>;
}

function SummaryRow({ label, value, onEdit }: { label: string; value: string; onEdit: () => void }) {
  const { t } = useI18n();
  return <div className="flex min-w-0 items-center justify-between gap-3 border-b border-line px-5 py-4 last:border-b-0"><div className="min-w-0"><p className="text-xs text-faint">{label}</p><p className="mt-1 truncate text-sm font-medium text-fg">{value}</p></div><Button size="sm" variant="ghost" onClick={onEdit}>{t("setupWizard.edit")}</Button></div>;
}

function StatusBadge({ ok, label }: { ok: boolean; label: string }) {
  return <Badge tone={ok ? "aqua" : "amber"}><span className={ok ? "h-1.5 w-1.5 rounded-full bg-aqua" : "h-1.5 w-1.5 rounded-full bg-amber"} />{label}</Badge>;
}

function clusterIdFromDocument(document: string): string {
  try {
    const parsed = JSON.parse(document) as { anchor?: { cluster_id?: unknown } };
    return typeof parsed.anchor?.cluster_id === "string" ? parsed.anchor.cluster_id : "";
  } catch {
    return "";
  }
}

function isReadyToSave(setup: SetupState): boolean {
  if (!setup.profile || !setup.trustDocument.trim() || !setup.trustFingerprint.trim() || !setup.trustConfirmed) return false;
  if (setup.profile !== "admin" && setup.network.visibility !== "public" && !setup.connectionKey.trim()) return false;
  if (setup.profile === "admin" && (!setup.network.public_name.trim() || !setup.advertiseAddr.trim() || !setup.adminAccessToken.trim())) return false;
  return true;
}

function canContinue(stage: WizardStage, setup: SetupState): boolean {
  if (stage === "setup-network" && setup.profile === "admin") {
    return Boolean(setup.network.public_name.trim() && setup.advertiseAddr.trim());
  }
  if (stage === "setup-security" && setup.profile === "admin") {
    return Boolean(setup.adminAccessToken.trim() && (setup.network.visibility !== "public" || setup.tlsRequired));
  }
  if (stage === "setup-trust") {
    return Boolean(setup.trustFingerprint.trim() && setup.trustConfirmed && (setup.network.visibility === "public" || setup.connectionKey.trim()));
  }
  return true;
}

function modelDirsForSave(dirs: string[], selectedPath?: string): string[] {
  const result = dirs.filter(Boolean);
  if (selectedPath) {
    const separator = selectedPath.lastIndexOf("/");
    if (separator > 0) {
      const parent = selectedPath.slice(0, separator);
      if (!result.includes(parent)) result.push(parent);
    }
  }
  return result;
}
