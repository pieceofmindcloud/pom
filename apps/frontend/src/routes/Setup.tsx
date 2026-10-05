/**
 * Primeiro acesso real.
 *
 * Pergunta só o que não dá para decidir depois (ver `setupWizardFlow.ts`):
 * o papel da máquina e o nome dela; para quem cria a rede, o nome da rede e se
 * ela é privada ou pública (a cidade só quando é pública, para o pkarr); para
 * quem entra, a raiz e a chave - de preferência trazidas pela descoberta local.
 * Recursos, pastas, disco, senha e exposição têm padrões e são ajustados depois,
 * dentro do POM. A casca usa a identidade da página inicial do site, a mesma do
 * portão de acesso (`.pom-login*` em `index.css`).
 */
import { useEffect, useMemo, useRef, useState, type ChangeEvent, type ReactNode } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { ArrowLeft, Check, ChevronDown, Copy, Cpu, Database, Download, Globe2, HardDrive, Landmark, MessageCircle, Monitor, Radar, RefreshCw, Server, Sparkles, Upload } from "lucide-react";
import clsx from "clsx";
import type { WizardStage, WizardStep } from "./setupWizardTypes";
import { setupNextStage, setupPreviousStage, setupStepsFor, type SetupProfile } from "./setupWizardFlow";
import { api } from "../lib/api";
import type { EnrollmentRole } from "../lib/types";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { Field, Input, Select } from "../components/ui";
import { BrandLanguageSwitch } from "../components/BrandLanguageSwitch";
import { LobbyChat } from "../components/LobbyChat";
import { fingerprintFromTrustDocument, groupFingerprint } from "../lib/trustFingerprint";
import pomMark from "../assets/pom-mark-dark.png";

type NetworkVisibility = "local" | "private" | "public";
type AccessPolicy = "authorized" | "anyone_with_anchor" | "local_only";
type Translate = (key: string, values?: Record<string, string | number>) => string;

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
  p2p_port: number;
  thermal_limit_c: number;
  default_model_dir: string;
  default_shard_dir: string;
  shard_max_gb: number;
  ram_reserve_pct: number;
  cpu_reserve_pct: number;
  node_name: string | null;
  network: SetupState["network"];
  public_directory: string[];
}

interface GeoCityReference {
  id: string;
  name: string;
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
  trust_document?: string;
  access_policy?: AccessPolicy;
  accepted_enrollments?: EnrollmentRole[];
  directory_replicas?: string[];
}

interface AnchorPreview {
  cluster_id: string;
  document: string;
  fingerprint: string;
  trust_filename: string;
  private_connection_key?: string;
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
  /** Chaves `pk:` dos relés dedicados. O documento não carrega IP de ninguém. */
  relay_hints?: string[];
  directory_replicas?: string[];
}

/** O que um computador informa de si ao procurar a rede local. */
export interface DiscoveredMachine {
  hostname: string;
  os?: string;
  arch?: string;
  cpu_count?: number;
  ram_mb?: number;
  gpus?: Array<{ name: string; vram_mb?: number | null }>;
}

export interface AutoDiscoveryPeer {
  request_id: string;
  address: string;
  machine?: DiscoveredMachine | null;
  joined?: boolean;
}

interface AutoDiscoveryResult {
  request_id: string;
  trust_document: string;
  trust_fingerprint: string;
  connection_key: string;
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
  /**
   * Roles the public network picked in the directory advertises as open, or
   * `null` when the wizard was not opened from there. Only narrows the role
   * choice on the first screen; it is not sent to the node.
   */
  joinAccepts: EnrollmentRole[] | null;
  publicDirectory: string;
  connectionKey: string;
  advertiseAddr: string;
  trustDocument: string;
  trustFingerprint: string;
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

/** O IP de um endereço `host:porta`, sem colchetes nem porta. */
export function addressHost(address: string): string {
  const bracketed = address.match(/^\[([^\]]+)\](?::\d+)?$/);
  if (bracketed) return bracketed[1];
  const lastColon = address.lastIndexOf(":");
  return lastColon > 0 && address.indexOf(":") === lastColon ? address.slice(0, lastColon) : address;
}

function gigabytes(mb: number | null | undefined): string | null {
  if (typeof mb !== "number" || !Number.isFinite(mb) || mb <= 0) return null;
  return `${(mb / 1024).toFixed(mb >= 10 * 1024 ? 0 : 1)} GB`;
}

const INITIAL: SetupState = {
  profile: null,
  joinAccepts: null,
  publicDirectory: "",
  connectionKey: "",
  advertiseAddr: "",
  trustDocument: "",
  trustFingerprint: "",
  nodeName: "",
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
};

export function Setup() {
  const { t } = useI18n();
  const toast = useToast();
  const [setup, setSetup] = useState<SetupState>(INITIAL);
  const [stage, setStage] = useState<WizardStage>("setup-profile");
  const [hardware, setHardware] = useState<HardwareResponse | null>(null);
  const [geoReference, setGeoReference] = useState<GeoReferenceResponse | null>(null);
  const [anchor, setAnchor] = useState<AnchorPreview | null>(null);
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
          // Pastas, disco, reservas e limite térmico ficam nos padrões da
          // máquina; todos são ajustáveis depois, dentro do POM.
          modelDirs: [detected.default_model_dir],
          shardDir: detected.default_shard_dir,
          shardMaxGb: Math.min(detected.shard_max_gb || 20, Math.max(0, detected.system.disk.free_mb / 1024)),
          ramReservePct: detected.ram_reserve_pct ?? current.ramReservePct,
          cpuReservePct: detected.cpu_reserve_pct ?? current.cpuReservePct,
          thermalLimitC: detected.thermal_limit_c || 85,
          nodeName: current.nodeName.trim() || detected.node_name?.trim() || detected.system.hostname,
          network: {
            ...current.network,
            ...detected.network,
            public_name: current.network.public_name.trim() || detected.network.public_name.trim() || createRandomNetworkName(),
            visibility: detected.network.visibility === "public" ? "public" : "private",
            access_policy: "anyone_with_anchor",
          },
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
        joinAccepts: network.accepted_enrollments ?? null,
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
    const clusterId = setup.network.public_name.trim();
    if (setup.profile !== "admin" || !clusterId) return;
    setBusy(true);
    try {
      const preview = await api.post<AnchorPreview>(
        "/api/setup/admin-anchor/preview",
        {
          cluster_id: clusterId,
          visibility: setup.network.visibility,
          network: setup.network,
          public_directory: splitList(setup.publicDirectory),
        },
        { auth: false },
      );
      setAnchor(preview);
      update({
        trustDocument: preview.document,
        trustFingerprint: preview.fingerprint,
        connectionKey: setup.network.visibility === "public" ? "" : preview.private_connection_key ?? "",
      });
      setStage("setup-invite");
    } catch (error) {
      toast.fromError(t("setupWizard.anchorFailed"), error);
    } finally {
      setBusy(false);
    }
  }

  async function save() {
    setBusy(true);
    try {
      await api.post("/api/setup/save", {
        profile: setup.profile,
        public_directory: splitList(setup.publicDirectory),
        connection_key: setup.connectionKey.trim(),
        advertise_addr: setup.advertiseAddr.trim(),
        cluster_id: clusterIdFromDocument(setup.trustDocument) || setup.network.public_name.trim(),
        trust_document: setup.trustDocument.trim(),
        trust_fingerprint: setup.trustFingerprint.trim(),
        network: setup.network,
        resources: { ram_reserve_pct: setup.ramReservePct, cpu_reserve_pct: setup.cpuReservePct },
        thermal_limit_c: setup.thermalLimitC,
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
    if (!canContinue(stageNow, setup)) return;
    if (stageNow === "setup-network") {
      void prepareAnchor();
      return;
    }
    if (stageNow === "setup-invite" || stageNow === "setup-join") {
      void save();
      return;
    }
    const next = setupNextStage(stageNow, steps);
    if (next) setStage(next);
  }

  function back() {
    const previous = setupPreviousStage(stage, steps);
    if (previous) setStage(previous);
  }

  const finishing = stage === "setup-invite" || stage === "setup-join";
  return (
    <FirstRunShell
      steps={steps}
      stage={stage}
      canGoBack={stage !== "setup-profile" && stage !== "setup-saved" && !busy}
      onBack={back}
      action={stage === "setup-saved" || loading ? null : (
        <button type="button" className="pom-login-button" disabled={busy || !canContinue(stage, setup)} onClick={() => continueFrom(stage)}>
          {busy && <span className="pom-login-spinner" aria-hidden="true" />}
          {finishing ? t("firstRun.finish") : t("wizard.continue")}
        </button>
      )}
      t={t}
    >
      {loading ? <LoadingScreen /> : (
        <Screen stage={stage} setup={setup} hardware={hardware} geoReference={geoReference} anchor={anchor} update={update} updateNetwork={updateNetwork} t={t} />
      )}
    </FirstRunShell>
  );
}

function splitList(value: string): string[] {
  return value.split(",").map((item) => item.trim()).filter(Boolean);
}

/** Casca do wizard com a identidade do site: painel escuro flutuante, marca e trilha de passos. */
function FirstRunShell({ steps, stage, canGoBack, onBack, action, children, t }: { steps: WizardStep[]; stage: WizardStage; canGoBack: boolean; onBack: () => void; action: ReactNode; children: ReactNode; t: Translate }) {
  return (
    <main className="pom-login pom-setup">
      <BrandLanguageSwitch />
      <section className="pom-login-panel pom-setup-panel pom-setup-scope">
        <header className="pom-setup-brand">
          <img className="pom-login-mark pom-setup-mark" src={pomMark} alt="POM" width={815} height={320} />
          <p className="pom-login-sub pom-setup-sub">PIECE OF MIND</p>
          {stage !== "setup-saved" && <StepTrail steps={steps} stage={stage} t={t} />}
        </header>
        <AnimatePresence mode="wait" initial={false}>
          <motion.div
            key={stage}
            initial={{ opacity: 0, y: 12, filter: "blur(4px)" }}
            animate={{ opacity: 1, y: 0, filter: "blur(0px)" }}
            exit={{ opacity: 0, y: -8, filter: "blur(4px)" }}
            transition={{ duration: 0.32, ease: [0.22, 1, 0.36, 1] }}
            className="pom-setup-body"
          >
            {children}
          </motion.div>
        </AnimatePresence>
        {action && (
          <footer className="pom-setup-footer">
            <button type="button" className={clsx("pom-setup-ghost", !canGoBack && "invisible")} onClick={onBack} disabled={!canGoBack} aria-hidden={!canGoBack}>
              <ArrowLeft className="h-4 w-4" />
              {t("wizard.back")}
            </button>
            {action}
          </footer>
        )}
      </section>
    </main>
  );
}

/** Os passos como os nós ligados do site: o pulso viaja entre eles e o atual acende. */
function StepTrail({ steps, stage, t }: { steps: WizardStep[]; stage: WizardStage; t: Translate }) {
  const active = Math.max(0, steps.findIndex((step) => step.id === stage));
  return (
    <nav aria-label={t("wizard.progressLabel")} className="pom-setup-trail">
      <ol>
        {steps.map((step, index) => (
          <li key={step.id} className={clsx(index < active && "is-done", index === active && "is-active")} aria-current={index === active ? "step" : undefined}>
            <i />
            <span>{t(step.labelKey)}</span>
          </li>
        ))}
      </ol>
    </nav>
  );
}

function Screen({ stage, setup, hardware, geoReference, anchor, update, updateNetwork, t }: {
  stage: WizardStage;
  setup: SetupState;
  hardware: HardwareResponse | null;
  geoReference: GeoReferenceResponse | null;
  anchor: AnchorPreview | null;
  update: (patch: Partial<SetupState>) => void;
  updateNetwork: (patch: Partial<SetupState["network"]>) => void;
  t: Translate;
}) {
  switch (stage) {
    case "setup-profile": return <StartScreen setup={setup} hardware={hardware} update={update} t={t} />;
    case "setup-network": return <NetworkScreen setup={setup} hardware={hardware} countries={geoReference?.countries ?? []} update={update} updateNetwork={updateNetwork} t={t} />;
    case "setup-invite": return <InviteScreen setup={setup} anchor={anchor} t={t} />;
    case "setup-join": return <JoinScreen setup={setup} update={update} updateNetwork={updateNetwork} t={t} />;
    case "setup-saved": return <SavedScreen t={t} />;
    default: return null;
  }
}

function Heading({ title, detail }: { title: string; detail?: string }) {
  return (
    <div className="mb-6">
      <h1 className="font-display text-2xl font-bold tracking-tight text-fg sm:text-[1.7rem]">{title}</h1>
      {detail && <p className="mt-2 max-w-xl text-sm leading-relaxed text-muted">{detail}</p>}
    </div>
  );
}

export function StartScreen({ setup, hardware, update, t }: { setup: SetupState; hardware: HardwareResponse | null; update: (patch: Partial<SetupState>) => void; t: Translate }) {
  const roles: { id: SetupProfile; icon: ReactNode; title: string; detail: string }[] = [
    { id: "admin", icon: <Landmark className="h-5 w-5" />, title: t("setupWizard.admin"), detail: t("firstRun.adminDetail") },
    { id: "node", icon: <Server className="h-5 w-5" />, title: t("setupWizard.node"), detail: t("firstRun.nodeDetail") },
    { id: "infer", icon: <Sparkles className="h-5 w-5" />, title: t("setupWizard.infer"), detail: t("firstRun.inferDetail") },
  ];
  const gpu = hardware?.system.gpus[0];
  const hasNvidia = hardware?.system.gpus.some((candidate) => candidate.vendor.toLowerCase() === "nvidia") ?? false;
  return (
    <>
      <Heading title={t("firstRun.startTitle")} detail={t("firstRun.startDetail")} />
      <div className="grid gap-3 sm:grid-cols-3" role="radiogroup" aria-label={t("firstRun.startTitle")}>
        {roles.map((role, index) => {
          const selected = setup.profile === role.id;
          // `admin` founds a network of its own, so the joined one never closes it.
          const notAccepted = role.id !== "admin" && setup.joinAccepts !== null && !setup.joinAccepts.includes(role.id);
          return (
            <motion.button
              key={role.id}
              type="button"
              role="radio"
              aria-checked={selected}
              disabled={notAccepted}
              title={notAccepted ? t("firstRun.roleNotAccepted") : undefined}
              onClick={() => update({ profile: role.id })}
              initial={{ opacity: 0, y: 10 }}
              animate={{ opacity: 1, y: 0 }}
              transition={{ delay: 0.08 * index, duration: 0.35, ease: [0.22, 1, 0.36, 1] }}
              className={clsx("pom-setup-choice", selected && "is-selected", notAccepted && "cursor-not-allowed opacity-45")}
            >
              <span className="pom-setup-choice-icon">{role.icon}</span>
              <span className="mt-3 block font-display text-[0.95rem] font-semibold text-fg">{role.title}</span>
              <span className="mt-1.5 block text-xs leading-relaxed text-muted">{role.detail}</span>
            </motion.button>
          );
        })}
      </div>
      <div className="mt-6">
        <Field label={t("setupWizard.nodeName")} hint={t("firstRun.nodeNameHint")}>
          <Input value={setup.nodeName} onChange={(event) => update({ nodeName: event.target.value })} />
        </Field>
      </div>
      {hardware && (
        <div className="mt-5 flex flex-wrap gap-2" data-testid="hardware-summary">
          <Chip icon={<Cpu className="h-3.5 w-3.5" />}>{t("setupWizard.cpuValue", { count: hardware.system.cpu_count })}</Chip>
          <Chip icon={<Database className="h-3.5 w-3.5" />}>{`${(hardware.system.ram.total_mb / 1024).toFixed(1)} GB RAM`}</Chip>
          <Chip icon={<Monitor className="h-3.5 w-3.5" />}>{gpu ? `${gpu.name}${gpu.vram_total_mb ? ` · ${gigabytes(gpu.vram_total_mb)}` : ""}` : t("setupWizard.noGpu")}</Chip>
          <Chip icon={<HardDrive className="h-3.5 w-3.5" />}>{hardware.backend}</Chip>
        </div>
      )}
      {hardware && hasNvidia && !hardware.cuda_available && <p className="mt-3 text-xs leading-relaxed text-amber">{t("setupWizard.cudaInstallHint")}</p>}
    </>
  );
}

function Chip({ icon, children }: { icon: ReactNode; children: ReactNode }) {
  return <span className="pom-setup-chip">{icon}<span className="truncate">{children}</span></span>;
}

export function NetworkScreen({ setup, hardware, countries, update, updateNetwork, t }: { setup: SetupState; hardware: HardwareResponse | null; countries: GeoCountryReference[]; update: (patch: Partial<SetupState>) => void; updateNetwork: (patch: Partial<SetupState["network"]>) => void; t: Translate }) {
  const publicNetwork = setup.network.visibility === "public";
  const p2pPort = hardware?.p2p_port ?? 0;
  const selectedCountry = countries.find((country) => country.code === setup.network.country_code);
  const cityIsReference = selectedCountry?.cities.some((city) => city.name === setup.network.city) ?? false;
  const options: { id: "private" | "public"; title: string; detail: string }[] = [
    { id: "private", title: t("firstRun.private"), detail: t("firstRun.privateDetail") },
    { id: "public", title: t("firstRun.public"), detail: t("firstRun.publicDetail") },
  ];
  return (
    <>
      <Heading title={t("firstRun.networkTitle")} detail={t("firstRun.networkDetail")} />
      <Field label={t("setupWizard.publicName")} hint={t("firstRun.networkNameHint")}>
        <Input autoFocus value={setup.network.public_name} onChange={(event) => updateNetwork({ public_name: event.target.value })} />
      </Field>
      <div className="mt-5 grid gap-3 sm:grid-cols-2" role="radiogroup" aria-label={t("setupWizard.visibility")}>
        {options.map((option) => (
          <button key={option.id} type="button" role="radio" aria-checked={setup.network.visibility === option.id} onClick={() => updateNetwork({ visibility: option.id, access_policy: "anyone_with_anchor" })} className={clsx("pom-setup-choice", setup.network.visibility === option.id && "is-selected")}>
            <span className="block font-display text-[0.95rem] font-semibold text-fg">{option.title}</span>
            <span className="mt-1.5 block text-xs leading-relaxed text-muted">{option.detail}</span>
          </button>
        ))}
      </div>
      <AnimatePresence initial={false}>
        {publicNetwork && (
          <motion.div key="geo" initial={{ opacity: 0, height: 0 }} animate={{ opacity: 1, height: "auto" }} exit={{ opacity: 0, height: 0 }} className="overflow-hidden">
            <p className="mt-5 text-xs leading-relaxed text-muted">{t("firstRun.publicLocationDetail")}</p>
            <div className="mt-3 grid gap-4 sm:grid-cols-2">
              <Field label={t("setupWizard.country")}>
                <Select value={setup.network.country_code} onChange={(event) => { const country = countries.find((candidate) => candidate.code === event.target.value); updateNetwork({ country_code: country?.code ?? "", country_name: country?.name ?? "", city: "" }); }}>
                  <option value="">{t("setupWizard.chooseCountry")}</option>
                  {countries.map((country) => <option key={country.code} value={country.code}>{country.name}</option>)}
                </Select>
              </Field>
              <Field label={t("setupWizard.city")}>
                <Select value={cityIsReference ? setup.network.city : setup.network.city ? "__manual__" : ""} disabled={!selectedCountry} onChange={(event) => updateNetwork({ city: event.target.value === "__manual__" ? "" : event.target.value })}>
                  <option value="">{t("setupWizard.chooseCity")}</option>
                  {selectedCountry?.cities.map((city) => <option key={city.id} value={city.name}>{city.name}</option>)}
                  <option value="__manual__">{t("setupWizard.otherCity")}</option>
                </Select>
              </Field>
              {selectedCountry && !cityIsReference && (
                <Field label={t("setupWizard.manualCity")}>
                  <Input value={setup.network.city} onChange={(event) => updateNetwork({ city: event.target.value })} />
                </Field>
              )}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
      <details className="pom-setup-details mt-5">
        <summary>
          <span>{t("firstRun.advanced")}</span>
          <ChevronDown className="h-4 w-4" />
        </summary>
        <div className="pt-3">
          <Field label={t("setupWizard.advertiseAddress")} hint={t("setupWizard.advertiseAddressHint")}>
            <div className="flex items-center gap-2">
              <Input value={advertiseHost(setup.advertiseAddr)} placeholder={t("setupWizard.advertiseAddressPlaceholder")} onChange={(event) => update({ advertiseAddr: formatAdvertiseAddress(event.target.value, p2pPort) })} />
              <span className="shrink-0 font-mono text-sm text-muted">:{p2pPort}</span>
            </div>
          </Field>
        </div>
      </details>
    </>
  );
}

export function InviteScreen({ setup, anchor, t }: { setup: SetupState; anchor: AnchorPreview | null; t: Translate }) {
  if (!anchor) return <Heading title={t("firstRun.inviteTitle")} detail={t("setupWizard.anchorFailed")} />;
  const privateNetwork = setup.network.visibility !== "public";
  return (
    <>
      <Heading title={t("firstRun.inviteTitle")} detail={t(privateNetwork ? "firstRun.inviteDetailPrivate" : "firstRun.inviteDetailPublic")} />
      {privateNetwork && setup.connectionKey && (
        <SecretCard label={t("setupWizard.connectionSecret")} value={setup.connectionKey} t={t} />
      )}
      <TrustActions anchor={anchor} t={t} />
      <LocalDiscoveryPanel mode="admin" setup={setup} t={t} />
    </>
  );
}

function SecretCard({ label, value, t }: { label: string; value: string; t: Translate }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } catch {
      // O conteúdo continua selecionável.
    }
  }
  return (
    <div className="pom-setup-secret" data-testid="connection-secret">
      <div className="flex items-center justify-between gap-3">
        <p className="text-[0.68rem] font-semibold uppercase tracking-[0.16em] text-aqua">{label}</p>
        <button type="button" className="pom-setup-ghost pom-setup-ghost-sm" onClick={() => void copy()}>
          {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
          {copied ? t("setupWizard.copied") : t("setupWizard.copy")}
        </button>
      </div>
      <code className="mt-2 block break-all font-mono text-base font-semibold leading-7 text-fg">{value}</code>
    </div>
  );
}

function TrustActions({ anchor, t }: { anchor: AnchorPreview; t: Translate }) {
  const [copied, setCopied] = useState(false);
  function download() {
    const url = URL.createObjectURL(new Blob([anchor.document], { type: "application/json;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = anchor.trust_filename;
    link.click();
    URL.revokeObjectURL(url);
  }
  async function copy() {
    try {
      await navigator.clipboard.writeText(anchor.document);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } catch {
      // O download continua disponível.
    }
  }
  return (
    <div className="mt-3 flex flex-wrap items-center gap-2" data-testid="trust-actions">
      <button type="button" className="pom-setup-ghost" onClick={download}><Download className="h-4 w-4" />{t("firstRun.downloadTrust", { filename: anchor.trust_filename })}</button>
      <button type="button" className="pom-setup-ghost" onClick={() => void copy()}>{copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}{copied ? t("setupWizard.copied") : t("firstRun.copyTrust")}</button>
      <span className="min-w-0 truncate font-mono text-[0.68rem] text-faint" title={anchor.fingerprint}>{groupFingerprint(anchor.fingerprint)}</span>
    </div>
  );
}

/**
 * A descoberta na rede local, dos dois lados. O administrador espera e vê cada
 * computador que procura a rede (nome, sistema, IP, RAM, GPU) entrar com uma
 * animação; é opcional - concluir sem ninguém é normal. Quem entra procura o
 * administrador e recebe a raiz e a chave sem digitar nada.
 */
export function LocalDiscoveryPanel({ mode, setup, onResult, t }: { mode: "admin" | "join"; setup: SetupState; onResult?: (result: AutoDiscoveryResult) => void; t: Translate }) {
  const toast = useToast();
  const [status, setStatus] = useState<AutoDiscoveryStatus | null>(null);
  const [attempt, setAttempt] = useState(0);
  const onResultRef = useRef(onResult);
  onResultRef.current = onResult;
  const isAdmin = mode === "admin";

  useEffect(() => {
    let cancelled = false;
    let timer: number | undefined;
    async function start() {
      try {
        if (isAdmin) {
          await api.post("/api/setup/auto-discovery/start", { trust_document: setup.trustDocument, connection_key: setup.connectionKey }, { auth: false });
        } else {
          await api.post("/api/setup/auto-discovery/search", { node_name: setup.nodeName }, { auth: false });
        }
        const poll = async () => {
          if (cancelled) return;
          try {
            const next = await api.get<AutoDiscoveryStatus>("/api/setup/auto-discovery/status", { auth: false });
            if (cancelled) return;
            setStatus(next);
            if (next.result) onResultRef.current?.(next.result);
            if ((isAdmin && next.active) || (!isAdmin && !next.found && next.searching)) {
              timer = window.setTimeout(() => void poll(), 800);
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
    return () => {
      cancelled = true;
      if (timer) window.clearTimeout(timer);
    };
    // A procura recomeça só por pedido explícito (`attempt`); editar o nome
    // no meio não abre outra sessão.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isAdmin, attempt]);

  const peers = status?.peers ?? [];
  const found = Boolean(status?.found);
  const searching = !isAdmin && !found && (status == null || status.searching);
  return (
    <section className="pom-setup-discovery mt-6" data-testid="local-discovery">
      <div className="flex items-center gap-3">
        <span className={clsx("pom-setup-radar", (isAdmin || searching) && "is-live", found && "is-found")} aria-hidden="true"><Radar className="h-4 w-4" /></span>
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium text-fg">
            {isAdmin
              ? peers.length > 0 ? t("firstRun.discoveryPeers", { count: peers.length }) : t("firstRun.discoveryAdminWaiting")
              : found ? t("firstRun.joinFound") : searching ? t("firstRun.joinSearching") : t("firstRun.joinNotFound")}
          </p>
          <p className="mt-0.5 text-xs leading-relaxed text-muted">{isAdmin ? t("firstRun.discoveryAdminOptional") : found ? t("firstRun.joinFoundDetail") : t("firstRun.joinSearchingDetail")}</p>
        </div>
        {!isAdmin && !found && !searching && (
          <button type="button" className="pom-setup-ghost pom-setup-ghost-sm" onClick={() => setAttempt((value) => value + 1)}>
            <RefreshCw className="h-3.5 w-3.5" />{t("firstRun.searchAgain")}
          </button>
        )}
      </div>
      {isAdmin && peers.length > 0 && (
        <ul className="mt-4 grid gap-2.5 sm:grid-cols-2">
          <AnimatePresence initial={false}>
            {peers.map((peer) => <DiscoveredMachineCard key={peer.request_id} peer={peer} t={t} />)}
          </AnimatePresence>
        </ul>
      )}
    </section>
  );
}

export function DiscoveredMachineCard({ peer, t }: { peer: AutoDiscoveryPeer; t: Translate }) {
  const machine = peer.machine;
  const gpus = machine?.gpus ?? [];
  const system = [machine?.os, machine?.arch].filter(Boolean).join(" · ");
  return (
    <motion.li
      layout
      initial={{ opacity: 0, y: 14, scale: 0.94 }}
      animate={{ opacity: 1, y: 0, scale: 1 }}
      exit={{ opacity: 0, scale: 0.94 }}
      transition={{ duration: 0.45, ease: [0.22, 1, 0.36, 1] }}
      className={clsx("pom-setup-machine", peer.joined && "is-joined")}
      data-testid="discovered-machine"
    >
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <span className="pom-setup-choice-icon !h-9 !w-9"><Monitor className="h-4 w-4" /></span>
          <div className="min-w-0">
            <p className="truncate text-sm font-semibold text-fg">{machine?.hostname || addressHost(peer.address)}</p>
            <p className="truncate text-[0.7rem] text-muted">{system || t("firstRun.machineUnknownSystem")}</p>
          </div>
        </div>
        <span className={clsx("pom-setup-status", peer.joined ? "is-joined" : "is-joining")}>
          {peer.joined ? t("firstRun.machineJoined") : t("firstRun.machineJoining")}
        </span>
      </div>
      <dl className="mt-3 grid grid-cols-2 gap-x-3 gap-y-1.5 text-[0.7rem]">
        <dt className="text-faint">IP</dt><dd className="truncate text-right font-mono text-fg">{addressHost(peer.address)}</dd>
        {machine?.ram_mb ? <><dt className="text-faint">RAM</dt><dd className="text-right text-fg">{gigabytes(machine.ram_mb)}</dd></> : null}
        {machine?.cpu_count ? <><dt className="text-faint">CPU</dt><dd className="text-right text-fg">{t("setupWizard.cpuValue", { count: machine.cpu_count })}</dd></> : null}
        <dt className="text-faint">GPU</dt>
        <dd className="truncate text-right text-fg" title={gpus.map((gpu) => gpu.name).join(", ")}>
          {gpus.length === 0 ? t("setupWizard.noGpu") : gpus.map((gpu) => `${gpu.name}${gpu.vram_mb ? ` (${gigabytes(gpu.vram_mb)})` : ""}`).join(", ")}
        </dd>
      </dl>
    </motion.li>
  );
}

/** A public network as the federated directory lists it to the wizard. */
interface DirectoryNetwork {
  cluster_id: string;
  name: string;
  fingerprint: string;
  publisher_key: string;
  trust_document: string;
  organization: string;
  country_name: string;
  country_code: string;
  city: string;
  access_policy?: AccessPolicy;
  accepted_enrollments?: EnrollmentRole[];
  lobby_chat?: boolean;
  directory_replicas?: string[];
}

/**
 * Public networks, inside the wizard. A machine that has joined nothing has
 * no cluster to discover networks through, so this is the federated directory
 * alone (`GET /api/setup/public-networks`). Each network can be talked to
 * before joining - that is the point of listing it here - and "Join" fills the
 * same trust fields the manual block does.
 */
export function PublicDirectoryPanel({ setup, onUse, t }: { setup: SetupState; onUse: (network: DirectoryNetwork) => void; t: Translate }) {
  const [networks, setNetworks] = useState<DirectoryNetwork[] | null>(null);
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [chatTarget, setChatTarget] = useState<DirectoryNetwork | null>(null);
  const replicas = setup.publicDirectory.trim();

  useEffect(() => {
    let cancelled = false;
    setFailed(false);
    void api.get<DirectoryNetwork[]>(`/api/setup/public-networks?replicas=${encodeURIComponent(replicas)}`, { auth: false })
      .then((listed) => { if (!cancelled) setNetworks(Array.isArray(listed) ? listed : []); })
      .catch(() => { if (!cancelled) { setNetworks([]); setFailed(true); } });
    return () => { cancelled = true; };
    // `replicas` is read when the list is asked for, not on every keystroke.
  }, [attempt]);

  const role = setup.profile === "node" || setup.profile === "infer" ? setup.profile : null;
  return (
    <details className="pom-setup-details mt-5" data-testid="setup-public-directory">
      <summary>
        <span className="flex items-center gap-2"><Globe2 className="h-4 w-4" />{t("firstRun.publicDirectory")}</span>
        <ChevronDown className="h-4 w-4" />
      </summary>
      <div className="space-y-2 pt-3">
        <p className="text-xs leading-relaxed text-muted">{t("firstRun.publicDirectoryDetail")}</p>
        {networks === null && <p className="text-xs text-faint">{t("firstRun.publicDirectoryLoading")}</p>}
        {networks?.length === 0 && <p className="text-xs text-faint">{t(failed ? "firstRun.publicDirectoryFailed" : "firstRun.publicDirectoryEmpty")}</p>}
        {(networks ?? []).map((network) => {
          const accepted = network.accepted_enrollments;
          const closed = accepted !== undefined && (accepted.length === 0 || (role !== null && !accepted.includes(role)));
          const selected = setup.trustDocument === network.trust_document;
          return (
            <div key={`${network.cluster_id}:${network.fingerprint}`} className={clsx("flex flex-wrap items-center justify-between gap-3 rounded-lg border px-3 py-2.5", selected ? "border-aqua/50 bg-aqua/5" : "border-white/10")}>
              <div className="min-w-0">
                <p className="truncate text-sm font-medium text-fg">{network.name || network.cluster_id}</p>
                <p className="truncate text-xs text-muted">{[network.organization, [network.country_name || network.country_code, network.city].filter(Boolean).join(" / ")].filter(Boolean).join(" - ")}</p>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                {network.lobby_chat && (
                  <button type="button" className="pom-setup-ghost pom-setup-ghost-sm" onClick={() => setChatTarget(network)}>
                    <MessageCircle className="h-3.5 w-3.5" />{t("publicNetworks.chat")}
                  </button>
                )}
                <button type="button" className="pom-setup-ghost pom-setup-ghost-sm" disabled={closed} title={closed ? t("firstRun.roleNotAccepted") : undefined} onClick={() => onUse(network)}>
                  {t(closed ? "publicNetworks.closed" : selected ? "firstRun.publicDirectorySelected" : "firstRun.publicDirectoryUse")}
                </button>
              </div>
            </div>
          );
        })}
        <button type="button" className="pom-setup-ghost pom-setup-ghost-sm" onClick={() => setAttempt((value) => value + 1)}>
          <RefreshCw className="h-3.5 w-3.5" />{t("publicNetworks.refresh")}
        </button>
      </div>
      <LobbyChat network={chatTarget && { name: chatTarget.name || chatTarget.cluster_id, publisher_key: chatTarget.publisher_key }} endpoint="/api/setup/lobby" onClose={() => setChatTarget(null)} />
    </details>
  );
}

export function JoinScreen({ setup, update, updateNetwork, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; updateNetwork: (patch: Partial<SetupState["network"]>) => void; t: Translate }) {
  const [manualOpen, setManualOpen] = useState(false);
  const networkName = trustNetworkMetadata(setup.trustDocument)?.public_name ?? clusterIdFromDocument(setup.trustDocument);

  function applyTrust(document: string) {
    update({ trustDocument: document });
    const metadata = trustNetworkMetadata(document);
    if (!metadata) return;
    updateNetwork({
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
      publicDirectory: metadata.directory_replicas?.join(", ") ?? "",
    });
  }

  return (
    <>
      <Heading title={t("firstRun.joinTitle")} detail={t("firstRun.joinDetail")} />
      <LocalDiscoveryPanel
        mode="join"
        setup={setup}
        onResult={(result) => {
          applyTrust(result.trust_document);
          update({ trustFingerprint: result.trust_fingerprint, connectionKey: result.connection_key });
        }}
        t={t}
      />
      <AnimatePresence initial={false}>
        {setup.trustFingerprint && networkName && (
          <motion.div key="joined" initial={{ opacity: 0, y: 10 }} animate={{ opacity: 1, y: 0 }} className="pom-setup-secret mt-4" data-testid="join-network">
            <p className="text-[0.68rem] font-semibold uppercase tracking-[0.16em] text-aqua">{t("firstRun.joinNetwork")}</p>
            <p className="mt-1 font-display text-lg font-semibold text-fg">{networkName}</p>
            <code className="mt-1 block break-words font-mono text-[0.7rem] text-muted">{groupFingerprint(setup.trustFingerprint)}</code>
          </motion.div>
        )}
      </AnimatePresence>
      <PublicDirectoryPanel
        setup={setup}
        t={t}
        onUse={(network) => {
          applyTrust(network.trust_document);
          updateNetwork({ public_name: network.name || network.cluster_id, visibility: "public", access_policy: network.access_policy ?? "anyone_with_anchor" });
          update({
            joinAccepts: network.accepted_enrollments ?? null,
            // A public network needs no shared secret; one left over from the
            // local discovery would be sent to a network it does not belong to.
            connectionKey: "",
            ...(network.directory_replicas?.length ? { publicDirectory: network.directory_replicas.join(", ") } : {}),
          });
          void fingerprintFromTrustDocument(network.trust_document).then((fingerprint) => update({ trustFingerprint: fingerprint ?? "" }));
        }}
      />
      <details className="pom-setup-details mt-5" open={manualOpen} onToggle={(event) => setManualOpen((event.target as HTMLDetailsElement).open)}>
        <summary>
          <span>{t("firstRun.manualJoin")}</span>
          <ChevronDown className="h-4 w-4" />
        </summary>
        {/* Só monta aberto: os campos recalculam a impressão digital, e fechados
            eles apagariam a que a descoberta local acabou de trazer. */}
        {manualOpen && (
          <div className="pt-3">
            <TrustFields setup={setup} update={update} applyTrust={applyTrust} t={t} />
          </div>
        )}
      </details>
    </>
  );
}

export function TrustFields({ setup, update, applyTrust, t }: { setup: SetupState; update: (patch: Partial<SetupState>) => void; applyTrust: (document: string) => void; t: Translate }) {
  const [pending, setPending] = useState(false);
  const [fileName, setFileName] = useState("");
  useEffect(() => {
    let cancelled = false;
    if (!setup.trustDocument.trim()) {
      update({ trustFingerprint: "" });
      return;
    }
    setPending(true);
    void fingerprintFromTrustDocument(setup.trustDocument).then((fingerprint) => {
      if (cancelled) return;
      update({ trustFingerprint: fingerprint ?? "" });
      setPending(false);
    });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [setup.trustDocument]);

  async function readTrustFile(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    event.target.value = "";
    if (!file) return;
    applyTrust(await readTextFile(file));
    setFileName(file.name);
  }

  const invalid = setup.trustDocument.trim() && !pending && !setup.trustFingerprint;
  return (
    <div className="space-y-4">
      <Field label={t("setupWizard.trustDocument")} hint={t("setupWizard.trustDocumentHint")}>
        <textarea data-testid="trust-document-textarea" value={setup.trustDocument} onChange={(event) => applyTrust(event.target.value)} className="min-h-32 w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 font-mono text-xs text-fg outline-none transition-colors focus:border-iris focus:ring-2 focus:ring-iris/25" />
        <div className="mt-2 flex flex-wrap items-center gap-3">
          <label className="pom-setup-ghost pom-setup-ghost-sm cursor-pointer">
            <Upload className="h-3.5 w-3.5" />
            <span>{t("setupWizard.uploadTrust")}</span>
            <input data-testid="trust-document-file" type="file" accept=".json,application/json" className="sr-only" onChange={(event) => void readTrustFile(event)} />
          </label>
          {fileName && <span className="max-w-full truncate text-xs text-muted">{fileName}</span>}
          {invalid && <span data-testid="trust-document-invalid" className="text-xs text-amber">{t("setupWizard.trustDocumentInvalid")}</span>}
        </div>
      </Field>
      {setup.network.visibility !== "public" && (
        <Field label={t("setupWizard.connectionSecret")} hint={t("setupWizard.connectionSecretHint")}>
          <Input data-testid="connection-key-input" type="password" value={setup.connectionKey} onChange={(event) => update({ connectionKey: event.target.value })} />
        </Field>
      )}
    </div>
  );
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

function SavedScreen({ t }: { t: Translate }) {
  return (
    <div className="flex flex-col items-center py-4 text-center" data-testid="setup-saved">
      <motion.span initial={{ scale: 0.6, opacity: 0 }} animate={{ scale: 1, opacity: 1 }} transition={{ type: "spring", stiffness: 260, damping: 18 }} className="flex h-14 w-14 items-center justify-center rounded-2xl border border-aqua/40 bg-aqua/12 text-aqua">
        <Check className="h-7 w-7" />
      </motion.span>
      <h1 className="mt-5 font-display text-xl font-semibold text-fg">{t("setupWizard.savedTitle")}</h1>
      <p className="mt-2 max-w-sm text-sm leading-relaxed text-muted">{t("firstRun.savedDetail")}</p>
      <span className="pom-login-link mt-7" aria-hidden="true"><i /><i /><i /></span>
    </div>
  );
}

/** O carregamento é a mesma pipeline do portão: três nós e o pulso viajando. */
function LoadingScreen() {
  return (
    <div className="flex min-h-[260px] items-center justify-center" data-testid="setup-loading">
      <span className="pom-login-link pom-setup-loading" aria-hidden="true"><i /><i /><i /></span>
    </div>
  );
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
  if (!setup.profile || !setup.nodeName.trim() || !setup.trustDocument.trim() || !setup.trustFingerprint.trim()) return false;
  const publicNetwork = setup.network.visibility === "public";
  if (setup.profile !== "admin") return publicNetwork || Boolean(setup.connectionKey.trim());
  if (!setup.network.public_name.trim()) return false;
  if (publicNetwork && (!setup.network.country_code.trim() || !setup.network.city.trim())) return false;
  return true;
}

export function canContinue(stage: WizardStage, setup: SetupState): boolean {
  if (stage === "setup-profile") return Boolean(setup.profile && setup.nodeName.trim());
  if (stage === "setup-network") {
    const publicNetwork = setup.network.visibility === "public";
    return Boolean(
      setup.network.public_name.trim()
      && (!publicNetwork || (setup.network.country_code.trim() && setup.network.city.trim())),
    );
  }
  if (stage === "setup-invite" || stage === "setup-join") return isReadyToSave(setup);
  return true;
}
