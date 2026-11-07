/**
 * Everything that is deployed in the cluster, and what can be changed without redoing it.
 *
 * A deployment links a set of machines to a model, in one form (slices,
 * whole model or tensor parallelism) and with one configuration per
 * machine. The same model can be in service more than once - on different
 * machines, or with different parameters -, and before this screen that was
 * invisible: the dashboard showed a single pipeline, deduced from the model name.
 *
 * Two words the screen must **not** confuse, because they are not the same:
 *
 * * **in service** - there is a live machine announcing this deployment.
 * * **default** - the one that serves whoever asks for just `"qwen3:8b"`, without
 *   the `@<deploy_id>` suffix. A deployment can be in service without being the
 *   default, and the default can be out of service (registered, but with its machines down).
 *
 * Redeploying here is only changing parameters: the machines reload the same
 * file with another configuration, keeping the `deploy_id`. Changing machine or
 * range remains a matter for the deploy screen, which is where there is
 * something to plan and something to copy.
 */
import { useMemo, useState } from "react";
import { ChevronDown, ChevronRight, Layers, RefreshCw, Star, Terminal, X } from "lucide-react";
import {
  activateDeployment,
  redeployDeployment,
  type DeploymentRow,
} from "../lib/api";
import { useCluster, useDeployments, useImageDeployments, useInvalidate, useJobs, useNode } from "../lib/queries";
import { engineOfParams, inferenceServiceCatalog } from "../lib/inferenceServices";
import { useToast } from "../lib/toast";
import { layerRange } from "../lib/format";
import { DeploymentInspector } from "../components/DeploymentInspector";
import { NodeOverridesPanel } from "../components/NodeOverridesPanel";
import { UndeployPipeline } from "../components/UndeployPipeline";
import {
  Badge,
  Button,
  Empty,
  Input,
  Mono,
  Progress,
  ScrollX,
  Skeleton,
} from "../components/ui";
import type { DeploymentKind, DeploymentVersion, RuntimeParams } from "../lib/types";
import type { InferenceServiceEntry } from "../lib/inferenceServices";
import { machineParamsOf } from "../lib/machineParams";
import { ImageDeploymentsTable } from "../components/ImageDeploymentsTable";
import { useI18n } from "../i18n/provider";

/** Which version of which deployment is open in the inspector on the right. */
type Inspected = { deployId: string; version: DeploymentVersion };

/** A version as the list holds it now (the record of a version does not change, but its overrides do). */
function findVersion(
  deployments: { deployment: { deploy_id: string; versions: DeploymentVersion[] } }[],
  deployId: string,
  version: number,
): DeploymentVersion | undefined {
  return deployments
    .find((row) => row.deployment.deploy_id === deployId)
    ?.deployment.versions.find((v) => v.version === version);
}

/** The redeploy being edited on the right: of which deployment, starting from which values. */
type Editing = { deployId: string; seed: RuntimeParams; fromVersion: number };

const KIND_TONE: Record<DeploymentKind, "iris" | "aqua" | "sky"> = {
  shards: "iris",
  whole: "aqua",
  tensor_parallel: "sky",
  mixed: "iris",
};

function dateTimeLabel(iso: string, locale: string) {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "-";
  return new Intl.DateTimeFormat(locale, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}

/**
 * Redeploy: the same inspector as the deploy screen, editable, to the right of
 * the list, plus the two adjustments that really are per machine.
 *
 * The fields come filled with the chosen version (the active one, by default) -
 * which is what the registry keeps -, so changing a field is changing a real
 * value, and not filling a form from scratch hoping to get the rest right. A
 * single inspector on both screens is what keeps a new property from entering
 * one and missing from the other.
 */
function RedeployPanel({
  row,
  editing,
  pomDevMode,
  onClose,
}: {
  row: DeploymentRow;
  editing: Editing;
  pomDevMode: boolean;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const toast = useToast();
  const invalidate = useInvalidate();
  const { data: jobsData } = useJobs();
  const [params, setParams] = useState<RuntimeParams>(() => ({ ...editing.seed }));
  const engine = engineOfParams(params.inference_service, useEngineCatalog());
  const [perNode, setPerNode] = useState<
    Record<string, { n_gpu_layers?: number; offload_kqv?: boolean }>
  >(() =>
    Object.fromEntries(
      row.deployment.nodes.map((n) => [
        n.node_id,
        { n_gpu_layers: n.config.n_gpu_layers, offload_kqv: n.config.offload_kqv },
      ]),
    ),
  );
  const [busy, setBusy] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);
  const job = jobsData?.jobs.find((j) => j.id === jobId);

  async function submit() {
    setBusy(true);
    try {
      const res = await redeployDeployment(row.deployment.deploy_id, {
        params,
        per_node: perNode,
      });
      setJobId(res.job_id);
      toast.success(t("deployments.redeployStarted"), row.deployment.model.label);
      invalidate();
    } catch (e) {
      toast.fromError(t("deployments.redeployFailed"), e);
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="sticky top-0 z-10 flex items-start justify-between gap-2 border-b border-line bg-surface/95 px-2 py-2 backdrop-blur-sm">
        <div className="min-w-0">
          <p className="truncate text-[0.7rem] font-semibold text-fg">
            {t("deployments.redeployTitle", { model: row.deployment.model.label })}
          </p>
          <p className="truncate font-mono text-[0.6rem] text-faint">
            {row.deployment.deploy_id} · {t("deployments.version", { n: editing.fromVersion })}
          </p>
        </div>
        <button
          type="button"
          aria-label={t("deployments.closeProperties")}
          onClick={onClose}
          className="shrink-0 rounded p-1 text-muted hover:text-fg"
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
      <p className="border-b border-line px-2 py-1.5 text-[0.6rem] leading-relaxed text-faint">
        {t("deployments.redeployHint", { id: row.deployment.deploy_id })}
      </p>
      {job && (
        <div className="space-y-1.5 border-b border-line px-2 py-2">
          <div className="text-[0.7rem] text-fg">{job.message || job.step}</div>
          <Progress value={job.progress ?? 0} />
          {job.outputs.length > 0 && (
            <ul className="space-y-0.5 text-[0.6rem] text-muted">
              {job.outputs.map((output) => (
                <li key={output} className="break-path font-mono">{output}</li>
              ))}
            </ul>
          )}
        </div>
      )}

      <DeploymentInspector
        value={params}
        onChange={setParams}
        showInferenceMode
        pomDevMode={pomDevMode}
        showHiddenStateDtype={params.hidden_state_dtype != null}
        inferenceService={engine ?? undefined}
      />

      {/* GPU layers and the KV offload are llama.cpp's; a plugin's engine places its model itself. */}
      {!engine && (
        <div className="space-y-1.5 border-t border-line px-2 py-2">
          <p className="text-[0.65rem] font-semibold text-fg">{t("deployments.perMachine")}</p>
          {row.deployment.nodes.map((n) => (
            <div key={n.node_id} className="grid grid-cols-[minmax(0,1fr)_4.5rem_auto] items-center gap-2 text-[0.65rem]">
              <div className="min-w-0">
                <p className="truncate text-fg">{n.hostname || n.node_id.slice(0, 12)}</p>
                <p className="text-faint">{t("deployments.layers")} {layerRange(n.start_layer, n.end_layer)}</p>
              </div>
              <Input
                type="number"
                min={0}
                compact
                disabled={busy}
                aria-label={`${t("deploy.knob.n_gpu_layers")} ${n.hostname}`}
                title={t("deploy.knob.n_gpu_layers")}
                value={String(perNode[n.node_id]?.n_gpu_layers ?? "")}
                onChange={(e) =>
                  setPerNode((s) => ({
                    ...s,
                    [n.node_id]: { ...s[n.node_id], n_gpu_layers: Number(e.target.value) },
                  }))
                }
              />
              <label className="flex items-center gap-1 text-muted" title={t("deploy.offloadKqv")}>
                <input
                  type="checkbox"
                  aria-label={`${t("deploy.offloadKqv")} ${n.hostname}`}
                  checked={perNode[n.node_id]?.offload_kqv ?? true}
                  disabled={busy}
                  onChange={(e) =>
                    setPerNode((s) => ({
                      ...s,
                      [n.node_id]: { ...s[n.node_id], offload_kqv: e.target.checked },
                    }))
                  }
                />
                KQV
              </label>
            </div>
          ))}
        </div>
      )}

      <div className="sticky bottom-0 flex justify-end gap-2 border-t border-line bg-surface/95 px-2 py-2 backdrop-blur-sm">
        <Button variant="ghost" size="sm" onClick={onClose}>{t("common.cancel")}</Button>
        <Button size="sm" onClick={submit} loading={busy} icon={<RefreshCw className="h-3.5 w-3.5" />}>
          {t("deployments.redeployAction")}
        </Button>
      </div>
    </>
  );
}

/** The inference services the cluster announces, to name a deployment's
 *  plugin engine (Strata, ...) and show its settings; see `engineOfParams`. */
function useEngineCatalog() {
  const cluster = useCluster();
  return useMemo(() => inferenceServiceCatalog(cluster.data?.peers), [cluster.data]);
}

/** A short summary of the parameters that almost always change between versions. With
 *  a plugin's engine, the llama.cpp numbers do not apply: the summary states the
 *  engine, the context, the sessions and its settings. */
function versionSummary(p: RuntimeParams, engineName?: string): string {
  if (p.inference_service) {
    const settings = Object.entries(p.engine_settings ?? {}).map(([key, value]) => `${key}=${String(value)}`);
    return [engineName ?? p.inference_service, `ctx ${p.n_ctx}`, `sess ${p.max_sessions}`, ...settings].join(" · ");
  }
  const bits = [
    `ctx ${p.n_ctx}`,
    `leg ${p.max_new_tokens}`,
    `batch ${p.n_batch}`,
    `sess ${p.max_sessions}`,
  ];
  if (p.hidden_state_dtype) bits.push(p.hidden_state_dtype);
  return bits.join(" · ");
}

function VersionList({
  row,
  inspected,
  onInspect,
}: {
  row: DeploymentRow;
  inspected: Inspected | null;
  onInspect: (next: Inspected | null) => void;
}) {
  const { locale, t } = useI18n();
  const toast = useToast();
  const catalog = useEngineCatalog();
  const invalidate = useInvalidate();
  const [busy, setBusy] = useState<number | null>(null);
  const d = row.deployment;
  const versions = [...(d.versions ?? [])].sort((a, b) => b.version - a.version);

  async function redeployVersion(version: number) {
    setBusy(version);
    try {
      await redeployDeployment(d.deploy_id, { version });
      toast.success(
        t("deployments.versionRedeployStarted", { n: version }),
        d.model.label,
      );
      invalidate();
    } catch (e) {
      toast.fromError(t("deployments.redeployFailed"), e);
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="space-y-2 px-5 py-3">
      <div className="text-[0.82rem] text-fg">{t("deployments.versions")}</div>
      <p className="text-xs text-muted">{t("deployments.versionsHint")}</p>
      <ul className="divide-y divide-line rounded-md border border-line">
        {versions.map((v) => {
          const open =
            inspected?.deployId === d.deploy_id && inspected.version.version === v.version;
          return (
            <li
              key={v.version}
              className={`flex flex-wrap items-center gap-x-3 gap-y-1 pr-3 text-sm ${
                open ? "bg-iris/10" : ""
              }`}
            >
              {/* The whole row is the gesture of "show me what it came up with";
                  the redeploy button stays outside it, because it is another act. */}
              <button
                type="button"
                aria-label={t("deployments.versionPick", { n: v.version })}
                aria-pressed={open}
                onClick={() => onInspect(open ? null : { deployId: d.deploy_id, version: v })}
                className="flex min-w-0 flex-1 flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 text-left hover:bg-raised/60"
              >
                <Mono>{t("deployments.version", { n: v.version })}</Mono>
                {v.version === d.active_version && (
                  <Badge tone="lime">{t("deployments.activeVersion")}</Badge>
                )}
                <span className="text-xs text-muted">
                  <time dateTime={v.created_at}>
                    {dateTimeLabel(v.created_at, locale)}
                  </time>
                </span>
                <span className="text-xs text-faint">
                  {versionSummary(v.params, engineOfParams(v.params.inference_service, catalog)?.name)}
                </span>
              </button>
              <Button
                variant="ghost"
                disabled={busy !== null}
                onClick={() => redeployVersion(v.version)}
                icon={<RefreshCw className="h-3.5 w-3.5" />}
              >
                {t("deployments.redeployVersion")}
              </Button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function Row({
  row,
  inspected,
  onInspect,
  onRedeploy,
}: {
  row: DeploymentRow;
  inspected: Inspected | null;
  onInspect: (next: Inspected | null) => void;
  onRedeploy: () => void;
}) {
  const { locale, t } = useI18n();
  const toast = useToast();
  const invalidate = useInvalidate();
  const [expanded, setExpanded] = useState(false);
  const d = row.deployment;
  const versionCount = d.versions?.length ?? 1;
  const catalog = useEngineCatalog();
  const activeParams = d.versions?.find((v) => v.version === d.active_version)?.params ?? d.nodes[0]?.config;
  const engine = engineOfParams(activeParams?.inference_service, catalog);

  async function activate() {
    try {
      await activateDeployment(d.deploy_id);
      toast.success(t("deployments.activated", { model: d.model.label }));
      invalidate();
    } catch (e) {
      toast.fromError(t("deployments.activateFailed"), e);
    }
  }

  return (
    <>
      <tr className="border-t border-line align-top">
        <td className="px-2 py-3 align-middle">
          <button
            type="button"
            className="rounded p-1 text-muted hover:text-fg"
            aria-label={
              expanded
                ? t("deployments.collapseVersions")
                : t("deployments.expandVersions")
            }
            aria-expanded={expanded}
            onClick={() => setExpanded((s) => !s)}
          >
            {expanded ? (
              <ChevronDown className="h-4 w-4" />
            ) : (
              <ChevronRight className="h-4 w-4" />
            )}
          </button>
        </td>
        <td className="whitespace-nowrap px-5 py-3">
          <Mono>{d.deploy_id}</Mono>
          <div className="text-xs text-faint">
            {t("deployments.versionCount", { count: versionCount })}
          </div>
        </td>
        <td className="px-5 py-3">{d.model.label}</td>
        <td className="px-5 py-3">
          <div className="flex flex-wrap items-center gap-1.5">
            <Badge tone={KIND_TONE[d.kind]}>{t("deployments.kind." + d.kind)}</Badge>
            {engine && <Badge tone="sky">{engine.name}</Badge>}
          </div>
        </td>
        <td className="whitespace-nowrap px-5 py-3 text-muted">
          {t("deployments.nodeCount", { count: d.nodes.length })}
        </td>
        <td className="whitespace-nowrap px-5 py-3 text-muted">
          <time dateTime={d.activated_at}>
            {dateTimeLabel(d.activated_at, locale)}
          </time>
        </td>
        <td className="px-5 py-3">
          <div className="flex flex-wrap gap-1.5">
            {!row.serving ? (
              <Badge tone="neutral">{t("deployments.noAir")}</Badge>
            ) : row.idle === true ? (
              <Badge tone="amber">{t("deployments.peerIdle")}</Badge>
            ) : (
              <Badge tone="lime">{t("deployments.serving")}</Badge>
            )}
            {row.is_default && (
              <Badge tone="amber">{t("deployments.default")}</Badge>
            )}
          </div>
        </td>
        {/* Narrow column and actions that do not wrap: the label broken in half
            left the icon loose on the left, and the whole row grew. */}
        <td className="whitespace-nowrap px-5 py-3">
          <div className="flex flex-col items-start gap-1">
            <a
              href={`/deployments/logs?deploy_id=${encodeURIComponent(d.deploy_id)}`}
              className="inline-flex h-8 items-center justify-center gap-1.5 rounded-lg border border-line px-2.5 text-[0.8rem] font-medium text-muted transition-colors hover:border-line-strong hover:text-fg"
            >
              <Terminal className="h-3.5 w-3.5" />
              {t("deployments.viewLogs")}
            </a>
            <Button
              size="sm"
              variant="ghost"
              onClick={onRedeploy}
              icon={<RefreshCw className="h-3.5 w-3.5" />}
            >
              {t("deployments.redeployNewParams")}
            </Button>
            {!row.is_default && (
              <Button
                size="sm"
                variant="ghost"
                onClick={activate}
                icon={<Star className="h-3.5 w-3.5" />}
              >
                {t("deployments.makeDefault")}
              </Button>
            )}
            {row.serving && (
              <UndeployPipeline
                group={d.model.label}
                deployId={d.deploy_id}
                targets={d.nodes.map((n) => ({
                  node_id: n.node_id,
                  hostname: n.hostname,
                  start_layer: n.start_layer,
                  end_layer: n.end_layer,
                }))}
              />
            )}
          </div>
        </td>
      </tr>
      {expanded && (
        <tr className="border-t border-line bg-raised/40">
          <td colSpan={8} className="p-0">
            <VersionList row={row} inspected={inspected} onInspect={onInspect} />
          </td>
        </tr>
      )}
    </>
  );
}

/**
 * The machines of the version on the right: one button per machine, and the selected one's own values
 * (`NodeOverridesPanel`). A machine with its own values is marked.
 */
function MachinesSection({
  deployId,
  version,
  selected,
  onSelect,
  onSaved,
  engineCatalog,
}: {
  deployId: string;
  version: DeploymentVersion;
  selected: string | null;
  onSelect: (nodeId: string | null) => void;
  onSaved: () => void;
  engineCatalog: InferenceServiceEntry[];
}) {
  const { t } = useI18n();
  const node = version.nodes.find((n) => n.node_id === selected);
  return (
    <div className="grid gap-2 border-t border-line px-2 py-2">
      <p className="eyebrow">{t("deployments.nodesTitle")}</p>
      <div className="grid gap-1">
        {version.nodes.map((n) => {
          const active = selected === n.node_id;
          const own = Object.keys(version.overrides?.[n.node_id] ?? {}).length > 0;
          return (
            <button
              key={n.node_id}
              type="button"
              aria-pressed={active}
              onClick={() => onSelect(active ? null : n.node_id)}
              className={`flex items-center justify-between gap-2 rounded-md border px-2 py-1 text-left text-xs ${
                active ? "border-iris text-fg" : "border-line text-muted"
              }`}
            >
              <span className="min-w-0 truncate">
                {n.hostname || n.node_id.slice(0, 12)}{" "}
                <span className="font-mono text-faint">
                  {n.start_layer}-{n.end_layer}
                </span>
                <span className="block truncate text-[0.6rem] text-faint">
                  {engineOfParams(machineParamsOf(n, version).inference_service, engineCatalog)?.name ??
                    t("deploy.inferenceServiceLlama")}
                </span>
              </span>
              {own && <span className="shrink-0 text-[0.6rem] text-amber">{t("deployments.nodeOwn")}</span>}
            </button>
          );
        })}
      </div>
      {node && (
        <NodeOverridesPanel
          key={node.node_id}
          deployId={deployId}
          nodeId={node.node_id}
          hostname={node.hostname}
          versionParams={version.params}
          overrides={version.overrides?.[node.node_id] ?? {}}
          onSaved={onSaved}
        />
      )}
    </div>
  );
}

export function Deployments() {
  const images = useImageDeployments().data?.deployments ?? [];
  const { t } = useI18n();
  const { data, isLoading } = useDeployments();
  const node = useNode();
  const pomDevMode = node.data?.pom_dev_mode === true;
  /*
   * The version open in the inspector on the right. It keeps the whole version,
   * and not just the number: a version's record does not change once written, so
   * a copy here does not age, and the inspector does not need to find the row
   * again on every refresh of the list.
   */
  const [inspected, setInspected] = useState<Inspected | null>(null);
  const invalidate = useInvalidate();
  const engineCatalog = useEngineCatalog();
  // The right side shows one thing at a time: the version being read or the redeploy.
  const [editing, setEditing] = useState<Editing | null>(null);
  // The machine whose own values the properties panel shows (`NodeOverridesPanel`).
  const [machine, setMachine] = useState<string | null>(null);
  const inspect = (next: Inspected | null) => {
    setEditing(null);
    setMachine(null);
    setInspected(next);
  };
  const edit = (deployId: string, version: DeploymentVersion | undefined, fallback: RuntimeParams, fromVersion: number) => {
    setInspected(null);
    setEditing({ deployId, seed: version?.params ?? fallback, fromVersion: version?.version ?? fromVersion });
  };
  // The copy taken when the panel opened goes stale once a machine's values change: read the version from the list.
  const liveVersion = inspected
    ? findVersion(data?.deployments ?? [], inspected.deployId, inspected.version.version) ?? inspected.version
    : null;
  // The panel shows the version's parameters, or, once a machine is picked, that machine's own configuration.
  const shownMachine =
    liveVersion && machine ? liveVersion.nodes.find((candidate) => candidate.node_id === machine) ?? null : null;
  const shownParams = liveVersion ? (shownMachine ? machineParamsOf(shownMachine, liveVersion) : liveVersion.params) : null;
  const shownEngine = engineOfParams(shownParams?.inference_service, engineCatalog);
  const rows = [...(data?.deployments ?? [])].sort((a, b) => {
    const aTime = Date.parse(a.deployment.activated_at);
    const bTime = Date.parse(b.deployment.activated_at);
    if (Number.isNaN(aTime) && Number.isNaN(bTime)) return 0;
    if (Number.isNaN(aTime)) return 1;
    if (Number.isNaN(bTime)) return -1;
    return bTime - aTime;
  });

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <header className="shrink-0 border-b border-line bg-surface/75">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-3 px-3 py-3 sm:px-4">
          <div className="min-w-0">
            <h1 className="font-display text-lg font-semibold tracking-tight text-fg">
              {t("deployments.title")}
            </h1>
            <p className="mt-0.5 truncate text-[0.7rem] text-muted">
              {t("deployments.subtitle")}
            </p>
          </div>
          <p className="min-w-0 shrink text-right text-[0.7rem] text-faint">
            {t("deployments.listSubtitle")}
          </p>
        </div>
      </header>

      <div
        className={`flex min-h-0 min-w-0 flex-1 flex-col ${
          inspected || editing ? "lg:grid lg:grid-cols-[minmax(0,1fr)_minmax(19rem,24rem)]" : ""
        }`}
      >
        <div className="min-h-0 min-w-0 flex-1 overflow-auto">
          {isLoading ? (
            <div className="space-y-2 px-3 py-4 sm:px-4">
              <Skeleton className="h-8" />
              <Skeleton className="h-8" />
            </div>
          ) : rows.length === 0 && images.length === 0 ? (
            <Empty icon={<Layers className="h-5 w-5" />} title={t("deployments.emptyTitle")}>
              {t("deployments.emptyHint")}
            </Empty>
          ) : (
            <ScrollX>
              <table className="w-full min-w-[72rem] text-sm">
                <thead className="sticky top-0 z-10 bg-surface/95 backdrop-blur-sm">
                  <tr className="text-left text-xs text-faint">
                    <th className="px-2 py-2" aria-hidden />
                    <th className="px-5 py-2">{t("deployments.id")}</th>
                    <th className="px-5 py-2">{t("deployments.model")}</th>
                    <th className="px-5 py-2">{t("deployments.kindLabel")}</th>
                    <th className="px-5 py-2">{t("deployments.machines")}</th>
                    <th className="px-5 py-2">{t("deployments.lastActivated")}</th>
                    <th className="px-5 py-2">{t("deployments.state")}</th>
                    <th className="px-5 py-2">{t("deployments.actions")}</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <Row
                      key={r.deployment.deploy_id}
                      row={r}
                      inspected={inspected}
                      onInspect={inspect}
                      onRedeploy={() => {
                        const d = r.deployment;
                        edit(
                          d.deploy_id,
                          d.versions?.find((v) => v.version === d.active_version),
                          d.nodes[0].config,
                          d.active_version,
                        );
                      }}
                    />
                  ))}
                </tbody>
              </table>
              <ImageDeploymentsTable deployments={images} />
            </ScrollX>
          )}
        </div>

        {inspected && (
          <aside className="min-h-0 shrink-0 overflow-y-auto border-t border-line bg-surface/45 lg:border-l lg:border-t-0">
            <div className="sticky top-0 z-10 flex items-start justify-between gap-2 border-b border-line bg-surface/95 px-2 py-2 backdrop-blur-sm">
              <div className="min-w-0">
                <p className="truncate text-[0.7rem] font-semibold text-fg">
                  {t("deployments.versionProperties", { n: inspected.version.version })}
                </p>
                <p className="truncate font-mono text-[0.6rem] text-faint">
                  {inspected.deployId}
                </p>
                {shownMachine && (
                  <p className="truncate text-[0.65rem] text-iris">
                    {t("deployments.machineProperties", {
                      host: shownMachine.hostname || shownMachine.node_id.slice(0, 12),
                    })}
                    {" · "}
                    {t("deployments.machineEngine", {
                      engine: shownEngine?.name ?? t("deploy.inferenceServiceLlama"),
                    })}
                    {" · "}
                    <button type="button" onClick={() => setMachine(null)} className="underline">
                      {t("deployments.backToVersion")}
                    </button>
                  </p>
                )}
              </div>
              <button
                type="button"
                aria-label={t("deployments.closeProperties")}
                onClick={() => inspect(null)}
                className="shrink-0 rounded p-1 text-muted hover:text-fg"
              >
                <X className="h-3.5 w-3.5" />
              </button>
            </div>
            <p className="border-b border-line px-2 py-1.5 text-[0.6rem] leading-relaxed text-faint">
              {t("deployments.versionPropertiesHint")}
            </p>
            {/*
              The same inspector as the deploy screen, read-only. The KV cache
              is left out: it belongs to the installation and the moment, not to the
              version - showing today's value next to what came up yesterday would be
              inventing a record nobody kept.
            */}
            <DeploymentInspector
              value={shownParams ?? inspected.version.params}
              readOnly
              showInferenceMode
              pomDevMode={pomDevMode}
              showHiddenStateDtype={(shownParams ?? inspected.version.params).hidden_state_dtype != null}
              inferenceService={shownEngine ?? undefined}
            />
            <MachinesSection
              deployId={inspected.deployId}
              version={liveVersion ?? inspected.version}
              selected={machine}
              onSelect={setMachine}
              onSaved={() => invalidate()}
              engineCatalog={engineCatalog}
            />
            <div className="sticky bottom-0 flex justify-end border-t border-line bg-surface/95 px-2 py-2 backdrop-blur-sm">
              <Button
                size="sm"
                variant="outline"
                icon={<RefreshCw className="h-3.5 w-3.5" />}
                onClick={() => edit(inspected.deployId, inspected.version, inspected.version.params, inspected.version.version)}
              >
                {t("deployments.editAndRedeploy")}
              </Button>
            </div>
          </aside>
        )}

        {editing && (() => {
          const row = rows.find((r) => r.deployment.deploy_id === editing.deployId);
          return row ? (
            <aside className="min-h-0 shrink-0 overflow-y-auto border-t border-line bg-surface/45 lg:border-l lg:border-t-0">
              <RedeployPanel
                key={`${editing.deployId}:${editing.fromVersion}`}
                row={row}
                editing={editing}
                pomDevMode={pomDevMode}
                onClose={() => setEditing(null)}
              />
            </aside>
          ) : null;
        })()}
      </div>
    </div>
  );
}
