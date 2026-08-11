import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import {
  Activity,
  CircleSlash,
  HardDrive,
  LayoutGrid,
  MemoryStick,
  RefreshCw,
  Route as RouteIcon,
  Zap,
} from "lucide-react";
import { useState } from "react";
import { Confirm } from "../components/Confirm";
import { MachineIcon, machineKind } from "../components/MachineIcon";
import { LayerRibbon, PipelineRail, pipelinePeers, trackOf } from "../components/PipelineRail";
import { ReleaseSlice } from "../components/ReleaseSlice";
import { ModelNetworkDemo } from "../components/ModelNetworkDemo";
import { ModelNetworkCanvas } from "../components/ModelNetworkCanvas";
import {
  Badge,
  Button,
  Dot,
  Empty,
  ErrorState,
  Footnote,
  KeyValue,
  Meter,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { devUpdate } from "../lib/api";
import {
  isServing,
  layerRange,
  megabytes,
  num,
  pct,
  serverStateLabel,
  serverStateTone,
  since,
} from "../lib/format";
import { useCluster, useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { PeerView } from "../lib/types";
import { useI18n } from "../i18n/provider";

/** O botão de atualizar e reiniciar, só visível quando o peer aceita o pedido. */
function UpdateNode({ nodeId, hostname, isSelf }: { nodeId: string; hostname: string; isSelf: boolean }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  async function run() {
    setBusy(true);
    try {
      await devUpdate(isSelf ? undefined : nodeId);
      toast.success(
        t("dashboard.updateNode", { hostname }),
        t("dashboard.updateDetail"),
      );
      invalidate();
    } catch (e) {
      toast.fromError(t("dashboard.updateFailed", { hostname }), e);
    } finally {
      setBusy(false);
      setOpen(false);
    }
  }

  return (
    <>
      <Button size="sm" variant="outline" onClick={() => setOpen(true)}>
        <RefreshCw className="h-3.5 w-3.5" />
        {t("dashboard.updateRestart")}
      </Button>
      <Confirm
        open={open}
        busy={busy}
        tone="primary"
        title={t("dashboard.updateTitle")}
        confirmLabel={t("dashboard.updateRestart")}
        onCancel={() => setOpen(false)}
        onConfirm={() => void run()}
        description={
          <>
            <p>
              {t("dashboard.updateDescription")}
            </p>
            <p className="mt-2 text-xs text-faint">
              {t("dashboard.updateWarning")}
            </p>
          </>
        }
      />
    </>
  );
}

type View = "minima" | "estendida";

function ViewSwitch({ view, onChange }: { view: View; onChange: (v: View) => void }) {
  const { t } = useI18n();
  const options: Array<{ id: View; label: string; icon: typeof RouteIcon }> = [
    { id: "minima", label: t("dashboard.flow"), icon: RouteIcon },
    { id: "estendida", label: t("dashboard.details"), icon: LayoutGrid },
  ];
  return (
    <div className="flex items-center gap-1 rounded-lg border border-line bg-raised p-1">
      {options.map((o) => {
        const active = view === o.id;
        const Icon = o.icon;
        return (
          <button
            key={o.id}
            onClick={() => onChange(o.id)}
            className={clsx(
              "relative flex h-7 items-center gap-1.5 rounded-md px-2.5 text-[0.8rem] font-medium",
              "transition-colors duration-150",
              active ? "text-fg" : "text-muted hover:text-fg",
            )}
          >
            {active && (
              <motion.span
                layoutId="view-switch"
                className="absolute inset-0 rounded-md bg-surface shadow-panel ring-1 ring-inset ring-line"
                transition={{ type: "spring", stiffness: 520, damping: 40 }}
              />
            )}
            <Icon className="relative h-3.5 w-3.5" />
            <span className="relative">{o.label}</span>
          </button>
        );
      })}
    </div>
  );
}

function ClusterStat({
  label,
  value,
  detail,
  tone = "neutral",
}: {
  label: string;
  value: string;
  detail?: string;
  tone?: "neutral" | "aqua" | "amber" | "rose" | "iris";
}) {
  const color = {
    neutral: "text-fg",
    aqua: "text-aqua",
    amber: "text-amber",
    rose: "text-rose",
    iris: "text-iris",
  }[tone];
  return (
    <div className="min-w-0 px-5 py-4">
      <div className="eyebrow mb-1.5">{label}</div>
      <div className={clsx("font-display text-xl font-bold tracking-tight", color)}>
        {value}
      </div>
      {detail && <div className="mt-0.5 truncate text-xs text-muted">{detail}</div>}
    </div>
  );
}

/** Everything one machine reports, laid out the same way for every machine. */
function PeerDetailCard({ peer, index }: { peer: PeerView; index: number }) {
  const { t } = useI18n();
  const track = trackOf(index);
  const n = peer.node;
  const system = n?.system;
  const kind = machineKind(system);
  const cfg = n?.config;
  const serving = isServing(n?.status.state);
  const gpu = system?.gpus[0];

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.26, delay: Math.min(index, 6) * 0.04 }}
      className="panel relative overflow-hidden"
    >
      <span className={clsx("absolute inset-x-0 top-0 h-[2px]", track.fill)} />

      <div className="flex flex-wrap items-start justify-between gap-3 border-b border-line px-5 py-4">
        <div className="flex min-w-0 items-start gap-3">
          <div className="mt-0.5 flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-line bg-raised">
            <MachineIcon system={system} />
          </div>
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <h3 className="truncate font-display text-[0.95rem] font-semibold text-fg">
                {n?.hostname || peer.short_id}
              </h3>
              {peer.is_self && (
                <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">
                  {t("dashboard.here")}
                </Badge>
              )}
            </div>
            <p className="truncate text-xs text-muted">{kind.label}</p>
            {n?.build_commit && (
              <p className="mt-0.5 truncate font-mono text-[0.62rem] text-faint">
                {n.build_commit}
              </p>
            )}
          </div>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1.5">
          <span className={clsx("font-mono text-sm font-semibold", track.text)}>
            {layerRange(peer.start_layer, peer.end_layer)}
          </span>
          <span className="flex items-center gap-1.5 text-[0.72rem] text-muted">
            <Dot
              tone={
                peer.unreachable ? "rose" : serverStateTone(n?.status.state)
              }
              pulse={serving}
            />
            {peer.unreachable ?? serverStateLabel(n?.status.state)}
          </span>
        </div>
      </div>

      {peer.unreachable ? (
        <div className="px-5 py-5 text-sm text-muted">
          {t("dashboard.unreachable", { error: peer.unreachable })}
        </div>
      ) : (
        <>
          <div className="grid gap-4 px-5 py-4 sm:grid-cols-2">
            <Meter
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
            {system?.gpu_kind === "nvidia" && gpu ? (
              <Meter
                label={t("dashboard.vram", { name: gpu.name.replace(/^NVIDIA\s+/i, "") })}
                value={gpu.vram_used_pct ?? null}
                tone={(gpu.vram_used_pct ?? 0) > 88 ? "rose" : "lime"}
                detail={t("dashboard.usedOf", {
                  used: megabytes(gpu.vram_used_mb),
                  total: megabytes(gpu.vram_total_mb),
                })}
              />
            ) : (
              <Meter
                label={t("dashboard.wiredMemory")}
                value={
                  system?.wired_mb != null && system.ram.total_mb > 0
                    ? (system.wired_mb / system.ram.total_mb) * 100
                    : null
                }
                tone="sky"
                detail={system?.wired_mb != null ? megabytes(system.wired_mb) : "—"}
              />
            )}
            <Meter
              label={t("dashboard.disk", { path: system?.disk.mount_point || "—" })}
              value={
                system && system.disk.total_mb > 0
                  ? ((system.disk.total_mb - system.disk.free_mb) / system.disk.total_mb) *
                    100
                  : null
              }
              tone="amber"
              detail={
                system ? megabytes(system.disk.free_mb) + " " + t("dashboard.free") : "—"
              }
            />
            <Meter
              label={t("dashboard.load", { count: system?.cpu_count ?? "—" })}
              value={
                system && system.cpu_count > 0
                  ? Math.min(100, (system.load_avg[0] / system.cpu_count) * 100)
                  : null
              }
              tone="iris"
              detail={
                system
                  ? system.load_avg.map((l) => num(l, 2)).join(" · ")
                  : "—"
              }
            />
          </div>

          {system?.gpu_note && (
            <p className="border-t border-line px-5 py-2.5 text-xs leading-relaxed text-faint">
              {system.gpu_note}
            </p>
          )}

          <dl className="grid gap-x-6 gap-y-4 border-t border-line px-5 py-4 sm:grid-cols-2">
            <KeyValue label={t("dashboard.model")}>
              {cfg?.model_label ? (
                <span className="font-mono">{cfg.model_label}</span>
              ) : (
                <span className="text-muted">{t("dashboard.noneLoaded")}</span>
              )}
            </KeyValue>
            <KeyValue label={t("dashboard.role")}>
              {cfg
                ? cfg.single_peer
                  ? t("dashboard.single")
                  : cfg.start_layer === 0
                    ? t("dashboard.prompt")
                    : cfg.end_layer + 1 >= cfg.total_layers
                      ? t("dashboard.tokens")
                      : t("dashboard.middle")
                : (n?.role ?? "—")}
            </KeyValue>
            <KeyValue label={t("dashboard.file")}>
              {cfg?.model_path ? (
                <span className="font-mono text-xs">{cfg.model_path}</span>
              ) : (
                "—"
              )}
            </KeyValue>
            <KeyValue label={t("dashboard.context")}>
              {cfg ? (
                <span className="font-mono text-xs">
                  n_ctx {num(cfg.n_ctx)} · n_batch {num(cfg.n_batch)} · {cfg.max_sessions}{" "}
                  {t("dashboard.session", { count: cfg.max_sessions })}
                </span>
              ) : (
                "—"
              )}
            </KeyValue>
            <KeyValue label={t("dashboard.inference")}>
              {peer.inference_url ? (
                <span className="font-mono text-xs">{peer.inference_url}</span>
              ) : (
                t("dashboard.notAdvertised")
              )}
            </KeyValue>
            <KeyValue label={t("dashboard.server")}>
              {n ? (
                <span className="text-xs">
                  {n.status.detail || serverStateLabel(n.status.state)}
                  {n.status.pid ? ` · pid ${n.status.pid}` : ""}
                  {n.status.restarts > 0 ? " · " + t("dashboard.restarts", { count: n.status.restarts }) : ""}
                </span>
              ) : (
                "—"
              )}
            </KeyValue>
          </dl>

          {/* Only on this machine's own card: the slice is released by the node
              that owns the memory, never by a coordinator reaching in. */}
          {peer.is_self && cfg && (
            <ReleaseSlice startLayer={cfg.start_layer} endLayer={cfg.end_layer} />
          )}

          {n?.dev_update && (
            <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
              <Footnote icon={<RefreshCw className="h-3.5 w-3.5" />} className="min-w-0 flex-1">
                {t("dashboard.devUpdate")}
              </Footnote>
              <UpdateNode
                nodeId={peer.node_id}
                hostname={n.hostname || peer.short_id}
                isSelf={peer.is_self}
              />
            </div>
          )}

          {system && system.gpu_processes.length > 0 && (
            <div className="border-t border-line px-5 py-3">
              <div className="eyebrow mb-2">{t("dashboard.gpuProcesses")}</div>
              <ul className="space-y-1">
                {system.gpu_processes.map((p) => (
                  <li
                    key={p.pid}
                    className="flex items-baseline justify-between gap-3 font-mono text-xs"
                  >
                    <span className="truncate text-muted">
                      {p.name} <span className="text-faint">#{p.pid}</span>
                    </span>
                    <span className="shrink-0 text-fg">{megabytes(p.used_mb)}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </>
      )}
    </motion.div>
  );
}

export function Dashboard() {
  const { t } = useI18n();
  const [view, setView] = useState<View>("minima");
  const [selected, setSelected] = useState<string | null>(null);
  const cluster = useCluster();
  const node = useNode();
  const demo = new URLSearchParams(window.location.search).get("demo") === "network";

  const c = cluster.data;
  const peers = c ? pipelinePeers(c) : [];
  const serving = peers.filter((p) => isServing(p.node?.status.state)).length;
  const inFlight = peers.reduce((sum, p) => sum + p.load, 0);

  return (
    <>
      <PageHeader
        title={t("dashboard.title")}
        subtitle={
          c?.active_model
            ? t("dashboard.activeModel", { model: c.active_model })
            : t("dashboard.subtitle")
        }
        actions={<ViewSwitch view={view} onChange={setView} />}
      />

      {cluster.isError && (
        <ErrorState
          title={t("dashboard.nodeNoResponse")}
          error={cluster.error}
          onRetry={() => void cluster.refetch()}
        />
      )}

      {cluster.isLoading && (
        <div className="space-y-4">
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-56 w-full" />
        </div>
      )}

      {c && (
        <div className="space-y-5">
          {demo && <ModelNetworkDemo />}
          {!demo && <ModelNetworkCanvas cluster={c} />}
          <Panel className="grid grid-cols-2 divide-x divide-y divide-line sm:grid-cols-4 sm:divide-y-0">
            <ClusterStat
              label={t("dashboard.machines")}
              value={num(c.peers.length)}
              detail={num(serving) + " " + t("dashboard.serving")}
              tone="iris"
            />
            <ClusterStat
              label={t("dashboard.coverage")}
              value={
                c.total_layers > 0
                  ? pct((c.covered_layers / c.total_layers) * 100)
                  : "—"
              }
              detail={
                c.total_layers > 0
                  ? t("dashboard.layers", { covered: c.covered_layers, total: c.total_layers })
                  : t("dashboard.noModelAssigned")
              }
              tone={c.complete ? "aqua" : "amber"}
            />
            <ClusterStat
              label={t("dashboard.model")}
              value={c.active_model || "—"}
              detail={c.complete ? t("dashboard.closedPipeline") : t("dashboard.incompletePipeline")}
              tone={c.complete ? "aqua" : "amber"}
            />
            <ClusterStat
              label={t("dashboard.inFlight")}
              value={num(inFlight)}
              detail={
                node.data ? t("dashboard.thisNode", { count: num(node.data.load) }) : t("dashboard.requestsNow")
              }
              tone={inFlight > 0 ? "amber" : "neutral"}
            />
          </Panel>

          {!c.complete && c.total_layers > 0 && (
            <div className="flex items-start gap-3 rounded-xl border border-amber/30 bg-amber/[0.07] px-4 py-3.5">
              <CircleSlash className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
              <p className="text-sm leading-relaxed text-muted">
                {t("dashboard.missingLayers", { count: c.total_layers - c.covered_layers })}
              </p>
            </div>
          )}

          <AnimatePresence mode="wait">
            {view === "minima" ? (
              <motion.div
                key="minima"
                initial={{ opacity: 0, y: 8 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -8 }}
                transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
              >
                <Panel>
                  <PanelHeader
                    eyebrow={t("dashboard.executionOrder")}
                    title={t("dashboard.requestPath")}
                    subtitle={t("dashboard.requestPathDetail")}
                    actions={
                      <Badge tone={c.complete ? "aqua" : "amber"}>
                        <Activity className="h-3 w-3" />
                        {c.complete ? t("dashboard.ready") : t("dashboard.incompletePipeline")}
                      </Badge>
                    }
                  />
                  {/* The flow is the whole point of this view: it takes the
                      height rather than leaving the screen half empty. */}
                  <div className="flex min-h-[min(30rem,calc(100vh-24rem))] flex-col justify-center px-5 py-7">
                    <PipelineRail
                      cluster={c}
                      selected={selected}
                      onSelect={(id) => {
                        setSelected(id);
                        setView("estendida");
                      }}
                    />
                  </div>
                </Panel>
              </motion.div>
            ) : (
              <motion.div
                key="estendida"
                initial={{ opacity: 0, y: 8 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -8 }}
                transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
                className="space-y-5"
              >
                <Panel className="px-5 py-4">
                  <LayerRibbon cluster={c} />
                </Panel>

                {peers.length === 0 ? (
                  <Panel>
                    <Empty
                      icon={<MemoryStick className="h-5 w-5" />}
                      title={t("dashboard.noMachines")}
                    >
                      {t("dashboard.noMachinesDetail")}
                    </Empty>
                  </Panel>
                ) : (
                  <div className="grid gap-5 xl:grid-cols-2">
                    {peers.map((p, i) => (
                      <PeerDetailCard key={p.node_id} peer={p} index={i} />
                    ))}
                  </div>
                )}

                <Footnote
                  icon={<Zap className="h-3.5 w-3.5" />}
                >
                  {t("dashboard.sampled", { time: since(node.data?.system.sampled_at) })}
                </Footnote>
              </motion.div>
            )}
          </AnimatePresence>
        </div>
      )}

      {c && c.peers.length === 0 && (
        <Panel className="mt-5">
          <Empty icon={<HardDrive className="h-5 w-5" />} title={t("dashboard.emptyNetwork")}>
            {t("dashboard.emptyNetworkDetail")}
          </Empty>
        </Panel>
      )}
    </>
  );
}
