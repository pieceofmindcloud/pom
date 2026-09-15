/**
 * Primeiro acesso real.
 *
 * Esta tela usa o mesmo layout do wizard de produto, mas não usa os mocks de
 * `SetupWizard.tsx`: cada leitura e cada gravação conversa com `/api/setup/*`.
 * O percurso começa pela máquina, porque essa é a primeira resposta útil para
 * quem ainda não sabe o que o POM pode compartilhar.
 */
import { useEffect, useMemo, useRef, useState, type ChangeEvent, type ReactNode } from "react";
import { Check, ChevronDown, Copy, Cpu, Database, Download, Landmark, LocateFixed, MapPin, Monitor, Server, ShieldCheck, Sparkles, Upload } from "lucide-react";
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
import { useI18n } from "../i18n/provider";
import { LogoMark } from "../components/Logo";
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
  p2p_port: number;
  thermal_limit_c: number;
  default_model_dir: string;
  default_shard_dir: string;
  shard_max_gb: number;
  ram_reserve_pct: number;
  cpu_reserve_pct: number;
  model_dirs: string[];
  shard_dir: string;
  node_name: string | null;
  network: SetupState["network"];
  public_directory: string[];
}

interface GeoCityReference {
  id: string;
  name: string;
  latitude: number;
  longitude: number;
}

interface GeoCountryReference {
  code: string;
  name: string;
  cities: GeoCityReference[];
}

interface GeoReferenceResponse {
  countries: GeoCountryReference[];
}

interface PublicJoinPayload {
  name?: string;
  cluster_id?: string;
  fingerprint?: string;
  bootstrap_address?: string;
  trust_document?: string;
  access_policy?: AccessPolicy;
  directory_replicas?: string[];
}

interface AnchorPreview {
  cluster_id: string;
  document: string;
  fingerprint: string;
  trust_filename: string;
  private_connection_key?: string;
  admin_access_token?: string;
}

interface TrustNetworkMetadata {
  public_name?: string;
  organization?: string;
  visibility?: NetworkVisibility;
  access_policy?: AccessPolicy;
  country_code?: string;
  country_name?: string;
  city?: string;
  geo_label?: string;
  advertised_address?: string;
  bootstrap_address?: string;
  directory_replicas?: string[];
}

interface HealthResponse {
  gpu_detected: boolean;
  cuda_working: boolean;
  vulkan_working: boolean;
  configuration_ready: boolean;
  p2p_discovery_working: boolean;
  public_ipv6_enabled: boolean;
  firewall_allowed: boolean;
  firewall_supported: boolean;
}

interface AutoDiscoveryResult {
  request_id: string;
  trust_document: string;
  trust_fingerprint: string;
  connection_key: string;
}

interface AutoDiscoveryPeer {
  request_id: string;
  address: string;
}

interface AutoDiscoveryStatus {
  enabled: boolean;
  active: boolean;
  searching: boolean;
  found: boolean;
  port: number;
  peers: AutoDiscoveryPeer[];
  result?: AutoDiscoveryResult;
}

export interface SetupState {
  profile: SetupProfile | null;
  bootstrap: string;
  publicDirectory: string;
  connectionKey: string;
  advertiseAddr: string;
  trustDocument: string;
  trustFingerprint: string;
  adminAccessToken: string;
  adminExposure: AdminExposure;
  tlsRequired: boolean;
  nodeName: string;
  network: {
    public_name: string;
    organization: string;
    description: string;
    rules: string;
    channels: string;
    visibility: NetworkVisibility;
    access_policy: AccessPolicy;
    country_code: string;
    country_name: string;
    city: string;
    geo_label: string;
    location: { latitude: number; longitude: number; accuracy_m?: number } | null;
  };
  ramReservePct: number;
  cpuReservePct: number;
  modelDirs: string[];
  shardDir: string;
  shardMaxGb: number;
  thermalLimitC: number;
}

export function createRandomNetworkName(randomValue = Math.random()): string {
  const normalized = Number.isFinite(randomValue) ? Math.min(0.999999, Math.max(0, randomValue)) : 0;
  return `POM-${Math.floor(normalized * 36 ** 5).toString(36).padStart(5, "0").toUpperCase()}`;
}

export function diskSharePercent(sharedGb: number, availableGb: number): number {
  if (!Number.isFinite(sharedGb) || !Number.isFinite(availableGb) || availableGb <= 0) return 0;
  return Math.min(100, Math.max(0, (sharedGb / availableGb) * 100));
}

export function formatAdvertiseAddress(value: string, p2pPort: number): string {
  const address = value.trim();
  if (!address) return "";
  if (/^\[[^\]]+\]:\d+$/.test(address)) return address;
  const unbracketed = address.replace(/^\[|\]$/g, "");
  if (unbracketed.includes(":")) {
    return p2pPort > 0 ? `[${unbracketed}]:${p2pPort}` : `[${unbracketed}]`;
  }
  return p2pPort > 0 ? `${unbracketed}:${p2pPort}` : unbracketed;
}

function advertiseHost(value: string): string {
  const address = value.trim();
  const bracketed = address.match(/^\[([^\]]+)\](?::\d+)?$/);
  if (bracketed) return bracketed[1];
  return address;
}

function trustNetworkMetadata(document: string): TrustNetworkMetadata | null {
  try {
    const parsed = JSON.parse(document) as { setup?: TrustNetworkMetadata };
    return parsed.setup && typeof parsed.setup === "object" ? parsed.setup : null;
  } catch {
    return null;
  }
}

const INITIAL: SetupState = {
  profile: null,
  bootstrap: "",
  publicDirectory: "",
  connectionKey: "",
  advertiseAddr: "",
  trustDocument: "",
  trustFingerprint: "",
  network: {
    public_name: "",
    organization: "",
    description: "",
    rules: "",
    channels: "",
    visibility: "private",
    access_policy: "anyone_with_anchor",
    country_code: "",
    country_name: "",
    city: "",
    geo_label: "",
    location: null,
  },
  ramReservePct: 20,
  cpuReservePct: 20,
  modelDirs: [],
  shardDir: "",
  shardMaxGb: 20,
  thermalLimitC: 85,
  adminAccessToken: "",
  adminExposure: "localhost",
  tlsRequired: false,
  nodeName: "",
};

export function Setup() {
  const { t } = useI18n();
  const toast = useToast();
  const [setup, setSetup] = useState<SetupState>(INITIAL);
  const [stage, setStage] = useState<WizardStage>("setup-profile");
  const [hardware, setHardware] = useState<HardwareResponse | null>(null);
  const [geoReference, setGeoReference] = useState<GeoReferenceResponse | null>(null);
  const [anchor, setAnchor] = useState<AnchorPreview | null>(null);
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const steps = useMemo(() => setupStepsFor(setup.profile), [setup.profile]);

  useEffect(() => {
    let cancelled = false;
    Promise.all([
      api.get<HardwareResponse>("/api/setup/hardware", { auth: false }),
      api.get<GeoReferenceResponse>("/api/setup/geo-reference", { auth: false }),
    ])
      .then(([detected, geo]) => {
        if (cancelled) return;
        setHardware(detected);
        setGeoReference(geo);
        setSetup((current) => ({
          ...current,
          // First access keeps storage predictable. Existing paths remain
          // editable later from administration, not in this wizard.
          modelDirs: [detected.default_model_dir],
          shardDir: detected.default_shard_dir,
          shardMaxGb: Math.min(
            detected.shard_max_gb || 20,
            Math.max(0, detected.system.disk.free_mb / 1024),
          ),
          ramReservePct: detected.ram_reserve_pct ?? current.ramReservePct,
          cpuReservePct: detected.cpu_reserve_pct ?? current.cpuReservePct,
          thermalLimitC: detected.thermal_limit_c || 85,
          nodeName: current.nodeName.trim() || detected.node_name?.trim() || detected.system.hostname,
          network: {
            ...current.network,
            ...detected.network,
            public_name: current.network.public_name.trim() || detected.network.public_name.trim() || createRandomNetworkName(),
            visibility: "private",
            access_policy: "anyone_with_anchor",
          },
          advertiseAddr: current.advertiseAddr || detected.suggested_advertise_addr || "",
          publicDirectory: detected.public_directory.join(", "),
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

  useEffect(() => {
    const raw = sessionStorage.getItem("pom.public-network-join");
    if (!raw) return;
    sessionStorage.removeItem("pom.public-network-join");
    try {
      const network = JSON.parse(raw) as PublicJoinPayload;
      setSetup((current) => ({
        ...current,
        bootstrap: network.bootstrap_address ?? "",
        publicDirectory: network.directory_replicas?.join(", ") ?? current.publicDirectory,
        trustDocument: network.trust_document ?? "",
        trustFingerprint: "",
        network: {
          ...current.network,
          public_name: network.name ?? network.cluster_id ?? "",
          visibility: "public",
          access_policy: network.access_policy ?? "anyone_with_anchor",
        },
      }));
    } catch {
      // A stale browser hint must never prevent the normal wizard from opening.
    }
  }, []);

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
      const preview = await api.post<AnchorPreview>(
        "/api/setup/admin-anchor/preview",
        {
          cluster_id: clusterId,
          visibility: setup.network.visibility,
          network: setup.network,
          advertise_addr: setup.advertiseAddr,
          bootstrap: setup.bootstrap,
          public_directory: setup.publicDirectory.split(",").map((value) => value.trim()).filter(Boolean),
        },
        { auth: false },
      );
      setAnchor(preview);
      update({
        trustDocument: preview.document,
        trustFingerprint: preview.fingerprint,
        adminAccessToken: "",
        connectionKey: setup.network.visibility === "public" ? "" : preview.private_connection_key ?? "",
      });
      setStage("setup-invite");
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
      setStage("setup-review");
    } catch (error) {
      toast.fromError(t("setupWizard.healthFailed"), error);
    } finally {
      setBusy(false);
    }
  }

  async function save() {
    setBusy(true);
    try {
      await api.post("/api/setup/save", {
        profile: setup.profile,
        bootstrap: setup.bootstrap.trim(),
        public_directory: setup.publicDirectory.split(",").map((value) => value.trim()).filter(Boolean),
        connection_key: setup.connectionKey.trim(),
        advertise_addr: setup.advertiseAddr.trim(),
        cluster_id: clusterIdFromDocument(setup.trustDocument) || setup.network.public_name.trim(),
        trust_document: setup.trustDocument.trim(),
        trust_fingerprint: setup.trustFingerprint.trim(),
        network: setup.network,
        resources: { ram_reserve_pct: setup.ramReservePct, cpu_reserve_pct: setup.cpuReservePct },
        thermal_limit_c: setup.thermalLimitC,
        admin_exposure: setup.adminExposure,
        admin_access_token: setup.adminExposure === "network" ? setup.adminAccessToken : "",
        tls_required: setup.tlsRequired,
        model_dirs: setup.modelDirs,
        shard_dir: setup.shardDir.trim(),
        shard_max_gb: setup.shardMaxGb,
        setup_model: null,
        node_name: setup.nodeName.trim(),
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
    if (stageNow === "setup-invite") {
      setStage("setup-discovery");
      return;
    }
    if (stageNow === "setup-discovery" && setup.profile !== "admin") {
      setStage("setup-trust");
      return;
    }
    if (stageNow === "setup-discovery" || stageNow === "setup-trust") {
      void runHealth();
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
      <WizardLayout
        stage={stage}
        steps={steps}
        canGoBack={stage !== "setup-profile" && !busy}
        onBack={back}
        footer={
          stage === "setup-review" ? (
            <Button variant="primary" loading={busy} disabled={!isReadyToSave(setup)} onClick={() => void save()} icon={<ShieldCheck className="h-4 w-4" />}>
              {t("setupWizard.finish")}
            </Button>
          ) : stage === "setup-profile" ? (
            <Button variant="primary" disabled={!canContinue(stage, setup)} onClick={() => continueFrom(stage)}>
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
      {loading ? <LoadingScreen /> : <Screen stage={stage} setup={setup} hardware={hardware} geoReference={geoReference} anchor={anchor} health={health} update={update} updateNetwork={updateNetwork} setStage={setStage} t={t} />}
      </WizardLayout>
    </div>
  );
}

function Screen({
  stage,
  setup,
  hardware,
  geoReference,
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
  geoReference: GeoReferenceResponse | null;
  anchor: AnchorPreview | null;
  health: HealthResponse | null;
  update: (patch: Partial<SetupState>) => void;
  updateNetwork: (patch: Partial<SetupState["network"]>) => void;
  setStage: (stage: WizardStage) => void;
  t: (key: string, values?: Record<string, string | number>) => string;
}) {
  switch (stage) {
    case "setup-profile": return <ProfileScreen profile={setup.profile} setup={setup} hardware={hardware} onSelect={(profile) => update({ profile })} update={update} t={t} />;
    case "setup-location": return <LocationScreen setup={setup} countries={geoReference?.countries ?? []} updateNetwork={updateNetwork} t={t} />;
    case "setup-network": return <NetworkScreen setup={setup} updateNetwork={updateNetwork} t={t} />;
    case "setup-trust": return <TrustScreen setup={setup} update={update} updateNetwork={updateNetwork} t={t} />;
    case "setup-invite": return <InviteScreen anchor={anchor} visibility={setup.network.visibility} connectionKey={setup.connectionKey} t={t} />;
    case "setup-discovery": return <AutoDiscoveryScreen setup={setup} update={update} updateNetwork={updateNetwork} t={t} />;
    case "setup-review": return <ReviewScreen setup={setup} health={health} hardware={hardware} setStage={setStage} t={t} />;
    default: return null;
  }
}

function Intro({ eyebrow, title, detail, titleAdornment, children }: { eyebrow?: string; title: string; detail: string; titleAdornment?: ReactNode; children?: ReactNode }) {
  return <div className="mx-auto w-full max-w-[900px]">{eyebrow && <p className="eyebrow">{eyebrow}</p>}<div className={titleAdornment ? "flex items-center gap-4" : ""}>{titleAdornment}{<h1 className="mt-2 max-w-2xl font-display text-2xl font-bold tracking-tight text-fg sm:text-3xl">{title}</h1>}</div><p className="mt-3 max-w-2xl text-sm leading-relaxed text-muted">{detail}</p>{children && <div className="mt-7">{children}</div>}</div>;
}

export function ProfileScreen({ profile, setup, hardware, onSelect, update, t }: { profile: SetupProfile | null; setup: SetupState; hardware: HardwareResponse | null; onSelect: (profile: SetupProfile) => void; update: (patch: Partial<SetupState>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const roles: { id: SetupProfile; icon: ReactNode; title: string; detail: string }[] = [
    { id: "admin", icon: <Landmark className="h-5 w-5" />, title: t("setupWizard.admin"), detail: t("setupWizard.adminDetail") },
    { id: "node", icon: <Server className="h-5 w-5" />, title: t("setupWizard.node"), detail: t("setupWizard.nodeDetail") },
    { id: "infer", icon: <Sparkles className="h-5 w-5" />, title: t("setupWizard.infer"), detail: t("setupWizard.inferDetail") },
  ];
  const ram = hardware ? hardware.system.ram.total_mb / 1024 : 0;
  const gpu = hardware?.system.gpus[0];
  const hasNvidia = hardware?.system.gpus.some((candidate) => candidate.vendor.toLowerCase() === "nvidia") ?? false;
  const hasAmd = hardware?.system.gpus.some((candidate) => candidate.vendor.toLowerCase() === "amd") ?? false;
  const acceleratorCheck = hardware && hasNvidia
    ? { ok: hardware.cuda_available, label: t("setupWizard.cudaAvailable") }
    : hardware && hasAmd
      ? { ok: hardware.vulkan_available, label: t("setupWizard.vulkanAvailable") }
      : null;
  const cudaHint = hardware && hasNvidia && !hardware.cuda_available ? t("setupWizard.cudaInstallHint") : null;
  const gpuCheckLabel = hardware?.gpu_detected ? t("setupWizard.checkGpu") : t("setupWizard.gpuUnavailable");
  const p2pPort = hardware?.p2p_port ?? 0;
  const availableDiskGb = hardware ? Math.max(0, hardware.system.disk.free_mb / 1024) : 0;
  const totalDiskGb = hardware ? Math.max(0, hardware.system.disk.total_mb / 1024) : 0;
  const diskMaxGb = Math.max(1, Math.ceil(availableDiskGb));
  const sharedDiskGb = Math.min(diskMaxGb, Math.max(0, setup.shardMaxGb));
  const sharedDiskPercent = diskSharePercent(sharedDiskGb, availableDiskGb);
  return <Intro title={t("setupWizard.welcomeTitle")} detail={t("setupWizard.welcomeDetail")} titleAdornment={<LogoMark size={72} />}><div className="space-y-7"><section><div className="grid gap-3 md:grid-cols-3">{roles.map((role) => <button key={role.id} type="button" aria-pressed={profile === role.id} onClick={() => onSelect(role.id)} className={["min-w-0 rounded-xl border bg-surface p-5 text-left transition-colors hover:border-line-strong", profile === role.id ? "border-iris/55 bg-iris/[0.07]" : "border-line"].join(" ")}><span className={["flex h-10 w-10 items-center justify-center rounded-lg border", profile === role.id ? "border-iris/40 bg-iris/12 text-iris" : "border-line bg-raised text-muted"].join(" ")}>{role.icon}</span><span className="mt-4 block font-display text-base font-semibold text-fg">{role.title}</span><span className="mt-2 block text-sm leading-relaxed text-muted">{role.detail}</span></button>)}</div></section>{hardware && <section className="border-t border-line pt-6"><div className="grid gap-px overflow-hidden rounded-xl border border-line bg-line sm:grid-cols-3"><HardwareStat icon={<Cpu className="h-4 w-4" />} label={t("setupWizard.cpu")} value={t("setupWizard.cpuValue", { count: hardware.system.cpu_count })} detail={`${hardware.system.hostname} · ${hardware.system.os}`} /><HardwareStat icon={<Database className="h-4 w-4" />} label={t("setupWizard.memory")} value={`${ram.toFixed(1)} GB`} detail={`${(hardware.system.ram.available_mb / 1024).toFixed(1)} GB ${t("setupWizard.available")}`} /><HardwareStat icon={<Monitor className="h-4 w-4" />} label={t("setupWizard.gpu")} value={gpu?.name ?? t("setupWizard.noGpu")} detail={gpu?.vram_total_mb ? `${(gpu.vram_total_mb / 1024).toFixed(1)} GB VRAM · ${hardware.backend}` : hardware.backend} /></div><div data-testid="disk-share-section" className="border-x border-b border-line bg-surface px-3 py-3"><Field label={t("setupWizard.shardSpace")} hint={t("setupWizard.shardSpaceHint")}><div className="flex items-center justify-between gap-3 text-xs"><span className="text-muted">{t("setupWizard.diskAvailable", { value: availableDiskGb.toFixed(1) })}</span><span className="font-display text-sm font-semibold text-fg">{sharedDiskGb.toFixed(0)} GB</span></div><div role="img" aria-label={t("setupWizard.shardSpace")} className="mt-2 h-2 overflow-hidden rounded-full bg-line"><div className="h-full rounded-full bg-iris transition-[width]" style={{ width: `${sharedDiskPercent}%` }} /></div><input data-testid="disk-share-slider" aria-label={t("setupWizard.shardSpace")} type="range" min="0" max={diskMaxGb} step="1" value={sharedDiskGb} onChange={(event) => update({ shardMaxGb: Number(event.target.value) })} className="mt-3 h-1.5 w-full cursor-pointer accent-iris" /><div className="mt-2 flex justify-between gap-3 text-[0.7rem] text-faint"><span>{t("setupWizard.diskAvailable", { value: availableDiskGb.toFixed(1) })}</span><span>{t("setupWizard.diskTotal", { value: totalDiskGb.toFixed(1) })}</span></div></Field></div><div className="flex flex-wrap gap-2 border-x border-b border-line px-3 py-2"><StatusBadge ok={hardware.gpu_detected} label={gpuCheckLabel} />{acceleratorCheck && <StatusBadge ok={acceleratorCheck.ok} label={acceleratorCheck.label} />}<StatusBadge ok={hardware.p2p_discovery_available} label={t("setupWizard.checkP2p")} /><StatusBadge ok={hardware.public_ipv6_available} label={t("setupWizard.checkIpv6")} /></div>{cudaHint && <p className="border-x border-b border-line px-3 py-2 text-xs leading-relaxed text-amber">{cudaHint}</p>}<div className="border-x border-b border-line px-3 py-3"><Field label={t("setupWizard.nodeName")}><Input autoFocus value={setup.nodeName} onChange={(event) => update({ nodeName: event.target.value })} /></Field></div>{profile === "admin" && <div className="border-x border-b border-line px-3 py-3"><Field label={t("setupWizard.advertiseAddress")} hint={t("setupWizard.advertiseAddressHint")}><div className="flex items-center gap-2"><Input value={advertiseHost(setup.advertiseAddr)} placeholder={t("setupWizard.advertiseAddressPlaceholder")} onChange={(event) => update({ advertiseAddr: formatAdvertiseAddress(event.target.value, p2pPort) })} /><span className="shrink-0 font-mono text-sm text-muted">:{p2pPort}</span></div></Field></div>}</section>}</div></Intro>;
}

function LocationScreen({ setup, countries, updateNetwork, t }: { setup: SetupState; countries: GeoCountryReference[]; updateNetwork: (patch: Partial<SetupState["network"]>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const [locationStatus, setLocationStatus] = useState<"idle" | "asking" | "saved" | "denied" | "unavailable">("idle");
  const locationAsked = useRef(false);
  const selectedCountry = countries.find((country) => country.code === setup.network.country_code);
  const cityIsReference = selectedCountry?.cities.some((city) => city.name === setup.network.city) ?? false;

  useEffect(() => {
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
        updateNetwork({ location: { latitude: Number(position.coords.latitude.toFixed(5)), longitude: Number(position.coords.longitude.toFixed(5)), accuracy_m: position.coords.accuracy } });
        setLocationStatus("saved");
      },
      (error) => setLocationStatus(error.code === error.PERMISSION_DENIED ? "denied" : "unavailable"),
      { enableHighAccuracy: false, maximumAge: 300000, timeout: 10000 },
    );
  }, [setup.network.location, updateNetwork]);

  return <Intro eyebrow={t("setupWizard.stepLocation")} title={t("setupWizard.locationTitle")} detail={t("setupWizard.locationDetail")}><Panel><PanelHeader title={t("setupWizard.locationPanelTitle")} subtitle={t("setupWizard.locationPanelDetail")} /><div className="grid gap-4 px-5 py-5 md:grid-cols-2"><Field label={t("setupWizard.country")} hint={t("setupWizard.countryHint")}><Select autoFocus value={setup.network.country_code} onChange={(event) => { const country = countries.find((candidate) => candidate.code === event.target.value); updateNetwork({ country_code: country?.code ?? "", country_name: country?.name ?? "", city: "" }); }}><option value="">{t("setupWizard.chooseCountry")}</option>{countries.map((country) => <option key={country.code} value={country.code}>{country.name}</option>)}</Select></Field><Field label={t("setupWizard.city")} hint={t("setupWizard.cityHint")}><Select value={cityIsReference ? setup.network.city : setup.network.city ? "__manual__" : ""} disabled={!selectedCountry} onChange={(event) => updateNetwork({ city: event.target.value === "__manual__" ? "" : event.target.value })}><option value="">{t("setupWizard.chooseCity")}</option>{selectedCountry?.cities.map((city) => <option key={city.id} value={city.name}>{city.name}</option>)}<option value="__manual__">{t("setupWizard.otherCity")}</option></Select></Field>{selectedCountry && !cityIsReference && <Field label={t("setupWizard.manualCity")} hint={t("setupWizard.manualCityHint")}><Input value={setup.network.city} onChange={(event) => updateNetwork({ city: event.target.value })} /></Field>}<div className="md:col-span-2 flex items-start gap-3 rounded-lg border border-aqua/25 bg-aqua/7 px-3 py-3 text-sm text-muted"><MapPin className="mt-0.5 h-4 w-4 shrink-0 text-aqua" /><span><span className="block font-medium text-fg">{t("setupWizard.locationPrompt")}</span><span className="mt-1 block text-xs leading-relaxed text-faint">{locationStatus === "saved" && setup.network.location ? t("setupWizard.locationSaved", { latitude: setup.network.location.latitude, longitude: setup.network.location.longitude }) : t(`setupWizard.location${locationStatus[0].toUpperCase()}${locationStatus.slice(1)}`)}</span></span></div></div></Panel><p className="mt-4 flex items-center gap-2 text-xs text-faint"><LocateFixed className="h-3.5 w-3.5" />{t("setupWizard.locationPrivacy")}</p></Intro>;
}

export function NetworkScreen({ setup, updateNetwork, t }: { setup: SetupState; updateNetwork: (patch: Partial<SetupState["network"]>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const publicNetwork = setup.network.visibility === "public";
  return <Intro eyebrow={t("setupWizard.stepNetwork")} title={t("setupWizard.networkTitle")} detail={t("setupWizard.networkDetail")}><Panel><PanelHeader title={t("setupWizard.networkIdentity")} subtitle={t("setupWizard.networkIdentityDetail")} /><div className="grid gap-4 px-5 py-5 md:grid-cols-2"><Field label={t("setupWizard.publicName")} hint={t("setupWizard.publicNameHint")}><Input autoFocus value={setup.network.public_name} onChange={(event) => updateNetwork({ public_name: event.target.value })} /></Field><Field label={t("setupWizard.organization")}><Input value={setup.network.organization} onChange={(event) => updateNetwork({ organization: event.target.value })} /></Field><Field label={t("setupWizard.visibility")} hint={t("setupWizard.visibilityHint")}><Select value={setup.network.visibility} onChange={(event) => updateNetwork({ visibility: event.target.value as NetworkVisibility, access_policy: event.target.value === "public" ? "anyone_with_anchor" : setup.network.access_policy })}><option value="private">{t("setupWizard.privateInternet")}</option><option value="public">{t("setupWizard.publicInternet")}</option><option value="local">{t("setupWizard.localOnly")}</option></Select></Field><Field label={t("setupWizard.accessPolicy")} hint={t("setupWizard.accessPolicyHint")}><Select value={setup.network.access_policy} onChange={(event) => updateNetwork({ access_policy: event.target.value as AccessPolicy })}><option value="authorized">{t("setupWizard.authorizedPeople")}</option><option value="anyone_with_anchor">{t("setupWizard.anyoneWithAnchor")}</option><option value="local_only">{t("setupWizard.localOnly")}</option></Select></Field>{publicNetwork && <div className="md:col-span-2 rounded-lg border border-aqua/25 bg-aqua/7 px-3 py-3 text-sm text-muted"><p>{t("setupWizard.publicGeoRequired")}</p></div>}</div></Panel></Intro>;
}

export function TrustScreen({ setup, update, updateNetwork, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; updateNetwork?: (patch: Partial<SetupState["network"]>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const [fingerprintPending, setFingerprintPending] = useState(false);
  const [trustFileName, setTrustFileName] = useState("");
  const [uploading, setUploading] = useState(false);
  useEffect(() => {
    let cancelled = false;
    if (!setup.trustDocument.trim()) {
      update({ trustFingerprint: "" });
      return;
    }
    setFingerprintPending(true);
    void fingerprintFromTrustDocument(setup.trustDocument).then((fingerprint) => {
      if (cancelled) return;
      update({ trustFingerprint: fingerprint ?? "" });
      setFingerprintPending(false);
    });
    return () => {
      cancelled = true;
    };
  }, [setup.trustDocument]);

  async function readTrustFile(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    event.target.value = "";
    if (!file) return;
    setUploading(true);
    try {
      const document = await readTextFile(file);
      update({ trustDocument: document });
      applyTrustMetadata(document);
      setTrustFileName(file.name);
    } finally {
      setUploading(false);
    }
  }

  function applyTrustMetadata(document: string) {
    const metadata = trustNetworkMetadata(document);
    if (!metadata) return;
    updateNetwork?.({
      public_name: metadata.public_name ?? "",
      organization: metadata.organization ?? "",
      visibility: metadata.visibility ?? "private",
      access_policy: metadata.access_policy ?? "anyone_with_anchor",
      country_code: metadata.country_code ?? "",
      country_name: metadata.country_name ?? "",
      city: metadata.city ?? "",
      geo_label: metadata.geo_label ?? "",
    });
    update({
      bootstrap: metadata.bootstrap_address ?? metadata.advertised_address ?? "",
      publicDirectory: metadata.directory_replicas?.join(", ") ?? "",
    });
  }

  return <Intro eyebrow={t("setupWizard.stepTrust")} title={t("setupWizard.trustTitle")} detail={t("setupWizard.trustJoinDetail")}><Panel><PanelHeader title={t("setupWizard.pasteTrust")} subtitle={t("setupWizard.pasteTrustDetail")} /><div className="space-y-4 px-5 py-5"><Field label={t("setupWizard.trustDocument")} hint={t("setupWizard.trustDocumentHint")}><textarea data-testid="trust-document-textarea" autoFocus value={setup.trustDocument} onChange={(event) => { update({ trustDocument: event.target.value }); applyTrustMetadata(event.target.value); }} className="min-h-44 w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 font-mono text-xs text-fg outline-none transition-colors focus:border-iris focus:ring-2 focus:ring-iris/25" /><div className="mt-2 flex flex-wrap items-center gap-3"><label className="inline-flex cursor-pointer items-center gap-2 rounded-lg border border-line bg-raised px-3 py-2 text-xs font-medium text-fg transition-colors hover:border-line-strong"><Upload className="h-3.5 w-3.5 text-iris" /><span>{uploading ? t("setupWizard.uploadingTrust") : t("setupWizard.uploadTrust")}</span><input data-testid="trust-document-file" type="file" accept=".json,application/json" className="sr-only" onChange={(event) => void readTrustFile(event)} /></label>{trustFileName && <span className="max-w-full truncate text-xs text-muted">{trustFileName}</span>}</div></Field><div className="rounded-lg border border-line bg-raised px-3 py-3"><p className="text-xs font-medium uppercase tracking-[0.14em] text-faint">{t("setupWizard.fingerprintCalculated")}</p>{fingerprintPending ? <p className="mt-2 text-sm text-muted">{t("setupWizard.fingerprintWaiting")}</p> : setup.trustFingerprint ? <code className="mt-2 block break-words font-mono text-sm leading-6 text-fg">{groupFingerprint(setup.trustFingerprint)}</code> : <p className="mt-2 text-sm text-muted">{t("setupWizard.fingerprintWaiting")}</p>}</div><Field label={t("setupWizard.connectionSecret")} hint={t("setupWizard.connectionSecretHint")}><Input data-testid="connection-key-input" type="password" value={setup.connectionKey} onChange={(event) => update({ connectionKey: event.target.value })} /></Field></div></Panel></Intro>;
}

function readTextFile(file: File): Promise<string> {
  if (typeof file.text === "function") return file.text();
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result ?? ""));
    reader.onerror = () => reject(reader.error ?? new Error("could not read file"));
    reader.readAsText(file);
  });
}

export function InviteScreen({ anchor, visibility, connectionKey, t }: { anchor: AnchorPreview | null; visibility: NetworkVisibility; connectionKey: string; t: (key: string, values?: Record<string, string | number>) => string }) {
  if (!anchor) return <Intro eyebrow={t("setupWizard.stepInvite")} title={t("setupWizard.inviteTitle")} detail={t("setupWizard.anchorFailed")} />;
  return <Intro eyebrow={t("setupWizard.stepInvite")} title={t("setupWizard.inviteTitle")} detail={t("setupWizard.inviteDetail")}><div className="space-y-4">{visibility !== "public" && connectionKey && <CopyBlock featured label={t("setupWizard.connectionSecret")} subtitle={t("setupWizard.connectionSecretHint")} value={connectionKey} copyLabel={t("setupWizard.copy")} copied={t("setupWizard.copied")} /> }<CopyBlock collapsed expandLabel={t("setupWizard.showContents")} collapseLabel={t("setupWizard.hideContents")} label={t("setupWizard.trustDocument")} value={anchor.document} copyLabel={t("setupWizard.copy")} copied={t("setupWizard.copied")} downloadLabel={t("setupWizard.download")} downloadName={anchor.trust_filename} /><div className="rounded-xl border border-aqua/35 bg-aqua/8 px-4 py-4"><p className="text-xs font-medium uppercase tracking-[0.14em] text-aqua">{t("setupWizard.fingerprint")}</p><code className="mt-2 block break-words font-mono text-sm leading-6 text-fg">{anchor.fingerprint}</code><p className="mt-2 text-xs leading-relaxed text-muted">{t("setupWizard.fingerprintInvite")}</p></div><p className="text-xs text-faint">{t("setupWizard.trustFilename", { filename: anchor.trust_filename })}</p></div></Intro>;
}

export function AutoDiscoveryScreen({ setup, update, updateNetwork, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; updateNetwork: (patch: Partial<SetupState["network"]>) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const toast = useToast();
  const [status, setStatus] = useState<AutoDiscoveryStatus | null>(null);
  const started = useRef(false);
  const isAdmin = setup.profile === "admin";

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    let cancelled = false;
    let timer: number | undefined;
    async function start() {
      try {
        if (isAdmin) {
          await api.post("/api/setup/auto-discovery/start", { trust_document: setup.trustDocument, connection_key: setup.connectionKey }, { auth: false });
        } else {
          await api.post("/api/setup/auto-discovery/search", {}, { auth: false });
        }
        const poll = async () => {
          if (cancelled) return;
          try {
            const next = await api.get<AutoDiscoveryStatus>("/api/setup/auto-discovery/status", { auth: false });
            if (cancelled) return;
            setStatus(next);
            if (next.result) {
              update({ trustDocument: next.result.trust_document, trustFingerprint: next.result.trust_fingerprint, connectionKey: next.result.connection_key });
              applyTrustMetadata(next.result.trust_document);
            }
            if ((isAdmin && next.active) || (!isAdmin && !next.found && next.searching)) {
              timer = window.setTimeout(() => void poll(), 700);
            }
          } catch (error) {
            if (!cancelled) toast.fromError(t("setupWizard.discoveryFailed"), error);
          }
        };
        await poll();
      } catch (error) {
        if (!cancelled) toast.fromError(t("setupWizard.discoveryFailed"), error);
      }
    }
    void start();
    return () => { cancelled = true; if (timer) window.clearTimeout(timer); };

    function applyTrustMetadata(document: string) {
      const metadata = trustNetworkMetadata(document);
      if (!metadata) return;
      updateNetwork({ public_name: metadata.public_name ?? "", organization: metadata.organization ?? "", visibility: metadata.visibility ?? "private", access_policy: metadata.access_policy ?? "anyone_with_anchor", country_code: metadata.country_code ?? "", country_name: metadata.country_name ?? "", city: metadata.city ?? "", geo_label: metadata.geo_label ?? "" });
      update({ bootstrap: metadata.bootstrap_address ?? metadata.advertised_address ?? "", publicDirectory: metadata.directory_replicas?.join(", ") ?? "" });
    }
  }, [isAdmin, setup.connectionKey, setup.trustDocument, t, toast, update, updateNetwork]);

  const peerCount = status?.peers.length ?? 0;
  const found = isAdmin ? peerCount > 0 : Boolean(status?.found);
  return <Intro eyebrow={t("setupWizard.stepDiscovery")} title={t(isAdmin ? "setupWizard.discoveryAdminTitle" : "setupWizard.discoveryNodeTitle")} detail={t(isAdmin ? "setupWizard.discoveryAdminDetail" : "setupWizard.discoveryNodeDetail")}><Panel><PanelHeader title={t("setupWizard.discoveryPanelTitle")} subtitle={t("setupWizard.discoveryPanelDetail")} /><div className="space-y-4 px-5 py-5"><div className={`rounded-xl border px-4 py-4 ${found ? "border-aqua/35 bg-aqua/8" : "border-line bg-raised"}`}><p className="text-sm font-medium text-fg">{found ? t("setupWizard.discoveryFound") : isAdmin ? t("setupWizard.discoveryWaiting") : status?.searching ? t("setupWizard.discoverySearching") : t("setupWizard.discoveryManualFallback")}</p><p className="mt-2 text-xs leading-relaxed text-muted">{isAdmin ? t("setupWizard.discoveryAdminSecurity") : t("setupWizard.discoveryNodeSecurity")}</p></div>{isAdmin && <div className="rounded-xl border border-line bg-surface px-4 py-4"><div className="flex items-center justify-between gap-3"><div><p className="text-sm font-medium text-fg">{t("setupWizard.discoveryPeerCount", { count: peerCount })}</p><p className="mt-1 text-xs text-muted">{t("setupWizard.discoveryPeerCountDetail")}</p></div><Badge tone={peerCount > 0 ? "aqua" : "neutral"}>{peerCount}</Badge></div>{peerCount > 0 && <ul className="mt-4 space-y-2 border-t border-line pt-3">{status?.peers.map((peer) => <li key={peer.request_id} className="flex min-w-0 items-center gap-3 rounded-lg border border-line bg-raised px-3 py-2.5"><Server className="h-4 w-4 shrink-0 text-aqua" /><span className="min-w-0 flex-1 truncate font-mono text-xs text-fg">{peer.address}</span><span className="shrink-0 text-[0.68rem] font-medium uppercase tracking-[0.12em] text-aqua">{t("setupWizard.discoveryPeerHandshake")}</span></li>)}</ul>}</div>}{!isAdmin && <p className="text-xs text-faint">{t("setupWizard.discoveryManualHint")}</p>}</div></Panel></Intro>;
}

export function ReviewScreen({ setup, health, hardware, setStage, t }: { setup: SetupState; health: HealthResponse | null; hardware: HardwareResponse | null; setStage: (stage: WizardStage) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const hasAmd = hardware?.system.gpus.some((gpu) => gpu.vendor.toLowerCase() === "amd") ?? false;
  const acceleratorCheck = hardware?.system.os === "macos" || !hardware?.gpu_detected ? null : hasAmd
    ? { id: "vulkan", ok: Boolean(health?.vulkan_working), label: t("setupWizard.checkVulkan") }
    : { id: "cuda", ok: Boolean(health?.cuda_working), label: t("setupWizard.checkCuda") };
  const checks = health ? [{ id: "gpu", ok: health.gpu_detected, label: health.gpu_detected ? t("setupWizard.checkGpu") : t("setupWizard.gpuUnavailable") }, ...(acceleratorCheck ? [acceleratorCheck] : []), { id: "config", ok: health.configuration_ready, label: t("setupWizard.checkConfig") }, { id: "p2p", ok: health.p2p_discovery_working, label: t("setupWizard.checkP2p") }, { id: "ipv6", ok: health.public_ipv6_enabled, label: t("setupWizard.checkIpv6") }, { id: "firewall", ok: health.firewall_allowed, label: health.firewall_supported ? t("setupWizard.checkFirewall") : t("setupWizard.checkFirewallLocal") }] : [];
  return <Intro eyebrow={t("setupWizard.stepReview")} title={t("setupWizard.reviewTitle")} detail={t("setupWizard.reviewDetail")}><div className="space-y-4"><Panel><PanelHeader title={t("setupWizard.healthTitle")} subtitle={t("setupWizard.healthDetail")} /><div className="divide-y divide-line">{checks.map((check) => <div key={check.id} className="flex items-center justify-between gap-4 px-5 py-3"><span className="text-sm text-fg">{check.label}</span><StatusBadge ok={check.ok} label={check.ok ? t("setupWizard.ok") : t("setupWizard.notAvailable")} /></div>)}</div><p className="px-5 py-3 text-xs text-faint">{t("setupWizard.healthModelLater")}</p></Panel><Panel><PanelHeader title={t("setupWizard.reviewTitle")} /><SummaryRow label={t("setupWizard.profileLabel")} value={setup.profile ? t(`setupWizard.${setup.profile}`) : "-"} onEdit={() => setStage("setup-profile")} />{setup.profile === "admin" && <><SummaryRow label={t("setupWizard.storageLabel")} value={`${setup.shardMaxGb} GB`} onEdit={() => setStage("setup-profile")} /><SummaryRow label={t("setupWizard.networkLabel")} value={setup.network.public_name || "-"} onEdit={() => setStage("setup-network")} /></>}{setup.profile !== "admin" && <SummaryRow label={t("setupWizard.trustLabel")} value={setup.trustFingerprint ? t("setupWizard.fingerprintEntered") : t("setupWizard.fingerprintMissing")} onEdit={() => setStage("setup-trust")} />}</Panel></div></Intro>;
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

function CopyBlock({ label, value, copyLabel, copied, downloadLabel, downloadName, collapsed = false, expandLabel, collapseLabel, featured = false, subtitle }: { label: string; value: string; copyLabel: string; copied: string; downloadLabel?: string; downloadName?: string; collapsed?: boolean; expandLabel?: string; collapseLabel?: string; featured?: boolean; subtitle?: string }) {
  const [isCopied, setIsCopied] = useState(false);
  const [isExpanded, setIsExpanded] = useState(false);
  async function copy() { try { await navigator.clipboard.writeText(value); setIsCopied(true); window.setTimeout(() => setIsCopied(false), 1800); } catch { /* o conteúdo continua selecionável */ } }
  function download() {
    if (!downloadName) return;
    const url = URL.createObjectURL(new Blob([value], { type: "application/json;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = downloadName;
    link.click();
    URL.revokeObjectURL(url);
  }
  const content = <pre className={featured ? "max-h-48 overflow-auto whitespace-pre-wrap break-words bg-ink/35 px-5 py-5 font-mono text-base font-semibold leading-7 text-fg selection:bg-aqua/25" : "max-h-72 overflow-auto whitespace-pre-wrap break-words px-5 py-4 font-mono text-[0.7rem] leading-5 text-muted"}>{value}</pre>;
  return <Panel className={featured ? "border-aqua/60 bg-aqua/8 shadow-lg" : undefined}><PanelHeader className={featured ? "border-aqua/30" : undefined} title={label} subtitle={subtitle} actions={<div className="flex flex-wrap gap-2"><Button variant={featured ? "primary" : "outline"} size="sm" onClick={() => void copy()} icon={isCopied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}>{isCopied ? copied : copyLabel}</Button>{downloadName && downloadLabel && <Button variant="outline" size="sm" onClick={download} icon={<Download className="h-3.5 w-3.5" />}>{downloadLabel}</Button>}</div>} />{collapsed ? <details><summary onClick={() => setIsExpanded((value) => !value)} className="flex cursor-pointer list-none items-center justify-between gap-3 border-t border-line px-5 py-3 text-sm font-medium text-fg outline-none transition-colors hover:text-aqua focus-visible:ring-2 focus-visible:ring-iris/50 [&::-webkit-details-marker]:hidden"><span>{isExpanded ? collapseLabel : expandLabel}</span><ChevronDown className={`h-4 w-4 shrink-0 text-muted transition-transform ${isExpanded ? "rotate-180" : ""}`} /></summary>{content}</details> : content}</Panel>;
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

export function isReadyToSave(setup: SetupState): boolean {
  if (!setup.profile || !setup.trustDocument.trim() || !setup.trustFingerprint.trim()) return false;
  if (setup.profile !== "admin" && setup.network.visibility !== "public" && !setup.connectionKey.trim()) return false;
  if (setup.profile === "admin" && setup.network.visibility === "public" && (!setup.network.country_code.trim() || !setup.network.city.trim())) return false;
  if (setup.profile === "admin" && (!setup.network.public_name.trim() || !setup.advertiseAddr.trim() || (setup.adminExposure === "network" && !setup.adminAccessToken.trim()))) return false;
  return true;
}

export function canContinue(stage: WizardStage, setup: SetupState): boolean {
  if (stage === "setup-profile") {
    return Boolean(setup.profile && (setup.profile !== "admin" || setup.advertiseAddr.trim()));
  }
  if (stage === "setup-network" && setup.profile === "admin") {
    return Boolean(setup.network.public_name.trim() && setup.advertiseAddr.trim() && (setup.network.visibility !== "public" || (setup.network.country_code.trim() && setup.network.city.trim())));
  }
  if (stage === "setup-trust") {
    return Boolean(setup.trustFingerprint.trim() && (setup.network.visibility === "public" || setup.connectionKey.trim()));
  }
  return true;
}
