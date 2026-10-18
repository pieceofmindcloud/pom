import { useState, type ReactNode } from "react";
import {
  Activity,
  Check,
  ChevronRight,
  Code2,
  Copy,
  Cpu,
  Globe2,
  KeyRound,
  Laptop,
  LockKeyhole,
  Network,
  QrCode,
  RefreshCw,
  Router,
  Server,
  ShieldCheck,
  Sparkles,
  Users,
  Wifi,
  Wrench,
} from "lucide-react";
import { useI18n } from "../../i18n/provider";
import { copyText } from "../../lib/clipboard";
import { Badge, Button, Field, Input, Panel, PanelHeader, Select } from "../ui";
import {
  CapabilityList,
  HardwareCard,
  IntentCard,
  ModelCard,
  NetworkCard,
  NodeCard,
  SelectionPill,
} from "./WizardCards";
import { ContinueButton } from "./WizardLayout";
import {
  MOCK_CAPABILITY_OPTIONS,
  MOCK_HEALTH,
  MOCK_HARDWARE,
  MOCK_MODELS,
  MOCK_NODES,
  MOCK_PUBLIC_NETWORKS,
  MOCK_TEST_INFERENCE,
} from "../../routes/setupWizardMocks";
import type { PomSetup, RoleName, WizardIntent, WizardModel } from "../../routes/setupWizardTypes";

type ScreenProps = {
  setup: PomSetup;
  onUpdate: (patch: Partial<PomSetup>) => void;
  onContinue: () => void;
};

export function WelcomeScreen({ onIntent, onObjective }: { onIntent: (intent: WizardIntent) => void; onObjective: () => void }) {
  const { t } = useI18n();
  const options: Array<{ intent: WizardIntent; icon: ReactNode; titleKey: string; descriptionKey: string }> = [
    { intent: "personal", icon: <Laptop className="h-4 w-4" />, titleKey: "wizard.intentPersonal", descriptionKey: "wizard.intentPersonalDescription" },
    { intent: "organization", icon: <Users className="h-4 w-4" />, titleKey: "wizard.intentOrganization", descriptionKey: "wizard.intentOrganizationDescription" },
    { intent: "contributor", icon: <Cpu className="h-4 w-4" />, titleKey: "wizard.intentContributor", descriptionKey: "wizard.intentContributorDescription" },
    { intent: "inference", icon: <Sparkles className="h-4 w-4" />, titleKey: "wizard.intentInference", descriptionKey: "wizard.intentInferenceDescription" },
    { intent: "advanced", icon: <Wrench className="h-4 w-4" />, titleKey: "wizard.intentAdvanced", descriptionKey: "wizard.intentAdvancedDescription" },
  ];
  return (
    <div className="grid min-w-0 gap-6 lg:grid-cols-[minmax(0,1.05fr)_minmax(0,1fr)] lg:items-start">
      <div className="min-w-0 pt-2 lg:pt-8">
        <Badge tone="iris"><Sparkles className="h-3.5 w-3.5" />{t("wizard.welcomeEyebrow")}</Badge>
        <h1 className="mt-5 max-w-xl font-display text-4xl font-bold leading-[1.05] tracking-[-0.04em] text-fg sm:text-5xl">{t("wizard.welcomeTitle")}</h1>
        <p className="mt-5 max-w-lg text-base leading-relaxed text-muted">{t("wizard.welcomeSubtitle")}</p>
        <button type="button" onClick={onObjective} className="mt-6 inline-flex items-center gap-1.5 text-sm font-medium text-iris hover:text-fg">{t("wizard.chooseObjective")}<ChevronRight className="h-4 w-4" /></button>
      </div>
      <div className="min-w-0 space-y-3">
        <Panel className="relative overflow-hidden border-iris/45 bg-iris/[0.07] p-5 shadow-lift">
          <div className="absolute -right-8 -top-12 h-40 w-40 rounded-full bg-iris/15 blur-3xl" aria-hidden="true" />
          <div className="relative flex items-start gap-4"><span className="flex h-11 w-11 shrink-0 items-center justify-center rounded-xl border border-iris/40 bg-iris/15 text-iris"><Sparkles className="h-5 w-5" /></span><div className="min-w-0"><p className="font-display text-lg font-semibold text-fg">{t("wizard.autoTitle")}</p><p className="mt-1.5 text-sm leading-relaxed text-muted">{t("wizard.autoDescription")}</p><Button variant="primary" className="mt-5" onClick={() => onIntent("quick")}>{t("wizard.start")}</Button></div></div>
        </Panel>
        <div className="grid gap-2 sm:grid-cols-2">
          {options.map((option) => <IntentCard key={option.intent} icon={option.icon} title={t(option.titleKey)} description={t(option.descriptionKey)} onClick={() => onIntent(option.intent)} />)}
        </div>
      </div>
    </div>
  );
}

export function ObjectiveScreen({ onIntent }: { onIntent: (intent: WizardIntent) => void }) {
  const { t } = useI18n();
  const options: Array<{ intent: WizardIntent; icon: ReactNode; titleKey: string; descriptionKey: string }> = [
    { intent: "quick", icon: <Sparkles className="h-4 w-4" />, titleKey: "wizard.intentQuick", descriptionKey: "wizard.intentQuickDescription" },
    { intent: "personal", icon: <Laptop className="h-4 w-4" />, titleKey: "wizard.intentPersonal", descriptionKey: "wizard.intentPersonalDescription" },
    { intent: "organization", icon: <Users className="h-4 w-4" />, titleKey: "wizard.intentOrganization", descriptionKey: "wizard.intentOrganizationDescription" },
    { intent: "contributor", icon: <Cpu className="h-4 w-4" />, titleKey: "wizard.intentContributor", descriptionKey: "wizard.intentContributorDescription" },
    { intent: "inference", icon: <Sparkles className="h-4 w-4" />, titleKey: "wizard.intentInference", descriptionKey: "wizard.intentInferenceDescription" },
    { intent: "advanced", icon: <Wrench className="h-4 w-4" />, titleKey: "wizard.intentAdvanced", descriptionKey: "wizard.intentAdvancedDescription" },
  ];
  return <ScreenFrame eyebrow={t("wizard.intentEyebrow")} title={t("wizard.intentTitle")} subtitle={t("wizard.intentSubtitle")}><div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">{options.map((option) => <IntentCard key={option.intent} icon={option.icon} title={t(option.titleKey)} description={t(option.descriptionKey)} recommended={option.intent === "quick"} onClick={() => onIntent(option.intent)} />)}</div></ScreenFrame>;
}

export function HardwareScreen({ onContinue }: Pick<ScreenProps, "onContinue">) {
  const { t } = useI18n();
  return <ScreenFrame eyebrow={t("wizard.hardwareEyebrow")} title={t("wizard.hardwareTitle")} subtitle={t("wizard.hardwareSubtitle")}><HardwareCard hardware={MOCK_HARDWARE} /><div className="mt-4 flex items-start gap-3 rounded-lg border border-aqua/25 bg-aqua/[0.05] px-4 py-3 text-sm text-muted"><ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-aqua" /><span>{t("wizard.hardwareReady")}</span></div><ScreenActions><ContinueButton onClick={onContinue} label="wizard.useComputer" /></ScreenActions></ScreenFrame>;
}

export function NetworkScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  const options: Array<{ mode: PomSetup["network"]["mode"]; icon: ReactNode; titleKey: string; descriptionKey: string }> = [
    { mode: "local", icon: <Router className="h-4 w-4" />, titleKey: "wizard.networkLocal", descriptionKey: "wizard.networkLocalDescription" },
    { mode: "private", icon: <LockKeyhole className="h-4 w-4" />, titleKey: "wizard.networkPrivate", descriptionKey: "wizard.networkPrivateDescription" },
    { mode: "public", icon: <Globe2 className="h-4 w-4" />, titleKey: "wizard.networkPublic", descriptionKey: "wizard.networkPublicDescription" },
  ];
  const contributor = setup.intent === "contributor";
  return <ScreenFrame eyebrow={t(contributor ? "wizard.networkChooseEyebrow" : "wizard.networkEyebrow")} title={t(contributor ? "wizard.networkChooseTitle" : "wizard.networkTitle")} subtitle={t(contributor ? "wizard.networkChooseSubtitle" : "wizard.networkSubtitle")}><div className="max-w-md"><Field label={t("wizard.networkName")} hint={t("wizard.networkNameHint")}><Input value={setup.network.name} onChange={(event) => onUpdate({ network: { ...setup.network, name: event.target.value } })} /></Field></div><div className="mt-6 grid gap-3 md:grid-cols-3">{options.map((option) => <button key={option.mode} type="button" aria-pressed={setup.network.mode === option.mode} onClick={() => onUpdate({ network: { ...setup.network, mode: option.mode } })} className={`min-w-0 rounded-xl border p-4 text-left transition-colors ${setup.network.mode === option.mode ? "border-iris/55 bg-iris/[0.06]" : "border-line bg-surface hover:border-line-strong"}`}><span className="flex h-9 w-9 items-center justify-center rounded-lg border border-line bg-raised text-muted">{option.icon}</span><span className="mt-3 block font-display text-sm font-semibold text-fg">{t(option.titleKey)}</span><span className="mt-1.5 block text-xs leading-relaxed text-muted">{t(option.descriptionKey)}</span></button>)}</div><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function DiscoveryScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  const selected = setup.nodes.map((node) => node.id);
  function addAll() {
    onUpdate({ nodes: MOCK_NODES.map((node) => ({ ...node, status: "connected" as const })) });
  }
  return <ScreenFrame eyebrow={t("wizard.discoveryEyebrow")} title={t("wizard.discoveryTitle")} subtitle={t("wizard.discoverySubtitle")}><div className="flex flex-wrap items-center justify-between gap-3"><div className="flex items-center gap-2 text-sm text-muted"><Wifi className="h-4 w-4 text-aqua" />{t("wizard.discoveryFound")}</div><Button size="sm" variant="outline" icon={<Check className="h-3.5 w-3.5" />} onClick={addAll}>{t("wizard.addAll")}</Button></div><div className="mt-4 grid gap-3 sm:grid-cols-2">{MOCK_NODES.map((node) => <NodeCard key={node.id} node={{ ...node, status: selected.includes(node.id) ? "connected" : node.status }} selected={selected.includes(node.id)} onClick={() => onUpdate({ nodes: selected.includes(node.id) ? setup.nodes.filter((current) => current.id !== node.id) : [...setup.nodes, { ...node, status: "connected" }] })} />)}</div><div className="mt-4 flex items-center gap-2 text-xs text-faint"><Activity className="h-3.5 w-3.5" />{t("wizard.discoveryMock")}</div><ScreenActions><Button variant="ghost" onClick={onContinue}>{t("wizard.skip")}</Button><ContinueButton onClick={() => { if (setup.nodes.length === 0) addAll(); onContinue(); }} /></ScreenActions></ScreenFrame>;
}

export function ModelScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  const [downloaded, setDownloaded] = useState(false);
  const activeModel = setup.model ?? MOCK_MODELS[0];
  function select(model: WizardModel) { onUpdate({ model }); setDownloaded(false); }
  return <ScreenFrame eyebrow={t("wizard.modelEyebrow")} title={t("wizard.modelTitle")} subtitle={t("wizard.modelSubtitle")}><div className="grid gap-3 md:grid-cols-3">{MOCK_MODELS.map((model) => <ModelCard key={model.id} model={model} selected={activeModel.id === model.id} downloaded={activeModel.id === model.id && downloaded} onSelect={() => select(model)} onDownload={() => setDownloaded(true)} />)}</div><p className="mt-4 text-xs text-faint">{t("wizard.downloadMock")}</p><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function PipelineScreen({ setup, onContinue }: Pick<ScreenProps, "setup" | "onContinue">) {
  const { t } = useI18n();
  const nodes = setup.nodes.length > 0 ? setup.nodes : [{ ...MOCK_NODES[0], name: t("wizard.thisComputer"), status: "connected" as const }];
  return <ScreenFrame eyebrow={t("wizard.pipelineEyebrow")} title={t("wizard.pipelineTitle")} subtitle={t("wizard.pipelineSubtitle")}><Panel className="overflow-hidden"><div className="border-b border-line bg-raised/60 px-4 py-3"><div className="flex items-center justify-between gap-2"><span className="text-xs text-muted">{t("wizard.modelToUse")}</span><Badge tone="iris">{setup.model?.name ?? MOCK_MODELS[0].name}</Badge></div></div><div className="flex flex-col items-stretch gap-2 p-4 sm:flex-row sm:items-center sm:gap-0">{nodes.slice(0, 3).map((node, index) => <div key={node.id} className="flex min-w-0 flex-1 items-center gap-2 sm:flex-col sm:items-stretch sm:gap-0"><div className="flex min-w-0 items-center gap-3 rounded-lg border border-line bg-surface px-3 py-3"><span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-iris/12 text-iris"><Server className="h-4 w-4" /></span><span className="min-w-0"><span className="block truncate text-sm font-medium text-fg">{node.name}</span><span className="mt-0.5 block font-mono text-[0.68rem] text-faint">{node.gpu}</span></span></div><div className="hidden items-center justify-center py-2 text-iris sm:flex"><ChevronRight className="h-4 w-4" /></div><div className="flex items-center gap-2 sm:hidden"><ChevronRight className="h-4 w-4 text-iris" /><span className="text-[0.68rem] text-faint">{t("wizard.connected")}</span></div>{index === nodes.slice(0, 3).length - 1 && <div className="hidden sm:block" />}</div>)}<div className="flex min-w-0 items-center gap-3 rounded-lg border border-aqua/30 bg-aqua/[0.06] px-3 py-3 sm:ml-2 sm:flex-1"><span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-aqua/12 text-aqua"><Sparkles className="h-4 w-4" /></span><span className="text-sm font-medium text-fg">Inference</span></div></div></Panel><div className="mt-4 grid gap-2 sm:grid-cols-2">{["wizard.pipelineModel", "wizard.pipelineHardware", "wizard.pipelineCreated", "wizard.pipelineConnected", "wizard.pipelineLoaded"].map((key) => <div key={key} className="flex items-center gap-2 text-sm text-muted"><Check className="h-4 w-4 text-aqua" />{t(key)}</div>)}</div><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function HealthScreen({ onContinue }: Pick<ScreenProps, "onContinue">) {
  const { t } = useI18n();
  return <ScreenFrame eyebrow={t("wizard.healthEyebrow")} title={t("wizard.healthTitle")} subtitle={t("wizard.healthSubtitle")}><div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">{MOCK_HEALTH.map((item) => <div key={item.id} className="flex items-center gap-2.5 rounded-lg border border-aqua/25 bg-aqua/[0.05] px-3 py-3 text-sm text-fg"><span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-aqua text-ink"><Check className="h-3 w-3" /></span>{t(item.labelKey)}</div>)}</div><Panel className="mt-5"><PanelHeader title={t("wizard.testInferenceTitle")} subtitle={t("wizard.testInferenceSubtitle")} /><div className="grid gap-4 px-5 py-4 md:grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)]"><div className="rounded-lg border border-line bg-ink px-3 py-3"><span className="block text-xs text-faint">{t("wizard.prompt")}</span><p className="mt-2 font-mono text-sm text-fg">{MOCK_TEST_INFERENCE.prompt}</p></div><div className="rounded-lg border border-aqua/25 bg-aqua/[0.05] px-3 py-3"><span className="block text-xs text-faint">{t("wizard.result")}</span><p className="mt-2 text-sm text-fg">{MOCK_TEST_INFERENCE.result}</p></div></div><div className="grid grid-cols-3 gap-px border-t border-line bg-line"><Metric label="TTFT" value={MOCK_TEST_INFERENCE.ttft} /><Metric label={t("wizard.generation")} value={MOCK_TEST_INFERENCE.generation} /><Metric label={t("wizard.latency")} value={MOCK_TEST_INFERENCE.latency} /></div></Panel><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

function Metric({ label, value }: { label: string; value: string }) { return <div className="bg-surface px-3 py-3"><span className="block text-[0.68rem] text-faint">{label}</span><span className="mt-1 block font-mono text-sm text-fg">{value}</span></div>; }

export function OrganizationScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  return <ScreenFrame eyebrow={t("wizard.organizationEyebrow")} title={t("wizard.organizationTitle")} subtitle={t("wizard.organizationSubtitle")}><Panel><PanelHeader title={t("wizard.organizationPanelTitle")} subtitle={t("wizard.organizationPanelSubtitle")} /><div className="grid gap-4 px-5 py-5 sm:grid-cols-2"><Field label={t("wizard.organizationName")}><Input value={setup.organization.name} placeholder={t("wizard.organizationNamePlaceholder")} onChange={(event) => onUpdate({ organization: { ...setup.organization, name: event.target.value } })} /></Field><Field label={t("wizard.organizationNetwork")} hint={t("wizard.organizationNetworkHint")}><Input value={setup.organization.networkName} placeholder={t("wizard.organizationNetworkPlaceholder")} onChange={(event) => onUpdate({ organization: { ...setup.organization, networkName: event.target.value } })} /></Field></div></Panel><div className="mt-4 flex items-center gap-2"><SelectionPill active>{t("wizard.networkPrivate")}</SelectionPill><span className="text-xs text-muted">{t("wizard.organizationPrivateNote")}</span></div><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function SecurityScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  const [access, setAccess] = useState("authorized");
  const [channels, setChannels] = useState(["chat", "api"]);
  const toggle = (value: string) => setChannels((current) => current.includes(value) ? current.filter((item) => item !== value) : [...current, value]);
  return <ScreenFrame eyebrow={t("wizard.securityEyebrow")} title={t("wizard.securityTitle")} subtitle={t("wizard.securitySubtitle")}><Panel><PanelHeader title={t("wizard.securityAccessTitle")} subtitle={t("wizard.securityAccessSubtitle")} /><div className="grid gap-2 px-5 py-4 sm:grid-cols-3">{[["authorized", "wizard.onlyUsers"], ["devices", "wizard.onlyDevices"], ["local", "wizard.anyLocal"]].map(([value, key]) => <ToggleChoice key={value} active={access === value} label={t(key)} onClick={() => { setAccess(value); onUpdate({ advanced: { ...setup.advanced, security: value as PomSetup["advanced"]["security"] } }); }} />)}</div></Panel><Panel className="mt-4"><PanelHeader title={t("wizard.securityChannelsTitle")} subtitle={t("wizard.securityChannelsSubtitle")} /><div className="grid gap-2 px-5 py-4 sm:grid-cols-2">{[["chat", "wizard.channelChat"], ["api", "wizard.channelApi"], ["keys", "wizard.channelKeys"], ["apps", "wizard.channelApps"]].map(([value, key]) => <ToggleChoice key={value} active={channels.includes(value)} label={t(key)} onClick={() => toggle(value)} />)}</div></Panel><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

function ToggleChoice({ active, label, onClick }: { active: boolean; label: string; onClick: () => void }) { return <button type="button" aria-pressed={active} onClick={onClick} className={`flex items-center gap-2 rounded-lg border px-3 py-3 text-left text-sm transition-colors ${active ? "border-iris/40 bg-iris/10 text-fg" : "border-line bg-surface text-muted hover:border-line-strong"}`}><span className={`flex h-4 w-4 shrink-0 items-center justify-center rounded-full border ${active ? "border-iris bg-iris text-white" : "border-line-strong"}`}>{active && <Check className="h-2.5 w-2.5" />}</span>{label}</button>; }

export function InviteScreen({ onContinue }: Pick<ScreenProps, "onContinue">) {
  const { t } = useI18n();
  const [code, setCode] = useState("POM-7K92-XA31");
  const [copied, setCopied] = useState(false);
  function copy() {
    void copyText(`POM invite: ${code}`).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    });
  }
  return <ScreenFrame eyebrow={t("wizard.inviteEyebrow")} title={t("wizard.inviteTitle")} subtitle={t("wizard.inviteSubtitle")}><div className="grid gap-5 lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]"><Panel className="flex min-h-[250px] items-center justify-center"><div className="flex flex-col items-center gap-4"><div className="grid h-40 w-40 grid-cols-9 gap-1 rounded-lg border border-line bg-white p-3">{Array.from({ length: 81 }, (_, index) => <span key={index} className={`${(index * 17 + index % 5) % 7 < 3 || index < 9 || index % 9 < 2 ? "bg-slate-900" : "bg-white"}`} />)}</div><div className="flex items-center gap-2 text-xs text-muted"><QrCode className="h-3.5 w-3.5" />{t("wizard.qrMock")}</div></div></Panel><Panel><PanelHeader title={t("wizard.inviteCodeTitle")} subtitle={t("wizard.inviteCodeSubtitle")} /><div className="px-5 py-5"><code className="block rounded-lg border border-line bg-ink px-4 py-3 text-center font-mono text-lg tracking-[0.12em] text-fg">{code}</code><div className="mt-4 flex flex-wrap gap-2"><Button size="sm" variant="outline" icon={copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />} onClick={copy}>{copied ? t("wizard.copied") : t("wizard.copyInvite")}</Button><Button size="sm" variant="ghost" icon={<RefreshCw className="h-3.5 w-3.5" />} onClick={() => setCode("POM-4M18-ZB74")}>{t("wizard.newCode")}</Button></div><p className="mt-5 text-xs leading-relaxed text-faint">{t("wizard.inviteLinkMock")}</p></div></Panel></div><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function UsersScreen({ onContinue }: Pick<ScreenProps, "onContinue">) {
  const { t } = useI18n();
  return <ScreenFrame eyebrow={t("wizard.usersEyebrow")} title={t("wizard.usersTitle")} subtitle={t("wizard.usersSubtitle")}><Panel><PanelHeader title={t("wizard.teamPanelTitle")} subtitle={t("wizard.teamPanelSubtitle")} actions={<Badge tone="aqua"><Check className="h-3.5 w-3.5" />{t("wizard.ready")}</Badge>} /><div className="divide-y divide-line"><TeamRow name="Ana Costa" email="ana@acme.local" role={t("wizard.adminRole")} /><TeamRow name="Bruno Lima" email="bruno@acme.local" role={t("wizard.memberRole")} /><div className="flex items-center gap-3 px-5 py-4"><Button size="sm" variant="outline" icon={<Users className="h-3.5 w-3.5" />}>{t("wizard.addUser")}</Button><span className="text-xs text-faint">{t("wizard.addUserMock")}</span></div></div></Panel><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

function TeamRow({ name, email, role }: { name: string; email: string; role: string }) { return <div className="flex flex-wrap items-center gap-3 px-5 py-3"><span className="flex h-8 w-8 items-center justify-center rounded-full bg-iris/12 text-xs font-semibold text-iris">{name.split(" ").map((part) => part[0]).join("")}</span><span className="min-w-0 flex-1"><span className="block text-sm font-medium text-fg">{name}</span><span className="block truncate text-xs text-faint">{email}</span></span><Badge tone="neutral">{role}</Badge></div>; }

export function ApiScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  return <ScreenFrame eyebrow={t("wizard.apiEyebrow")} title={t("wizard.apiTitle")} subtitle={t("wizard.apiSubtitle")}><Panel><PanelHeader title={t("wizard.apiPanelTitle")} subtitle={t("wizard.apiPanelSubtitle")} actions={<Badge tone="aqua"><Check className="h-3.5 w-3.5" />{t("wizard.enabled")}</Badge>} /><div className="grid gap-4 px-5 py-5 sm:grid-cols-2"><div className="flex items-start gap-3 rounded-lg border border-line bg-raised px-3 py-3"><Code2 className="mt-0.5 h-4 w-4 shrink-0 text-iris" /><div><p className="text-sm font-medium text-fg">{t("wizard.apiOpenAi")}</p><p className="mt-1 text-xs leading-relaxed text-muted">{t("wizard.apiOpenAiDescription")}</p><code className="mt-3 block break-all rounded-md bg-ink px-2 py-1.5 font-mono text-[0.68rem] text-faint">http://pom.local/v1</code></div></div><div className="flex items-start gap-3 rounded-lg border border-line bg-raised px-3 py-3"><KeyRound className="mt-0.5 h-4 w-4 shrink-0 text-aqua" /><div><p className="text-sm font-medium text-fg">{t("wizard.apiKeys")}</p><p className="mt-1 text-xs leading-relaxed text-muted">{t("wizard.apiKeysDescription")}</p><ToggleChoice active={setup.advanced.api} label={t("wizard.apiEnabled")} onClick={() => onUpdate({ advanced: { ...setup.advanced, api: !setup.advanced.api } })} /></div></div></div></Panel><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function ResourcesScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  const contributor = setup.intent === "contributor";
  if (!contributor) return <ScreenFrame eyebrow={t("wizard.resourcesEyebrow")} title={t("wizard.resourcesTitle")} subtitle={t("wizard.resourcesSubtitle")}><Panel><PanelHeader title={t("wizard.resourcesPanelTitle")} subtitle={t("wizard.resourcesPanelSubtitle")} /><div className="px-5 py-5"><CapabilityList options={MOCK_CAPABILITY_OPTIONS} selected={setup.capabilities} onToggle={(id) => onUpdate({ capabilities: setup.capabilities.includes(id) ? setup.capabilities.filter((item) => item !== id) : [...setup.capabilities, id] })} /></div></Panel><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
  return <ScreenFrame eyebrow={t("wizard.contributorEyebrow")} title={t("wizard.contributorTitle")} subtitle={t("wizard.contributorSubtitle")}><HardwareCard hardware={MOCK_HARDWARE} /><Panel className="mt-4"><PanelHeader title={t("wizard.resourcesPanelTitle")} subtitle={t("wizard.contributorResourcesSubtitle")} /><div className="grid gap-5 px-5 py-5 sm:grid-cols-3"><ResourceSlider label={t("wizard.gpuAvailable")} value={setup.resources.gpu} hint={resourceHint(setup.resources.gpu, t)} onChange={(value) => onUpdate({ resources: { ...setup.resources, gpu: value } })} /><ResourceSlider label={t("wizard.ramAvailable")} value={setup.resources.ram} hint={resourceHint(setup.resources.ram, t)} onChange={(value) => onUpdate({ resources: { ...setup.resources, ram: value } })} /><ResourceSlider label={t("wizard.cpuAvailable")} value={setup.resources.cpu} hint={resourceHint(setup.resources.cpu, t)} onChange={(value) => onUpdate({ resources: { ...setup.resources, cpu: value } })} /></div><div className="flex flex-wrap gap-2 border-t border-line px-5 py-4"><ToggleChoice active={setup.resources.availability === "idle"} label={t("wizard.whenIdle")} onClick={() => onUpdate({ resources: { ...setup.resources, availability: "idle" } })} /><ToggleChoice active={setup.resources.availability === "always"} label={t("wizard.alwaysAvailable")} onClick={() => onUpdate({ resources: { ...setup.resources, availability: "always" } })} /><ToggleChoice active={setup.resources.scheduleEnabled} label={t("wizard.allowedHours")} onClick={() => onUpdate({ resources: { ...setup.resources, scheduleEnabled: !setup.resources.scheduleEnabled } })} /></div></Panel><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

function ResourceSlider({ label, value, hint, onChange }: { label: string; value: number; hint: string; onChange: (value: number) => void }) { return <label className="block"><span className="flex items-center justify-between gap-2 text-sm text-muted"><span>{label}</span><span className="font-mono text-fg">{value}%</span></span><input aria-label={label} type="range" min="0" max="100" value={value} onChange={(event) => onChange(Number(event.target.value))} className="mt-3 w-full accent-[rgb(var(--iris))]" /><span className="mt-1 block text-xs text-faint">{hint}</span></label>; }
function resourceHint(value: number, t: (key: string) => string) { return value === 100 ? "100%" : value > 70 ? t("wizard.resourceGenerous") : value > 35 ? t("wizard.resourceBalanced") : t("wizard.resourceConservative"); }

export function NetworkBrowserScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  return <ScreenFrame eyebrow={t("wizard.networkBrowserEyebrow")} title={t("wizard.networkBrowserTitle")} subtitle={t("wizard.networkBrowserSubtitle")}><div className="grid gap-3 md:grid-cols-3">{MOCK_PUBLIC_NETWORKS.map((network) => <NetworkCard key={network.id} network={network} selected={setup.selectedNetwork?.id === network.id} onClick={() => onUpdate({ selectedNetwork: network })} />)}</div><p className="mt-4 flex items-center gap-2 text-xs text-faint"><Globe2 className="h-3.5 w-3.5" />{t("wizard.networkBrowserMock")}</p><ScreenActions><ContinueButton onClick={onContinue} label={setup.intent === "contributor" ? "wizard.connectToNetwork" : "wizard.continue"} /></ScreenActions></ScreenFrame>;
}

export function JoinScreen({ onContinue }: Pick<ScreenProps, "onContinue">) {
  const { t } = useI18n();
  return <ScreenFrame eyebrow={t("wizard.joinEyebrow")} title={t("wizard.joinTitle")} subtitle={t("wizard.joinSubtitle")}><div className="grid gap-3 sm:grid-cols-2"><JoinOption icon={<KeyRound className="h-4 w-4" />} title={t("wizard.joinCode")} detail={t("wizard.joinCodeDetail")} active /><JoinOption icon={<QrCode className="h-4 w-4" />} title={t("wizard.joinQr")} detail={t("wizard.joinQrDetail")} /><JoinOption icon={<Globe2 className="h-4 w-4" />} title={t("wizard.joinNetworks")} detail={t("wizard.joinNetworksDetail")} /><JoinOption icon={<Network className="h-4 w-4" />} title={t("wizard.joinManual")} detail={t("wizard.joinManualDetail")} /></div><div className="mt-5 max-w-md"><Field label={t("wizard.inviteCodeLabel")} hint={t("wizard.inviteCodeHint")}><Input defaultValue="POM-7K92-XA31" className="font-mono" /></Field></div><ScreenActions><ContinueButton onClick={onContinue} label="wizard.connect" /></ScreenActions></ScreenFrame>;
}
function JoinOption({ icon, title, detail, active = false }: { icon: ReactNode; title: string; detail: string; active?: boolean }) { return <div className={`flex items-start gap-3 rounded-xl border p-4 ${active ? "border-iris/45 bg-iris/[0.06]" : "border-line bg-surface"}`}><span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-muted">{icon}</span><div><p className="text-sm font-medium text-fg">{title}</p><p className="mt-1 text-xs leading-relaxed text-muted">{detail}</p></div></div>; }

export function ConnectionScreen({ onContinue }: Pick<ScreenProps, "onContinue">) {
  const { t } = useI18n();
  const checks = ["wizard.organizationFound", "wizard.networkFound", "wizard.secureConnection", "wizard.modelsFound", "wizard.permissionsLoaded"];
  return <ScreenFrame eyebrow={t("wizard.connectionEyebrow")} title={t("wizard.connectionTitle")} subtitle={t("wizard.connectionSubtitle")}><Panel><div className="divide-y divide-line">{checks.map((key) => <div key={key} className="flex items-center gap-3 px-5 py-3.5"><span className="flex h-5 w-5 items-center justify-center rounded-full bg-aqua text-ink"><Check className="h-3 w-3" /></span><span className="text-sm text-fg">{t(key)}</span><Badge tone="aqua" className="ml-auto">{t("wizard.connected")}</Badge></div>)}</div></Panel><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function AdvancedScreen({ setup, onUpdate, onContinue }: ScreenProps) {
  const { t } = useI18n();
  const roles: Array<{ id: RoleName; key: string; icon: ReactNode }> = [{ id: "admin", key: "profile.admin", icon: <ShieldCheck className="h-4 w-4" /> }, { id: "worker", key: "profile.worker", icon: <Server className="h-4 w-4" /> }, { id: "visitor", key: "profile.visitor", icon: <Sparkles className="h-4 w-4" /> }];
  function toggleRole(role: RoleName) { onUpdate({ roles: { ...setup.roles, [role]: !setup.roles[role] } }); }
  return <ScreenFrame eyebrow={t("wizard.advancedEyebrow")} title={t("wizard.advancedTitle")} subtitle={t("wizard.advancedSubtitle")}><Panel><PanelHeader title={t("wizard.rolesTitle")} subtitle={t("wizard.rolesSubtitle")} /><div className="grid gap-2 px-5 py-4 sm:grid-cols-3">{roles.map((role) => <ToggleChoice key={role.id} active={setup.roles[role.id]} label={t(role.key)} onClick={() => toggleRole(role.id)} />)}</div></Panel><div className="mt-4 grid gap-4 md:grid-cols-2"><Panel><PanelHeader title={t("wizard.networkingTitle")} /><div className="space-y-4 px-5 py-4"><Field label={t("wizard.networkMode")}><Select value={setup.network.mode} onChange={(event) => onUpdate({ network: { ...setup.network, mode: event.target.value as PomSetup["network"]["mode"] } })}><option value="local">{t("wizard.networkLocal")}</option><option value="private">{t("wizard.networkPrivate")}</option><option value="public">{t("wizard.networkPublic")}</option></Select></Field><Field label={t("wizard.discoveryMode")}><Select value={setup.advanced.discovery} onChange={(event) => onUpdate({ advanced: { ...setup.advanced, discovery: event.target.value as PomSetup["advanced"]["discovery"] } })}><option value="automatic">{t("wizard.discoveryAutomatic")}</option><option value="mdns">mDNS</option><option value="manual">{t("wizard.discoveryManual")}</option></Select></Field></div></Panel><Panel><PanelHeader title={t("wizard.runtimeTitle")} /><div className="space-y-4 px-5 py-4"><Field label={t("wizard.backendMode")}><Select value={setup.advanced.backend} onChange={(event) => onUpdate({ advanced: { ...setup.advanced, backend: event.target.value as PomSetup["advanced"]["backend"] } })}><option value="auto">{t("wizard.backendAuto")}</option><option value="cuda">CUDA</option><option value="cpu">CPU</option></Select></Field><ToggleChoice active={setup.advanced.api} label={t("wizard.apiEnabled")} onClick={() => onUpdate({ advanced: { ...setup.advanced, api: !setup.advanced.api } })} /></div></Panel></div><ScreenActions><ContinueButton onClick={onContinue} /></ScreenActions></ScreenFrame>;
}

export function SummaryScreen({ setup, onExit }: { setup: PomSetup; onExit: () => void }) {
  const { t } = useI18n();
  const [showAdvanced, setShowAdvanced] = useState(false);
  const yaml = `pom:\n  profile:\n    admin: ${setup.roles.admin}\n    worker: ${setup.roles.worker}\n    visitor: ${setup.roles.visitor}\n  network:\n    mode: ${setup.network.mode}\n    discovery: ${setup.network.discovery}\n  worker:\n    enabled: ${setup.roles.worker}\n    resources:\n      gpu: auto\n      cpu: auto\n      memory: auto\n  visitor:\n    enabled: ${setup.roles.visitor}\n  api:\n    enabled: ${setup.advanced.api}\n  security:\n    authentication: ${setup.advanced.security}`;
  return <ScreenFrame eyebrow={t("wizard.summaryEyebrow")} title={t("wizard.summaryTitle")} subtitle={t("wizard.summarySubtitle")}><div className="flex flex-col items-center rounded-2xl border border-aqua/30 bg-aqua/[0.06] px-5 py-8 text-center"><span className="flex h-12 w-12 items-center justify-center rounded-full bg-aqua text-ink"><Check className="h-6 w-6" /></span><h2 className="mt-4 font-display text-2xl font-bold tracking-tight text-fg">{t("wizard.readyTitle")}</h2><p className="mt-2 max-w-md text-sm leading-relaxed text-muted">{t("wizard.readySubtitle")}</p></div><div className="mt-5 grid gap-4 lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]"><Panel><PanelHeader title={t("wizard.summaryConfiguration")} subtitle={t("wizard.summaryConfigurationSubtitle")} /><div className="space-y-3 px-5 py-4"><SummaryRow label={t("wizard.summaryIntent")} value={t(`wizard.intent${setup.intent ? setup.intent[0].toUpperCase() + setup.intent.slice(1) : "Quick"}`)} /><SummaryRow label={t("wizard.summaryRoles")} value={Object.entries(setup.roles).filter(([, enabled]) => enabled).map(([role]) => role.toUpperCase()).join(" · ")} /><SummaryRow label={t("wizard.summaryNetwork")} value={`${setup.network.name} · ${setup.network.mode}`} />{setup.model && <SummaryRow label={t("wizard.summaryModel")} value={`${setup.model.name} · ${setup.model.quantization}`} />}{setup.selectedNetwork && <SummaryRow label={t("wizard.summaryConnectedNetwork")} value={setup.selectedNetwork.name} />}</div></Panel><Panel><PanelHeader title={t("wizard.summaryAdvancedTitle")} subtitle={t("wizard.summaryAdvancedSubtitle")} actions={<Button size="sm" variant="ghost" onClick={() => setShowAdvanced((value) => !value)}>{t("wizard.viewAdvanced")}</Button>} />{showAdvanced && <pre className="break-path overflow-auto bg-ink px-5 py-4 font-mono text-xs leading-relaxed text-fg">{yaml}</pre>} {!showAdvanced && <div className="flex items-center gap-3 px-5 py-8 text-sm text-muted"><Code2 className="h-4 w-4 text-iris" />{t("wizard.summaryAdvancedHint")}</div>}</Panel></div><ScreenActions><Button variant="primary" icon={<Sparkles className="h-4 w-4" />} onClick={onExit}>{t("wizard.usePom")}</Button></ScreenActions></ScreenFrame>;
}

function SummaryRow({ label, value }: { label: string; value: string }) { return <div className="flex flex-wrap items-baseline justify-between gap-2 border-b border-line pb-2 last:border-0 last:pb-0"><span className="text-xs text-muted">{label}</span><span className="font-mono text-xs text-fg">{value}</span></div>; }

function ScreenFrame({ eyebrow, title, subtitle, children }: { eyebrow: string; title: string; subtitle: string; children: ReactNode }) { return <div className="min-w-0"><p className="eyebrow">{eyebrow}</p><h1 className="mt-2 max-w-2xl font-display text-2xl font-bold tracking-tight text-fg sm:text-3xl">{title}</h1><p className="mt-2 max-w-2xl text-sm leading-relaxed text-muted">{subtitle}</p><div className="mt-6 min-w-0">{children}</div></div>; }
function ScreenActions({ children }: { children: ReactNode }) { return <div className="mt-6 flex flex-wrap items-center justify-end gap-2">{children}</div>; }
