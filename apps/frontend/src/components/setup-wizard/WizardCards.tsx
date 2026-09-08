import clsx from "clsx";
import { Check, ChevronRight, Circle, Cpu, Download, HardDrive, MemoryStick, Monitor, Server, Wifi } from "lucide-react";
import type { ReactNode } from "react";
import { useI18n } from "../../i18n/provider";
import { Badge, Button, Panel, Progress } from "../ui";
import type { WizardCapability, WizardModel, WizardNetwork, WizardNode } from "../../routes/setupWizardTypes";

export function IntentCard({
  icon,
  title,
  description,
  recommended = false,
  onClick,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  recommended?: boolean;
  onClick: () => void;
}) {
  const { t } = useI18n();
  return (
    <button
      type="button"
      aria-label={title}
      onClick={onClick}
      className={clsx(
        "group flex min-h-[156px] min-w-0 flex-col items-start rounded-xl border bg-surface p-4 text-left shadow-panel transition-[border-color,background-color,transform] duration-150 ease-swift hover:-translate-y-0.5 hover:border-line-strong",
        recommended ? "border-iris/45 bg-iris/[0.07]" : "border-line",
      )}
    >
      <span className={clsx("flex h-10 w-10 items-center justify-center rounded-lg border", recommended ? "border-iris/35 bg-iris/12 text-iris" : "border-line bg-raised text-muted")}>
        {icon}
      </span>
      <span className="mt-3 flex w-full items-start justify-between gap-2">
        <span className="font-display text-[0.96rem] font-semibold text-fg">{title}</span>
        <ChevronRight className="mt-0.5 h-4 w-4 shrink-0 text-faint transition-transform group-hover:translate-x-0.5" />
      </span>
      <span className="mt-1.5 max-w-prose text-xs leading-relaxed text-muted">{description}</span>
      {recommended && <span className="mt-auto pt-3 text-[0.7rem] font-medium text-iris">{t("wizard.recommended")}</span>}
    </button>
  );
}

export function HardwareCard({ hardware }: { hardware: NonNullable<import("../../routes/setupWizardTypes").PomSetup["hardware"]> }) {
  const { t } = useI18n();
  return (
    <Panel className="overflow-hidden">
      <div className="grid gap-px bg-line sm:grid-cols-3">
        <HardwareStat icon={<Cpu className="h-4 w-4" />} label={t("wizard.cpu")} value={hardware.cpu.name} detail={t("wizard.cpuDetail", { cores: hardware.cpu.cores, threads: hardware.cpu.threads })} />
        <HardwareStat icon={<MemoryStick className="h-4 w-4" />} label={t("wizard.memory")} value={`${hardware.memoryGb} GB`} detail={t("wizard.memoryAvailable")} />
        <HardwareStat icon={<Monitor className="h-4 w-4" />} label={t("wizard.gpu")} value={hardware.gpu.name} detail={`${hardware.gpu.vramGb} GB VRAM · ${hardware.gpu.backend}`} />
      </div>
      <div className="flex flex-wrap items-center gap-2 border-t border-line px-4 py-3">
        <Badge tone="aqua"><Check className="h-3.5 w-3.5" />{t("wizard.gpuDetected")}</Badge>
        <Badge tone="aqua"><Check className="h-3.5 w-3.5" />{t("wizard.cudaAvailable")}</Badge>
        <Badge tone="aqua"><Check className="h-3.5 w-3.5" />{t("wizard.backendAvailable")}</Badge>
      </div>
    </Panel>
  );
}

function HardwareStat({ icon, label, value, detail }: { icon: ReactNode; label: string; value: string; detail: string }) {
  return (
    <div className="min-w-0 bg-surface px-4 py-4">
      <div className="flex items-center gap-2 text-xs font-medium text-muted">{icon}<span>{label}</span></div>
      <p className="mt-3 truncate font-display text-[0.95rem] font-semibold text-fg">{value}</p>
      <p className="mt-1 text-xs text-faint">{detail}</p>
    </div>
  );
}

export function NodeCard({ node, selected, onClick }: { node: WizardNode; selected: boolean; onClick: () => void }) {
  const { t } = useI18n();
  const statusTone = node.status === "connected" ? "aqua" : node.status === "failed" ? "rose" : "amber";
  const statusLabel = node.status === "connected" ? t("wizard.connected") : node.status === "failed" ? t("wizard.failed") : node.status === "connecting" ? t("wizard.connecting") : t("wizard.found");
  return (
    <button type="button" onClick={onClick} aria-pressed={selected} className={clsx("flex min-w-0 items-start gap-3 rounded-xl border bg-surface p-4 text-left transition-colors", selected ? "border-iris/55 bg-iris/[0.06]" : "border-line hover:border-line-strong")}>
      <span className={clsx("mt-0.5 flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border", selected ? "border-iris/35 bg-iris/12 text-iris" : "border-line bg-raised text-muted")}><Server className="h-4 w-4" /></span>
      <span className="min-w-0 flex-1">
        <span className="flex flex-wrap items-center gap-2"><span className="truncate font-display text-sm font-semibold text-fg">{node.name}</span><Badge tone={statusTone as "aqua" | "rose" | "amber"}><DotForStatus status={node.status} />{statusLabel}</Badge></span>
        <span className="mt-1 block font-mono text-xs text-muted">{node.address}</span>
        <span className="mt-2 block text-xs text-faint">{node.gpu} · {node.vramGb} GB VRAM</span>
      </span>
    </button>
  );
}

function DotForStatus({ status }: { status: WizardNode["status"] }) {
  return <span className={clsx("h-1.5 w-1.5 rounded-full", status === "connected" ? "bg-aqua" : status === "failed" ? "bg-rose" : "bg-amber")} />;
}

export function ModelCard({ model, selected, downloaded, onSelect, onDownload }: { model: WizardModel; selected: boolean; downloaded: boolean; onSelect: () => void; onDownload: () => void }) {
  const { t } = useI18n();
  const compatibility = model.compatibility === "recommended" ? t("wizard.recommendedForHardware") : t("wizard.compatibleWithHardware");
  return (
    <div role="button" tabIndex={0} aria-label={model.name} onClick={onSelect} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); onSelect(); } }} aria-pressed={selected} className={clsx("min-w-0 rounded-xl border bg-surface p-4 text-left transition-colors", selected ? "border-iris/55 bg-iris/[0.06]" : "border-line hover:border-line-strong")}>
      <div className="flex items-start justify-between gap-3"><div className="min-w-0"><div className="flex items-center gap-2"><span className="font-display text-base font-semibold text-fg">{model.name}</span>{model.compatibility === "recommended" && <Badge tone="aqua">{t("wizard.recommended")}</Badge>}</div><p className="mt-1 text-xs text-muted">{model.sizeGb} GB · {model.quantization}</p></div><HardDrive className="h-4 w-4 shrink-0 text-faint" /></div>
      <div className="mt-4 grid grid-cols-2 gap-3 text-xs"><div><span className="block text-faint">{t("wizard.estimatedMemory")}</span><span className="mt-1 block font-mono text-fg">{model.memoryGb} GB</span></div><div><span className="block text-faint">{t("wizard.compatibility")}</span><span className="mt-1 block text-aqua">{compatibility}</span></div></div>
      {selected && <div className="mt-4 border-t border-line pt-3"><Button type="button" size="sm" variant={downloaded ? "outline" : "primary"} icon={downloaded ? <Check className="h-3.5 w-3.5" /> : <Download className="h-3.5 w-3.5" />} onClick={(event) => { event.stopPropagation(); onDownload(); }}>{downloaded ? t("wizard.downloaded") : t("wizard.downloadUse")}</Button>{downloaded && <Progress value={100} tone="aqua" className="mt-3" />}</div>}
    </div>
  );
}

export function NetworkCard({ network, selected, onClick }: { network: WizardNetwork; selected: boolean; onClick: () => void }) {
  const { t } = useI18n();
  return (
    <button type="button" onClick={onClick} aria-pressed={selected} className={clsx("min-w-0 rounded-xl border bg-surface p-4 text-left transition-colors", selected ? "border-iris/55 bg-iris/[0.06]" : "border-line hover:border-line-strong")}>
      <div className="flex items-start justify-between gap-3"><div className="min-w-0"><span className="block truncate font-display text-sm font-semibold text-fg">{network.name}</span><span className="mt-1 block text-xs text-muted">{network.nodes} {t("wizard.nodes")} · {network.vramGb} GB VRAM</span></div><Wifi className={clsx("h-4 w-4 shrink-0", selected ? "text-iris" : "text-faint")} /></div>
      <div className="mt-4 grid grid-cols-2 gap-3 text-xs"><div><span className="block text-faint">{t("wizard.reputation")}</span><span className="mt-1 block font-mono text-fg">{network.reputation.toFixed(1)} / 5</span></div><div><span className="block text-faint">{t("wizard.latency")}</span><span className="mt-1 block font-mono text-fg">{network.latencyMs} ms</span></div></div>
      <div className="mt-3 flex flex-wrap gap-1.5">{network.models.map((model) => <Badge key={model} tone="neutral">{model}</Badge>)}</div>
    </button>
  );
}

export function CapabilityList({ options, selected, onToggle }: { options: WizardCapability[]; selected: string[]; onToggle: (id: string) => void }) {
  const { t } = useI18n();
  return (
    <div className="grid gap-2 sm:grid-cols-3">
      {options.map((option) => {
        const checked = selected.includes(option.id);
        return <button key={option.id} type="button" aria-pressed={checked} onClick={() => onToggle(option.id)} className={clsx("flex items-center gap-3 rounded-lg border px-3 py-3 text-left transition-colors", checked ? "border-aqua/45 bg-aqua/[0.07]" : "border-line bg-surface hover:border-line-strong")}><span className={clsx("flex h-5 w-5 shrink-0 items-center justify-center rounded-full border", checked ? "border-aqua bg-aqua text-ink" : "border-line-strong text-transparent")}><Check className="h-3 w-3" /></span><span className="min-w-0"><span className="block text-sm font-medium text-fg">{t(option.labelKey)}</span><span className="mt-0.5 block text-xs text-faint">{checked ? t("wizard.enabled") : t("wizard.disabled")}</span></span></button>;
      })}
    </div>
  );
}

export function SelectionPill({ active, children }: { active: boolean; children: ReactNode }) {
  return <span className={clsx("inline-flex items-center gap-1.5 rounded-md border px-2 py-1 text-xs", active ? "border-iris/35 bg-iris/12 text-iris" : "border-line bg-raised text-muted")}><Circle className={clsx("h-2.5 w-2.5", active ? "fill-current" : "")} />{children}</span>;
}
