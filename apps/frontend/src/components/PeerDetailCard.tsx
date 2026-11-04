/**
 * Everything a machine reports, in the same layout for all of them.
 *
 * It lives here, and not inside the Dashboard, because it is shown in two places: the
 * Details tab stacks one card per machine of the network, and the double click on a card of the
 * Flow opens this same card in a window. A single component is what guarantees
 * that the two screens do not diverge later.
 *
 * A machine without a deployed slice is not a degenerate case of a machine with a slice:
 * it has no layer range, no role in the pipeline and no file. The
 * card shows what it actually reports - host, state, memory, disk, load -
 * and says there is no slice, instead of writing "0-0" or an empty layer
 * bar that reads as coverage.
 */
import clsx from "clsx";
import { motion } from "framer-motion";
import { ChevronRight } from "lucide-react";
import { type ReactNode } from "react";
import { Modal } from "./Confirm";
import { GpuTemperature } from "./GpuTemperature";
import { MachineIcon, gpuDisplayName, machineKind } from "./MachineIcon";
import { trackOf } from "./PipelineRail";
import { ReleaseSlice } from "./ReleaseSlice";
import { RemoveNode } from "./RemoveNode";
import { KvCache } from "./KvCache";
import { PluginServiceBadge } from "./PluginServices";
import { isCpuBuffer, ModelPlacementSection } from "./ModelPlacement";
import { SpeedBadges, type PeerSpeed } from "./SpeedBadge";
import { Badge, Button, Dot, InfoTip, KeyValue, Meter } from "./ui";
import {
  isServing,
  layerRange,
  megabytes,
  num,
  serverStateLabel,
  serverStateTone,
} from "../lib/format";
import { hasSlice } from "../lib/networkPeers";
import { usePeerRuntime } from "../lib/queries";
import type { ChatTelemetryPeerPerf, DiskStat, GpuStat, PeerLoadParams, PeerRuntimeHealth, PeerView, ThermalPolicy } from "../lib/types";
import { temperatureBand } from "../lib/thermal";
import { useI18n } from "../i18n/provider";

/**
 * A block that opens. `<details>` because the semantics already exist in the browser:
 * it opens by click and by keyboard, and the state belongs to the element, not to a `useState`
 * that is lost when the machine list renews under the card.
 */
function Accordion({
  title,
  children,
  tip,
}: {
  title: string;
  children: ReactNode;
  tip?: string;
}) {
  return (
    <details className="group border-t border-line">
      <summary className="flex cursor-pointer list-none items-center gap-1.5 px-3 py-2 text-muted hover:text-fg [&::-webkit-details-marker]:hidden">
        <ChevronRight
          aria-hidden="true"
          className="h-3 w-3 shrink-0 transition-transform group-open:rotate-90"
        />
        <span className="eyebrow">{title}</span>
        {tip && (
          <span onClick={(event) => event.preventDefault()}>
            <InfoTip label={title} message={tip} />
          </span>
        )}
      </summary>
      <div className="pb-1">{children}</div>
    </details>
  );
}

/**
 * What this node reports about the loaded model: MTP, load parameters and
 * layer placement.
 *
 * The health arrives ready from the card, and not from a fetch of its own: the MTP
 * badge went up to the toolbar, and two calls to the same
 * `/health` - one for the bar, another for this section - would be two possibly
 * different answers on the same screen.
 */
function PeerPlacement({
  health,
  kqvRequested,
  mtpRequested,
}: {
  health: PeerRuntimeHealth | undefined;
  kqvRequested: boolean;
  mtpRequested: boolean;
}) {
  const { t } = useI18n();
  if (!health) return null;
  return (
    <>
      <ContextReservationNotice health={health} />
      {mtpRequested && !health.mtp_ready && (
        <p
          role="status"
          className="border-t border-line px-3 py-2 text-[0.7rem] leading-relaxed text-rose"
        >
          {t("dashboard.mtpUnavailableDetail")}{" "}
          <span className="break-words text-muted">
            {health.mtp_unavailable_reason ?? t("dashboard.mtpReasonUnknown")}
          </span>
        </p>
      )}
      {health.load_params && <LoadParamsSection params={health.load_params} />}
      <ModelPlacementSection
        placement={health.placement ?? null}
        contextMemory={health.context_memory?.buffers ?? null}
        offloadKqv={health.offload_kqv ?? kqvRequested}
        fitApplied={health.fit_memory_applied ?? false}
      />
    </>
  );
}

/**
 * The total context is multiplied by `max_sessions` before reaching
 * llama.cpp. When the measured KV is on the GPU, two or more sessions may take
 * the fit to the limit and turn an apparently innocent configuration into a
 * large per-call degradation. The warning uses only the effective state of
 * `/health`, not the configuration requested by the deploy.
 */
function ContextReservationNotice({ health }: { health: PeerRuntimeHealth }) {
  const { t } = useI18n();
  const gpuKv =
    health.context_memory?.buffers.some(
      (buffer) => buffer.context > 0 && !isCpuBuffer(buffer.buffer),
    ) ?? false;
  if (health.max_sessions <= 1 || !gpuKv) return null;

  return (
    <p
      role="status"
      className="border-t border-line px-3 py-2 text-[0.7rem] leading-relaxed text-amber"
    >
      {t("dashboard.contextSessionsWarning", {
        sessions: num(health.max_sessions),
        total: num(health.n_ctx_total),
        perSession: num(health.n_ctx_per_session),
        fit: health.fit_memory_applied
          ? t("dashboard.contextFitApplied")
          : t("dashboard.contextFitNotApplied"),
      })}
    </p>
  );
}

/**
 * With which llama.cpp parameters this node's context came up.
 *
 * It exists for the operator who wants to compare the machine with a command line
 * of `llama-cli` without opening `/health` by hand. Every value comes from the node's
 * live context; nothing here is derived from the requested configuration, because the
 * KV-in-RAM fallback brings up a different context from the one that was requested.
 */
function LoadParamsSection({ params }: { params: PeerLoadParams }) {
  const { t } = useI18n();
  // Flash Attention is the only field that may be missing: it is the only one that depends
  // on the diagnostic bridge. `null` becomes "unknown", never "off".
  const fa =
    params.flash_attn === null
      ? t("dashboard.paramsUnknown")
      : params.flash_attn
        ? params.flash_attn_auto
          ? t("dashboard.faOnAuto")
          : t("dashboard.faOn")
        : params.flash_attn_auto
          ? t("dashboard.faOffAuto")
          : t("dashboard.faOff");
  return (
    <Accordion
      title={t("dashboard.loadParams")}
      tip={params.fit_memory_applied ? t("dashboard.paramsFitNote") : undefined}
    >
      <dl className="grid gap-x-3 gap-y-2 px-3 pb-2 sm:grid-cols-3">
        <KeyValue compact label={t("dashboard.paramFlashAttn")}>
          <Badge
            tone={params.flash_attn === null ? "neutral" : params.flash_attn ? "aqua" : "amber"}
            className="px-1.5 py-0 text-[0.62rem]"
          >
            {fa}
          </Badge>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramBatch")}>
          <span className="font-mono">
            {num(params.n_batch)} / {num(params.n_ubatch)}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramCtx")}>
          <span className="font-mono">
            {num(params.n_ctx)} / {num(params.n_ctx_seq)}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramSeqMax")}>
          <span className="font-mono">
            {num(params.n_seq_max)}
            {params.kv_unified ? " · " + t("dashboard.paramKvUnified") : ""}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramThreads")}>
          <span className="font-mono">
            {num(params.n_threads)} / {num(params.n_threads_batch)}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramGpuLayers")}>
          <span className="font-mono">{num(params.n_gpu_layers)}</span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramCacheType")}>
          <span className="font-mono">
            {params.cache_type_k} / {params.cache_type_v}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramKqv")}>
          <Badge tone={params.offload_kqv ? "aqua" : "amber"} className="px-1.5 py-0 text-[0.62rem]">
            {t(params.offload_kqv ? "dashboard.paramOnGpu" : "dashboard.paramOnCpu")}
          </Badge>
        </KeyValue>
      </dl>
    </Accordion>
  );
}

/**
 * The range this card can show, or `null` when there is none.
 *
 * The peer's announcement is the source; the local config only comes in when the peer has not yet
 * announced the slice that the machine already loaded. A range is never invented.
 */
export function cardRange(peer: PeerView): [number, number] | null {
  if (hasSlice(peer)) return [peer.start_layer, peer.end_layer];
  const cfg = peer.node?.config;
  if (cfg && cfg.total_layers > 0) return [cfg.start_layer, cfg.end_layer];
  return null;
}

/**
 * A disk, with the same layout as any other gauge on the card.
 *
 * `disks` brings one entry per mounted partition, and the label is the mount
 * point because it is what distinguishes two partitions of the same disk; the
 * device goes in the `title` for whoever needs to know which physical disk it
 * is. An old node, which does not publish the list yet, falls back to the single volume.
 */
function DiskMeter({ disk, label }: { disk: DiskStat; label: string }) {
  const { t } = useI18n();
  const used = disk.total_mb > 0 ? ((disk.total_mb - disk.free_mb) / disk.total_mb) * 100 : null;
  return (
    <div className="min-w-0" title={disk.device || undefined}>
      <Meter
        compact
        label={label}
        value={used}
        tone={used != null && used > 90 ? "rose" : "amber"}
        detail={disk.total_mb > 0 ? megabytes(disk.free_mb) + " " + t("dashboard.free") : "—"}
      />
    </div>
  );
}

/**
 * A card, whole in one block: name, VRAM, usage, temperature and power.
 *
 * The VRAM lived in a separate grid, below all the telemetry rows,
 * and in a box with two cards you had to match "which bar belongs to which row"
 * by name. Here each card answers for itself.
 */
function GpuTile({
  gpu,
  label,
  policy,
}: {
  gpu: GpuStat;
  label: string;
  policy?: ThermalPolicy;
}) {
  const { t } = useI18n();
  const band = temperatureBand(gpu.temperature_c, policy ?? FALLBACK_THERMAL_POLICY);
  const temperatureLabel = {
    unknown: t("dashboard.temperatureSensorUnavailable"),
    normal: t("dashboard.temperatureNormal"),
    attention: t("dashboard.temperatureAttention"),
    critical: t("dashboard.temperatureCritical"),
  }[band];
  const temperature = band === "unknown"
    ? temperatureLabel
    : `${num(gpu.temperature_c, 1)} °C · ${temperatureLabel}`;
  const tone = {
    unknown: "neutral",
    normal: "aqua",
    attention: "amber",
    critical: "rose",
  }[band] as "neutral" | "aqua" | "amber" | "rose";

  return (
    <div
      data-testid="gpu-telemetry-row"
      className="min-w-0 rounded-lg border border-line bg-raised/60 px-2.5 py-2"
      aria-label={`${label}: ${temperature}`}
    >
      <div className="mb-1.5 truncate text-[0.72rem] font-medium text-fg" title={label}>{label}</div>
      {gpu.vram_used_pct != null && (
        <Meter
          compact
          label="VRAM"
          value={gpu.vram_used_pct}
          tone={gpu.vram_used_pct > 88 ? "rose" : "lime"}
          detail={t("dashboard.usedOf", {
            used: megabytes(gpu.vram_used_mb),
            total: megabytes(gpu.vram_total_mb),
          })}
        />
      )}
      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[0.66rem]">
        <span className="whitespace-nowrap font-mono text-muted">
          <span className="mr-1 text-faint">{t("dashboard.gpuUsage")}</span>
          {gpu.utilization_pct == null ? num(null) : `${num(gpu.utilization_pct)}%`}
        </span>
        <Badge tone={tone} className="whitespace-nowrap px-1.5 py-0 text-[0.58rem]">
          {temperature}
        </Badge>
        <span className="ml-auto whitespace-nowrap font-mono text-muted">
          <span className="mr-1 text-faint">{t("dashboard.gpuPower")}</span>
          {gpu.power_watts == null || !Number.isFinite(gpu.power_watts)
            ? num(null)
            : `${num(gpu.power_watts)} W`}
        </span>
      </div>
    </div>
  );
}

const FALLBACK_THERMAL_POLICY: ThermalPolicy = {
  limit_c: 85,
  hysteresis_c: 8,
  cooldown_secs: 30,
};

export function PeerDetailCard({
  peer,
  index,
  group,
  otherRanges,
  variant = "panel",
  perf,
  speed,
}: {
  peer: PeerView;
  index: number;
  /** Model the cluster is serving, which is how the node groups the slices. */
  group: string;
  /** Ranges of the other peers of this model, to know whether this is the last. */
  otherRanges: [number, number][];
  /**
   * `panel` is the card of the Details tab, one among many in a grid. `dialog` is
   * the same machine alone in the Flow window: no frame (the window already is
   * one), with a larger header and named sections, because there is room there.
   */
  variant?: "panel" | "dialog";
  /** Measured speed and the note against the other slices, when the Flow has it. */
  perf?: ChatTelemetryPeerPerf;
  speed?: PeerSpeed;
}) {
  const dialog = variant === "dialog";
  const pad = dialog ? "px-0" : "px-3";
  const { t } = useI18n();
  const track = trackOf(index);
  const n = peer.node;
  const system = n?.system;
  const kind = machineKind(system);
  const cfg = n?.config;
  const serving = isServing(n?.status.state);
  const hasGpu = system != null && system.gpu_kind !== "none";
  // Only those that report VRAM: unified memory falls into the `unified` branch below,
  // which measures something else (wired), and a bar without a value would become a
  // "loading" animation forever.
  const range = cardRange(peer);
  /*
   * `/health` is requested once per card, here, and not inside the placement
   * section: the MTP badge went up to the toolbar and the
   * section remains below, and two fetches to the same place could show
   * two answers on the same screen. `enabled` remains what prevents one
   * `/health` per card on every Dashboard cycle.
   */
  const runtime = usePeerRuntime(peer.node_id, serving);
  const health = serving ? runtime.data?.health : undefined;
  const mtpRequested = cfg?.mtp_enabled ?? false;
  /*
   * The list of volumes, with the project's volume as the fallback. The empty list is
   * a node that does not publish the field yet, not a machine without a disk.
   */
  const disks = system ? (system.disks?.length ? system.disks : [system.disk]) : [];

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.26, delay: Math.min(index, 6) * 0.04 }}
      className={dialog ? "relative" : "panel relative overflow-hidden"}
    >
      {!dialog && (
        <span
          className={clsx(
            "absolute inset-x-0 top-0 h-[2px]",
            range ? track.fill : "bg-line-strong",
          )}
        />
      )}

      <div className={clsx("flex flex-wrap items-start justify-between gap-2 border-b border-line", dialog ? "pb-4" : "px-3 py-2.5")}>
        <div className={clsx("flex min-w-0 items-start", dialog ? "gap-3" : "gap-2")}>
          <div className={clsx("mt-0.5 flex shrink-0 items-center justify-center rounded-lg border border-line bg-raised", dialog ? "h-11 w-11" : "h-7 w-7")}>
            <MachineIcon system={system} size={dialog ? "md" : "sm"} />
          </div>
          <div className="min-w-0">
            <div className="flex items-center gap-1.5">
              <h3 className={clsx("truncate font-display font-semibold text-fg", dialog ? "text-lg leading-tight" : "text-[0.86rem]")}>
                {n?.hostname || peer.short_id}
              </h3>
              {peer.is_self && (
                <Badge tone="neutral" className="px-1.5 py-0 text-[0.58rem]">
                  {t("dashboard.here")}
                </Badge>
              )}
            </div>
            <p className={clsx("truncate text-muted", dialog ? "text-[0.75rem]" : "text-[0.66rem]")}>{kind.label}</p>
            {dialog && speed && <div className="mt-2"><SpeedBadges perf={perf} rating={speed} /></div>}
          </div>
        </div>
        <div className={clsx("flex shrink-0 items-center gap-2", dialog && "rounded-lg border border-line bg-raised/50 px-2.5 py-1.5")}>
          {range ? (
            <span className={clsx("font-mono font-semibold", dialog ? "text-sm" : "text-[0.78rem]", track.text)}>
              {layerRange(range[0], range[1])}
            </span>
          ) : (
            // The explanation of "no slice" left the wide strip that took up a
            // whole line of the card and went into the `[i]` of the badge itself.
            <span className="flex items-center gap-1">
              <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">
                {t("dashboard.noSlice")}
              </Badge>
              <InfoTip label={t("dashboard.noSlice")} message={t("dashboard.noSliceDetail")} />
            </span>
          )}
          <span className="flex items-center gap-1 text-[0.66rem] text-muted">
            <Dot
              tone={
                peer.rediscovering
                  ? "amber"
                  : peer.unreachable
                    ? "rose"
                    : serverStateTone(n?.status.state)
              }
              pulse={serving || peer.rediscovering}
            />
            {peer.rediscovering
              ? t("dashboard.rediscoveringShort")
              : (peer.unreachable ?? serverStateLabel(n?.status.state))}
          </span>
        </div>
      </div>

      {peer.rediscovering || peer.unreachable ? (
        <div className="space-y-2 px-3 py-3 text-[0.75rem] text-muted">
          {/*
            Re-finding is not the same as unreadable. The address died, the node
            has already asked for the current one, and the next gossip round usually resolves it:
            saying only "nothing below can be read" sent the user hunting for
            a defect that fixes itself.
          */}
          {peer.rediscovering
            ? t("dashboard.rediscovering")
            : t("dashboard.unreachable", { error: peer.unreachable ?? "" })}
          <GpuTemperature
            gpuKind={system?.gpu_kind ?? "none"}
            temperature={null}
            policy={n?.thermal}
          />
        </div>
      ) : (
        <>
          {/*
            The machine's toolbar, at the top.
            They were scattered across the foot of the card, one per section, and to restart
            a machine you had to scroll to the end of everything it reports.
            Here they are what you do *with* this machine, and they stay where it is
            named. The MTP badge comes along because it is operating state,
            not a measurement: it answers "is the draft up?" and where it fit.
          */}
          <div className={clsx("flex flex-wrap items-center gap-1.5 border-b border-line", dialog ? "py-3" : "bg-raised/40 px-3 py-2")}>
            {mtpRequested && health && (
              <span className="flex items-center gap-1">
                <Badge
                  tone={health.mtp_ready ? "aqua" : "rose"}
                  className="px-1.5 py-0 text-[0.62rem]"
                >
                  {health.mtp_ready
                    ? health.mtp_allocation === "ram"
                      ? t("dashboard.mtpReadyRam")
                      : health.mtp_allocation === "vram"
                        ? t("dashboard.mtpReadyVram")
                        : t("dashboard.mtpReadyUnknown")
                    : t("dashboard.mtpUnavailable")}
                </Badge>
                <InfoTip
                  label={t("dashboard.mtpStatus")}
                  message={
                    health.mtp_ready
                      ? t("dashboard.mtpStatus")
                      : `${t("dashboard.mtpUnavailableDetail")} ${
                          health.mtp_unavailable_reason ?? t("dashboard.mtpReasonUnknown")
                        }`
                  }
                />
              </span>
            )}
            <div className="ml-auto flex flex-wrap items-center justify-end gap-1.5">
              {/* Only on the machine's own card: whoever frees the memory is the node
                  that owns it, never a coordinator meddling. */}
              {peer.is_self && cfg && (
                <ReleaseSlice compact startLayer={cfg.start_layer} endLayer={cfg.end_layer} />
              )}
              {/* Only "Remove": it takes the machine out of the model and leaves it in the
                  cluster. Leaving the whole network is not offered from here; the machine
                  does it from its own My Machine screen. */}
              {serving && group && range && (
                <RemoveNode
                  nodeId={peer.node_id}
                  group={group}
                  deployId={peer.node?.config?.deploy_id || undefined}
                  range={range}
                  existingRanges={otherRanges}
                />
              )}
            </div>
          </div>

          {dialog && <h4 className="pt-4 font-display text-sm font-semibold text-fg">{t("dashboard.sectionResources")}</h4>}
          <div className={clsx("grid gap-2.5 sm:grid-cols-2", pad, dialog ? "pb-4 pt-2" : "py-2.5")}>
            {hasGpu && (
              <section data-testid="gpu-telemetry-section" className="min-w-0 sm:col-span-2">
                <div className="mb-1.5 flex items-center gap-1">
                  <span className="eyebrow">{t("dashboard.gpus")}</span>
                  {system?.gpu_note && (
                    <InfoTip label={t("dashboard.gpus")} message={system.gpu_note} />
                  )}
                </div>
                {system?.gpus.length ? (
                  <div className={clsx("grid min-w-0 gap-1.5", system.gpus.length > 1 && "sm:grid-cols-2")}>
                    {system.gpus.map((g) => (
                      <GpuTile
                        key={g.index}
                        gpu={g}
                        policy={n?.thermal}
                        label={gpuDisplayName(g, system.gpus)}
                      />
                    ))}
                  </div>
                ) : null}
                {system?.gpu_kind === "unified" && (
                  <div className="mt-1.5">
                    <Meter
                      compact
                      label={t("dashboard.wiredMemory")}
                      value={
                        system.wired_mb != null && system.ram.total_mb > 0
                          ? (system.wired_mb / system.ram.total_mb) * 100
                          : null
                      }
                      tone="sky"
                      detail={system.wired_mb != null ? megabytes(system.wired_mb) : "—"}
                    />
                  </div>
                )}
              </section>
            )}
            <Meter
              compact
              label="RAM"
              value={system?.ram.used_pct ?? null}
              tone={
                (system?.ram.used_pct ?? 0) > 88
                  ? "rose"
                  : (system?.ram.used_pct ?? 0) > 70
                    ? "amber"
                    : "iris"
              }
              detail={
                system
                  ? t("dashboard.usedOf", {
                      used: megabytes(system.ram.used_mb),
                      total: megabytes(system.ram.total_mb),
                    })
                  : "—"
              }
            />
            <Meter
              compact
              label={t("dashboard.load", { count: system?.cpu_count ?? "—" })}
              value={
                system && system.cpu_count > 0
                  ? Math.min(100, (system.load_avg[0] / system.cpu_count) * 100)
                  : null
              }
              tone="iris"
              detail={system ? system.load_avg.map((l) => num(l, 2)).join(" · ") : "—"}
            />
            <div className="grid min-w-0 gap-x-3 gap-y-2 sm:col-span-2 sm:grid-cols-2">
              {disks.map((disk) => (
                <DiskMeter
                  key={disk.mount_point + disk.device}
                  disk={disk}
                  // Every machine's volumes arrive as one, the space where slices can go,
                  // with no mount point (`shared_system` on the node).
                  label={disk.mount_point ? t("dashboard.disk", { path: disk.mount_point }) : t("dashboard.modelStorage")}
                />
              ))}
            </div>
          </div>

          {dialog && <h4 className="border-t border-line pt-4 font-display text-sm font-semibold text-fg">{t("dashboard.sectionDeployment")}</h4>}
          <dl className={clsx("grid gap-x-3 gap-y-2 sm:grid-cols-3", pad, dialog ? "pb-4 pt-2" : "border-t border-line py-2.5")}>
            <KeyValue compact label={t("dashboard.model")}>
              {cfg?.model_label ? (
                <span className="font-mono">{cfg.model_label}</span>
              ) : (
                <span className="text-muted">{t("dashboard.noneLoaded")}</span>
              )}
            </KeyValue>
            <KeyValue compact label={t("dashboard.role")}>
              <Badge tone={range ? "iris" : "neutral"} className="px-1.5 py-0 text-[0.62rem]">
                {cfg
                  ? cfg.single_peer
                    ? t("dashboard.single")
                    : cfg.start_layer === 0
                      ? t("dashboard.prompt")
                      : cfg.end_layer + 1 >= cfg.total_layers
                        ? t("dashboard.tokens")
                        : t("dashboard.middle")
                  : /* Without a loaded slice there is no role in the pipeline. The old
                       `node.role` (principal/node) left `/api/ui/node` with the
                       trust model and was already always undefined here. */
                    "—"}
              </Badge>
            </KeyValue>
            {/* Right beside the role, and not hidden in the header: the question
                this field answers is "are the nodes all on the same version?",
                and it is answered by comparing cards side by side. */}
            <KeyValue compact label={t("dashboard.build")}>
              {n?.build_commit ? <span className="font-mono">{n.build_commit}</span> : "—"}
            </KeyValue>
            {/* The file is a long path and is rarely the question: it stays in the
                `[i]`, and the card gets the line back. */}
            <KeyValue compact label={t("dashboard.context")}>
              <span className="flex min-w-0 items-center gap-1">
                <span className="truncate font-mono">
                  {cfg
                    ? `${num(cfg.n_ctx)} · ${cfg.max_sessions} ${t("dashboard.session", { count: cfg.max_sessions })}`
                    : "—"}
                </span>
                {cfg && (
                  <InfoTip
                    label={t("dashboard.context")}
                    message={`n_ctx ${num(cfg.n_ctx)} · n_batch ${num(cfg.n_batch)}${
                      cfg.model_path ? ` · ${cfg.model_path}` : ""
                    }`}
                  />
                )}
              </span>
            </KeyValue>
            {/* Through where this node talks to that one: with several announced
                addresses, "which of them won" is only answered here. */}
            <KeyValue compact label={t("dashboard.route")}>
              {peer.route ? (
                <span className="font-mono">
                  {peer.route.addr} · {Math.round(peer.route.rtt_ms)} ms
                </span>
              ) : (
                <span className="text-muted">{t("dashboard.routeUnmeasured")}</span>
              )}
            </KeyValue>
            <KeyValue compact label={t("dashboard.server")}>
              <span className="flex min-w-0 items-center gap-1">
                <span className="truncate">
                  {n ? n.status.detail || serverStateLabel(n.status.state) : "—"}
                </span>
                {n && (
                  <InfoTip
                    label={t("dashboard.server")}
                    message={[
                      peer.inference_url ?? t("dashboard.notAdvertised"),
                      n.status.pid ? `pid ${n.status.pid}` : null,
                      n.status.restarts > 0
                        ? t("dashboard.restarts", { count: n.status.restarts })
                        : null,
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  />
                )}
              </span>
            </KeyValue>
          </dl>

          {(n?.plugin_services?.length ?? 0) > 0 && (
            <div data-testid="plugin-services" className={clsx("border-t border-line", pad, dialog ? "py-4" : "py-2.5")}>
              <div className="mb-1.5 flex items-center gap-1">
                <span className="eyebrow">{t("dashboard.services")}</span>
                <InfoTip label={t("dashboard.services")} message={t("dashboard.servicesTip")} />
              </div>
              <div className="flex flex-wrap gap-1.5">
                {n?.plugin_services?.map((service) => (
                  <PluginServiceBadge key={`${service.plugin_code}:${service.id}`} service={service} />
                ))}
              </div>
            </div>
          )}

          {/* The cache disk exists with or without a loaded model; the RAM only
              appears when this machine's `/health` reports it. */}
          <KvCache
            nodeId={peer.node_id}
            hostname={n?.hostname || peer.short_id}
            isSelf={peer.is_self}
            disk={n?.kv_cache}
            ram={health?.cache_ram}
            className={clsx("border-t border-line", pad, dialog ? "py-4" : "py-2.5")}
          />

          {/* Only whoever has someone to ask asks a question: without a loaded model
              there is no placement to show. */}
          {/* The sections below bring their own indent; in the window, without a frame,
              they align with the rest by pulling the margin. */}
          <div className={clsx(dialog && "-mx-3")}>
          {serving && (
            <PeerPlacement
              health={health}
              kqvRequested={cfg?.offload_kqv ?? true}
              mtpRequested={mtpRequested}
            />
          )}

          {system && system.gpu_processes.length > 0 && (
            <Accordion title={t("dashboard.gpuProcesses")}>
              <ul className="space-y-0.5 px-3 pb-2">
                {system.gpu_processes.map((p) => (
                  <li
                    key={p.pid}
                    className="flex items-baseline justify-between gap-3 font-mono text-[0.7rem]"
                  >
                    <span className="truncate text-muted">
                      {/* `pid` 0 is every process that is not POM's, added up, with no name. */}
                      {p.pid === 0 ? t("dashboard.otherGpuProcesses") : <>{p.name} <span className="text-faint">#{p.pid}</span></>}
                    </span>
                    <span className="shrink-0 text-fg">{megabytes(p.used_mb)}</span>
                  </li>
                ))}
              </ul>
            </Accordion>
          )}
          </div>
        </>
      )}
    </motion.div>
  );
}

/**
 * The same card, in a window - what the double click on the Flow opens.
 *
 * It reuses the frame of `Modal`: it closes by Esc, by click outside and by the button,
 * and scrolls inside itself when the machine reports more than fits on the screen.
 */
export function PeerDetailDialog({
  peer,
  group,
  otherRanges,
  onClose,
  perf,
  speed,
}: {
  peer: PeerView | null;
  group: string;
  otherRanges: [number, number][];
  onClose: () => void;
  perf?: ChatTelemetryPeerPerf;
  speed?: PeerSpeed;
}) {
  const { t } = useI18n();
  return (
    <Modal
      open={peer != null}
      size="xl"
      titleHidden
      title={
        peer
          ? t("dashboard.machineDetails", { hostname: peer.node?.hostname || peer.short_id })
          : ""
      }
      onClose={onClose}
      footer={
        <Button variant="ghost" onClick={onClose}>
          {t("common.close")}
        </Button>
      }
    >
      {peer && (
        <div className="text-fg">
          <PeerDetailCard peer={peer} index={0} group={group} otherRanges={otherRanges} variant="dialog" perf={perf} speed={speed} />
        </div>
      )}
    </Modal>
  );
}
