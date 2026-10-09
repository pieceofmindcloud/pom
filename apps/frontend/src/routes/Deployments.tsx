/**
 * Tudo que está implantado no cluster, e o que dá para mexer sem refazer.
 *
 * Uma implantação liga um conjunto de máquinas a um modelo, de uma forma
 * (fatias, modelo inteiro ou tensor parallelism) e com uma configuração por
 * máquina. O mesmo modelo pode estar no ar mais de uma vez — em máquinas
 * diferentes, ou com parâmetros diferentes —, e antes desta tela isso era
 * invisível: o painel mostrava uma pipeline só, deduzida do nome do modelo.
 *
 * Duas palavras que a tela **não** pode confundir, porque não são a mesma:
 *
 * * **no ar** — há máquina viva anunciando esta implantação.
 * * **padrão** — é esta que atende quem pede só `"qwen3:8b"`, sem o sufixo
 *   `@<deploy_id>`. Uma implantação pode estar no ar sem ser a padrão, e a
 *   padrão pode estar fora do ar (registrada, mas com as máquinas caídas).
 *
 * Reimplantar aqui é só mexer em parâmetro: as máquinas recarregam o mesmo
 * arquivo com outra configuração, mantendo o `deploy_id`. Trocar de máquina ou
 * de faixa continua sendo assunto da tela de implantar, que é onde há o que
 * planejar e o que copiar.
 */
import { useMemo, useState } from "react";
import { ChevronDown, ChevronRight, Layers, RefreshCw, Star, Terminal, X } from "lucide-react";
import {
  activateDeployment,
  redeployDeployment,
  type DeploymentRow,
} from "../lib/api";
import { useCluster, useDeployments, useInvalidate, useJobs, useNode } from "../lib/queries";
import { engineOfParams, inferenceServiceCatalog } from "../lib/inferenceServices";
import { useToast } from "../lib/toast";
import { layerRange } from "../lib/format";
import { DeploymentInspector } from "../components/DeploymentInspector";
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
import { useI18n } from "../i18n/provider";

/** Que versão de que implantação está aberta no inspetor da direita. */
type Inspected = { deployId: string; version: DeploymentVersion };

/** A reimplantação em edição à direita: de que implantação, a partir de que valores. */
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
 * Reimplantar: o mesmo inspetor da tela de implantar, editável, à direita da
 * lista, mais os dois ajustes que são de verdade por máquina.
 *
 * Os campos vêm preenchidos com a versão escolhida (a ativa, por padrão) - que
 * é o que o registro guarda -, então mexer num campo é mexer num valor real, e
 * não preencher um formulário do zero torcendo para acertar o resto. Um só
 * inspetor nas duas telas é o que impede uma propriedade nova de entrar numa e
 * faltar na outra.
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

/** Um resumo curto dos parâmetros que quase sempre mudam entre versões. Com o
 *  motor de um plugin, os números do llama.cpp não se aplicam: o resumo diz o
 *  motor, o contexto, as sessões e as configurações dele. */
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
              {/* A linha inteira é o gesto de "mostre-me com o que ela subiu";
                  o botão de reimplantar fica fora dela, porque é outro ato. */}
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
        {/* Coluna estreita e ações que não quebram: o rótulo partido ao meio
            deixava o ícone solto à esquerda, e a linha inteira crescia. */}
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

export function Deployments() {
  const { t } = useI18n();
  const { data, isLoading } = useDeployments();
  const node = useNode();
  const pomDevMode = node.data?.pom_dev_mode === true;
  /*
   * A versão aberta no inspetor da direita. Guarda a versão inteira, e não só
   * o número: o registro de uma versão não muda depois de gravado, então uma
   * cópia aqui não envelhece, e o inspetor não precisa reencontrar a linha a
   * cada refresh da lista.
   */
  const [inspected, setInspected] = useState<Inspected | null>(null);
  const engineCatalog = useEngineCatalog();
  const inspectedEngine = engineOfParams(inspected?.version.params.inference_service, engineCatalog);
  // O lado direito mostra uma coisa de cada vez: a versão lida ou a reimplantação.
  const [editing, setEditing] = useState<Editing | null>(null);
  const inspect = (next: Inspected | null) => {
    setEditing(null);
    setInspected(next);
  };
  const edit = (deployId: string, version: DeploymentVersion | undefined, fallback: RuntimeParams, fromVersion: number) => {
    setInspected(null);
    setEditing({ deployId, seed: version?.params ?? fallback, fromVersion: version?.version ?? fromVersion });
  };
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
          ) : rows.length === 0 ? (
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
              O mesmo inspetor da tela de implantar, em somente leitura. O cache
              KV não entra: ele é da instalação e do momento, não da versão -
              mostrar o valor de hoje ao lado do que subiu ontem seria inventar
              um registro que ninguém guardou.
            */}
            <DeploymentInspector
              value={inspected.version.params}
              readOnly
              showInferenceMode
              pomDevMode={pomDevMode}
              showHiddenStateDtype={inspected.version.params.hidden_state_dtype != null}
              inferenceService={inspectedEngine ?? undefined}
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
