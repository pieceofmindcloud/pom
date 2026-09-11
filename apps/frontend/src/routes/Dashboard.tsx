import clsx from "clsx";
import { motion } from "framer-motion";
import {
  LayoutGrid,
  MemoryStick,
  PanelRightClose,
  Plus,
  Route as RouteIcon,
  Zap,
} from "lucide-react";
import { useMemo, useState } from "react";
import { LayerRibbon, PipelineRail } from "../components/PipelineRail";
import { PeerDetailCard } from "../components/PeerDetailCard";
import { DeploymentInspector } from "../components/DeploymentInspector";
import { DeploymentLogDock } from "../components/DeploymentLogDock";
import { UndeployPipeline } from "../components/UndeployPipeline";
import { AddNodeDialog } from "../components/AddNodeDialog";
import { ModelNetworkCanvas } from "../components/ModelNetworkCanvas";
import { Button, Empty, ErrorState, Footnote, Panel, Skeleton } from "../components/ui";
import { canSeePeerDetails } from "../lib/access";
import { isServing, layerRange, num, pct, since } from "../lib/format";
import { networkPeers } from "../lib/networkPeers";
import { useCluster, useDeployments, useInvalidate, useNetworkRtt, useNode } from "../lib/queries";
import { useProfile } from "../lib/profile";
import type { PeerView } from "../lib/types";
import { useI18n } from "../i18n/provider";

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
  progress,
  tone = "neutral",
}: {
  label: string;
  value: string;
  detail?: string;
  progress?: number | null;
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
    <div className="min-w-0 px-3 py-2">
      <div className="eyebrow mb-1">{label}</div>
      <div className={clsx("font-display text-lg font-bold tracking-tight", color)}>
        {value}
      </div>
      {detail && <div className="mt-0.5 truncate text-xs text-muted">{detail}</div>}
      {progress != null && (
        <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-line" aria-label={`${label}: ${value}`}>
          <div className={clsx("h-full rounded-full", tone === "amber" ? "bg-amber" : "bg-aqua")} style={{ width: `${Math.max(0, Math.min(100, progress))}%` }} />
        </div>
      )}
    </div>
  );
}

/** O rótulo de um dos dois grupos de cards da aba Detalhes. */
function GroupHeading({
  id,
  title,
  detail,
  count,
}: {
  id: string;
  title: string;
  detail?: string;
  count: number;
}) {
  return (
    <div className="mb-3 flex flex-wrap items-baseline gap-x-3 gap-y-1">
      <h2 id={id} className="font-display text-sm font-semibold text-fg">
        {title}
      </h2>
      <span className="font-mono text-xs text-faint">{num(count)}</span>
      {detail && <p className="min-w-0 flex-1 text-xs text-muted">{detail}</p>}
    </div>
  );
}

/** Faixas das *outras* máquinas do pipeline, que é o que `RemoveNode` compara. */
function otherRangesFor(pipeline: PeerView[], peer: PeerView): [number, number][] {
  return pipeline
    .filter((o) => o.node_id !== peer.node_id)
    .map((o) => [o.start_layer, o.end_layer] as [number, number]);
}

export function Dashboard() {
  const { t } = useI18n();
  const [view, setView] = useState<View>("minima");
  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [showProperties, setShowProperties] = useState(true);
  const cluster = useCluster();
  const node = useNode();
  const deployments = useDeployments();
  const invalidateAll = useInvalidate();
  const { profile } = useProfile();
  const isAdmin = profile === "admin";
  // Sem os Detalhes sobra o Fluxo, e uma vista só não é uma escolha: o seletor
  // sai da tela junto com a aba que ele abria.
  const detailed = canSeePeerDetails(profile);

  const c = cluster.data;
  // Duas listas: quem serve o modelo ativo, na ordem do pipeline, e o resto da
  // rede. A aba Detalhes mostra as duas - uma máquina conectada e sem fatia é
  // exatamente a que não aparece em lugar nenhum quando só se olha o pipeline.
  const { pipeline: peers, others } = c
    ? networkPeers(c)
    : { pipeline: [] as PeerView[], others: [] as PeerView[] };
  const group = c?.active_model ?? "";
  const modelDeployments = deployments.data?.deployments.filter(
    (row) => row.deployment.model.label === group,
  ) ?? [];
  // A principal may have old and current copies of the same model in its
  // local history. The API marks the current one explicitly; taking the first
  // row made the properties toggle depend on deploy_id ordering and could
  // leave `activeVersion` empty for an otherwise valid deployment.
  const activeDeployment = modelDeployments.find((row) => row.is_default && row.serving)?.deployment
    ?? modelDeployments.find((row) => row.serving)?.deployment
    ?? modelDeployments.find((row) => row.is_default)?.deployment
    ?? modelDeployments[0]?.deployment;
  const activeVersion = activeDeployment?.versions.find(
    (version) => version.version === activeDeployment.active_version,
  ) ?? activeDeployment?.versions.at(-1);
  const activeDeployId =
    activeDeployment?.deploy_id
    ?? peers.find((peer) => peer.node?.config?.deploy_id)?.node?.config?.deploy_id;
  const rttNodeIds = useMemo(() => {
    if (!c) return [];
    return Array.from(new Set(
      [...c.routing_edges, ...c.candidate_edges]
        .flatMap((edge) => [edge.from_node_id, edge.to_node_id]),
    )).sort();
  }, [c]);
  const networkRtt = useNetworkRtt(rttNodeIds, c != null && Boolean(c.active_model));

  const serving = peers.filter((p) => isServing(p.node?.status.state)).length;
  const inFlightKnown = peers.every((p) => p.load != null);
  const inFlight = inFlightKnown ? peers.reduce((sum, p) => sum + (p.load ?? 0), 0) : null;
  // As máquinas que a desimplantação vai alcançar, na ordem do pipeline. O nó
  // re-varre o cluster antes de agir; esta lista é só o que a confirmação
  // mostra, para o operador ver de quem está falando.
  const undeployTargets = peers
    .filter((p) => p.model === group && p.total_layers > 0)
    .map((p) => ({
      node_id: p.node_id,
      hostname: p.node?.hostname || p.short_id,
      start_layer: p.start_layer,
      end_layer: p.end_layer,
    }))
    .sort((a, b) => a.start_layer - b.start_layer);

  if (cluster.isError) {
    return <div className="p-6"><ErrorState title={t("dashboard.nodeNoResponse")} error={cluster.error} onRetry={() => void cluster.refetch()} /></div>;
  }

  if (cluster.isLoading || !c) {
    return <div className="grid h-full place-items-center p-6"><div className="w-full max-w-xl space-y-4"><Skeleton className="h-16 w-full" /><Skeleton className="h-[32rem] w-full" /></div></div>;
  }

  // Quantas máquinas o modelo ativo depende e este nó não conseguiu ler.
  const coverageUnconfirmed =
    (c.model_inventory ?? []).find((entry) => entry.model_id === c.active_model)
      ?.unreadable_nodes?.length ?? 0;
  const localNode = node.data
    ?? c.peers.find((peer) => peer.is_self || peer.node_id === c.self_id)?.node
    ?? null;
  const localModel = localNode?.active_model || localNode?.config?.model_label || null;
  const localConfig = localNode?.config;
  const localSlice = localConfig
    && localConfig.total_layers > 0
    && localConfig.start_layer <= localConfig.end_layer
    ? layerRange(localConfig.start_layer, localConfig.end_layer)
    : null;
  const localSliceValue = localNode == null
    ? t("dashboard.unknown")
    : localSlice ?? t("dashboard.noSlice");
  const localSliceDetail = localNode == null
    ? t("dashboard.localDataUnavailable")
    : localSlice
      ? localModel ?? t("dashboard.localModelUnavailable")
      : localModel
        ? t("dashboard.localSliceUnavailable")
        : t("dashboard.localNoSliceDetail");
  const localModelValue = localNode == null
    ? t("dashboard.unknown")
    : localModel ?? t("dashboard.noModelAssigned");
  const localModelDetail = localNode == null
    ? t("dashboard.localDataUnavailable")
    : localModel
      ? localNode.status.state === "idle"
        ? t("dashboard.localIdle")
        : isServing(localNode.status.state) ? t("dashboard.localServing") : t("dashboard.notServing")
      : t("dashboard.localNoSliceDetail");
  const localLoad = localNode && typeof localNode.load === "number" ? localNode.load : null;

  return <div className="flex h-full min-h-0 flex-col bg-ink">
    <div data-testid="dashboard-toolbar" className="relative z-30 flex shrink-0 flex-wrap items-stretch justify-between gap-2 border-b border-line bg-surface/95 px-3 py-1 shadow-panel backdrop-blur-md">
      <div className="grid min-w-0 flex-1 grid-cols-2 divide-x divide-y divide-line rounded-lg border border-line bg-raised/75 sm:flex sm:divide-y-0">
        {isAdmin ? (
          <>
            {/*
              A cobertura sai do que os pares anunciam no gossip, não da leitura de
              cada um: uma máquina cujo endereço morreu continua contribuindo
              camadas. Verde de "completo" enquanto metade do modelo está ilegível
              é o pior estado possível da tela - por isso o âmbar vence.
            */}
            <ClusterStat label={t("dashboard.coverage")} value={c.total_layers > 0 ? pct((c.covered_layers / c.total_layers) * 100) : "—"} detail={coverageUnconfirmed > 0 ? t("dashboard.coverageUnconfirmed", { count: coverageUnconfirmed }) : c.total_layers > 0 ? t("dashboard.layers", { covered: c.covered_layers, total: c.total_layers }) : t("dashboard.noModelAssigned")} progress={c.total_layers > 0 ? (c.covered_layers / c.total_layers) * 100 : null} tone={c.complete && coverageUnconfirmed === 0 ? "aqua" : "amber"} />
            <ClusterStat label={t("dashboard.machines")} value={num(c.peers.length)} detail={num(serving) + " " + t("dashboard.serving")} tone="iris" />
            <ClusterStat label={t("dashboard.model")} value={c.active_model || "—"} detail={c.complete ? t("dashboard.closedPipeline") : t("dashboard.incompletePipeline")} tone={c.complete ? "aqua" : "amber"} />
            <ClusterStat label={t("dashboard.inFlight")} value={inFlight == null ? t("dashboard.unknown") : num(inFlight)} detail={inFlight == null ? t("dashboard.inFlightUnavailable") : t("dashboard.thisNode", { count: num(node.data?.load) })} tone={inFlight == null ? "amber" : inFlight > 0 ? "amber" : "neutral"} />
          </>
        ) : (
          <>
            {/*
              Node and infer profiles own a local fact, not the administrator's
              cluster verdict. A missing local response is explicit; it never
              becomes an empty model, an incomplete pipeline, or load zero.
            */}
            <ClusterStat label={t("dashboard.localSlice")} value={localSliceValue} detail={localSliceDetail} tone={localNode == null ? "amber" : localSlice ? "aqua" : "neutral"} />
            <ClusterStat label={t("dashboard.model")} value={localModelValue} detail={localModelDetail} tone={localNode == null ? "amber" : localModel ? "aqua" : "neutral"} />
            <ClusterStat label={t("dashboard.localLoad")} value={localLoad == null ? t("dashboard.unknown") : num(localLoad)} detail={localLoad == null ? t("dashboard.localDataUnavailable") : t("dashboard.localLoadDetail")} progress={localLoad == null ? null : Math.min(100, (localLoad / 8) * 100)} tone={localLoad == null ? "amber" : localLoad > 0 ? "amber" : "neutral"} />
          </>
        )}
      </div>
      <div className="flex items-center gap-2">
        {isAdmin && (
          <>
            <Button size="sm" variant="outline" onClick={() => setAdding(true)}>
              <Plus className="h-3.5 w-3.5" />
              {t("deploy.addNode")}
            </Button>
            {/* Ao lado de "Adicionar nó" porque são o mesmo nível de decisão: uma
                soma uma máquina à pipeline, a outra desfaz a pipeline inteira. Só
                aparece quando há uma no ar - um botão que só sabe recusar é ruído. */}
            {undeployTargets.length > 0 && (
              <UndeployPipeline
                group={group}
                deployId={activeDeployId}
                targets={undeployTargets}
              />
            )}
          </>
        )}
        {detailed && <ViewSwitch view={view} onChange={setView} />}
      </div>
    </div>

    <div className="flex min-h-0 flex-1">
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        {!detailed || view === "minima" ? (
          <div className="min-h-0 flex-1"><ModelNetworkCanvas cluster={c} rttMeasurements={networkRtt.data?.measurements} pipelineCost={networkRtt.data?.pipeline_cost ? { legsMs: networkRtt.data.pipeline_cost.legs_ms, computeMs: networkRtt.data.pipeline_cost.compute_ms } : undefined} pipelineRoutes={networkRtt.data?.pipeline_routes?.map((route) => ({ nodeIds: route.node_ids, legsMs: route.legs_ms, computeMs: null, rttMs: route.rtt_ms }))} propertiesOpen={showProperties} onToggleProperties={activeVersion ? () => setShowProperties((value) => !value) : undefined} /></div>
        ) : (
          <div className="min-h-0 flex-1 overflow-y-auto p-4 sm:p-6">
        {c.peers.length === 0 ? (
          <Panel><Empty icon={<MemoryStick className="h-5 w-5" />} title={t("dashboard.noMachines")}>{t("dashboard.noMachinesDetail")}</Empty></Panel>
        ) : (
          <div className="space-y-6">
            {peers.length > 0 && (
              <section aria-labelledby="detalhes-pipeline">
                {/* O título só aparece quando há um segundo grupo para separar:
                    com a rede inteira dentro da implantação, ele seria uma
                    legenda para a única coisa na tela. */}
                {others.length > 0 && (
                  <GroupHeading
                    id="detalhes-pipeline"
                    title={t("dashboard.groupDeployed")}
                    detail={group ? t("dashboard.groupDeployedDetail", { model: group }) : undefined}
                    count={peers.length}
                  />
                )}
                <div className="grid gap-5 xl:grid-cols-2">{peers.map((p, i) => <PeerDetailCard key={p.node_id} peer={p} index={i} group={c.active_model} otherRanges={otherRangesFor(peers, p)} />)}</div>
              </section>
            )}
            {others.length > 0 && (
              <section aria-labelledby="detalhes-fora">
                <GroupHeading
                  id="detalhes-fora"
                  title={t("dashboard.groupOutside")}
                  detail={t("dashboard.groupOutsideDetail")}
                  count={others.length}
                />
                <div className="grid gap-5 xl:grid-cols-2">{others.map((p, i) => <PeerDetailCard key={p.node_id} peer={p} index={i} group={c.active_model} otherRanges={otherRangesFor(peers, p)} />)}</div>
              </section>
            )}
          </div>
        )}
        <Footnote icon={<Zap className="h-3.5 w-3.5" />} className="mt-5">{t("dashboard.sampled", { time: since(node.data?.system.sampled_at) })}</Footnote>
          </div>
        )}

        {/* Keep the former execution-order rail available for a later surface; the
            graph above is now the primary operational view. */}
        <div className="hidden" aria-hidden="true"><LayerRibbon cluster={c} /><PipelineRail cluster={c} selected={selected} onSelect={setSelected} /></div>
        {isAdmin && <DeploymentLogDock />}
      </div>

      {activeVersion && showProperties && <aside className="hidden min-h-0 w-[21rem] shrink-0 overflow-y-auto border-l border-line bg-surface/80 lg:block" aria-label={t("dashboard.versionProperties")}><div className="sticky top-0 z-10 flex items-center justify-between border-b border-line bg-surface px-3 py-2"><div className="min-w-0"><p className="eyebrow">{t("dashboard.versionProperties")}</p><p className="truncate font-mono text-[0.68rem] text-muted">v{activeVersion.version} · {activeDeployment?.deploy_id.slice(0, 8)}</p></div><button type="button" aria-label={t("dashboard.hideProperties")} onClick={() => setShowProperties(false)} className="rounded p-1 text-muted hover:text-fg"><PanelRightClose className="h-3.5 w-3.5" /></button></div><DeploymentInspector value={activeVersion.params} readOnly showInferenceMode showHiddenStateDtype={activeVersion.params.hidden_state_dtype != null} /></aside>}
    </div>

    {isAdmin && (
      <AddNodeDialog
        open={adding}
        group={group}
        deployId={activeDeployId}
        peers={c.peers.map((p) => ({ node_id: p.node_id, short_id: p.short_id, hostname: p.node?.hostname ?? p.short_id }))}
        slices={[]}
        existingRanges={peers.map((p) => [p.start_layer, p.end_layer] as [number, number])}
        onClose={() => setAdding(false)}
        onDone={() => invalidateAll()}
      />
    )}
  </div>;
}
