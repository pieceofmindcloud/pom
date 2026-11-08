/**
 * Cutting a whole model into tensor parallel shards (development mode).
 *
 * Pipeline slices are gone: a pipeline deploy serves each range straight from
 * the whole model. Tensor parallelism still loads a pre-cut shard per rank,
 * so this screen survives for that one job, reached from the Models screen
 * and only while the node runs with `POM_DEV_MODE=1`.
 */
import { Cpu, Layers, Play, Scissors, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Field,
  Footnote,
  FullBleedHeader,
  Input,
  Panel,
  PanelHeader,
  ScrollX,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { bytes, layerRange, num } from "../lib/format";
import { useInvalidate, useLocalCatalog, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { isWholeModel, type TpSlicePlan } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { SlicingJobsPanel } from "../components/SlicingJobsPanel";

/**
 * A 200 that is not a plan is not a plan: an older build or a proxy that
 * swallowed the body used to take the window down on `plan.ranks.map`.
 */
function asPlan(value: unknown, invalidPlanMessage: string): TpSlicePlan {
  const plan = value as TpSlicePlan;
  if (!plan || !Array.isArray(plan.warnings) || !Array.isArray(plan.ranks)) {
    throw new Error(invalidPlanMessage);
  }
  return plan;
}

export function SliceModels() {
  const { t } = useI18n();
  const [params] = useSearchParams();
  const catalog = useLocalCatalog();
  const node = useNode();
  const toast = useToast();
  const invalidate = useInvalidate();
  const pomDevMode = node.data?.pom_dev_mode === true;

  const models = (catalog.data?.models ?? []).filter(isWholeModel);
  const [path, setPath] = useState(params.get("path") ?? "");
  const [tpSize, setTpSize] = useState("2");
  const [plan, setPlan] = useState<TpSlicePlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [planning, setPlanning] = useState(false);
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (!path && models.length > 0) setPath(models[0].path);
  }, [models, path]);

  const model = models.find((m) => m.path === path);
  const metadataComplete = catalog.data?.metadata_complete ?? true;
  const tpSizeNumber = Math.max(2, Math.min(64, Number.parseInt(tpSize, 10) || 2));

  useEffect(() => {
    if (!pomDevMode || !path) return;
    if (!metadataComplete) {
      setPlan(null);
      setPlanError(null);
      return;
    }
    if (model?.multipart_error) {
      setPlan(null);
      setPlanError(model.multipart_error);
      return;
    }
    const timer = window.setTimeout(async () => {
      setPlanning(true);
      try {
        const body = { model_path: path, mode: "tensor_parallel", tp_size: tpSizeNumber };
        setPlan(asPlan(await api.post<TpSlicePlan>("/api/ui/slice/plan", body), t("slice.invalidPlan")));
        setPlanError(null);
      } catch (e) {
        setPlan(null);
        setPlanError(e instanceof Error ? e.message : String(e));
      } finally {
        setPlanning(false);
      }
    }, 260);
    return () => window.clearTimeout(timer);
  }, [metadataComplete, model?.multipart_error, path, pomDevMode, tpSizeNumber]);

  async function start() {
    if (!path || !plan || plan.blocked || model?.multipart_error) return;
    setStarting(true);
    try {
      const { job_id } = await api.post<{ job_id: string }>("/api/ui/slice", {
        model_path: path,
        name: model?.name,
        mode: "tensor_parallel",
        tp_size: tpSizeNumber,
      });
      if (!job_id) throw new Error(t("slice.noJob"));
      toast.success(t("slice.started"), t("slice.startedDetail", { job: job_id.slice(0, 8) }));
      invalidate();
    } catch (e) {
      toast.fromError(t("slice.startFailed"), e);
    } finally {
      setStarting(false);
    }
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader title={t("slice.tpTitle")} subtitle={t("slice.tpSubtitle")} />

      <div className="min-h-0 min-w-0 flex-1 overflow-auto p-4 sm:p-5">
        <div className="mb-5">
          <SlicingJobsPanel />
        </div>

        {!pomDevMode && (
          <Panel>
            <Empty icon={<Cpu className="h-5 w-5" />} title={t("slice.tpDevOnly")}>
              {t("slice.tpDevOnlyDetail")}
            </Empty>
          </Panel>
        )}

        {pomDevMode && catalog.isLoading && <Skeleton className="h-64 w-full" />}
        {pomDevMode && catalog.isError && (
          <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
        )}
        {pomDevMode && catalog.data && models.length === 0 && (
          <Panel>
            <Empty icon={<Layers className="h-5 w-5" />} title={t("slice.empty")}>
              {t("slice.emptyDetail")}
            </Empty>
          </Panel>
        )}

        {pomDevMode && models.length > 0 && (
          <div className="grid gap-5 xl:grid-cols-[minmax(0,22rem)_minmax(0,1fr)]">
            <Panel>
              <PanelHeader title={t("slice.tpParts")} subtitle={t("slice.tpPartsDetail")} />
              <div className="space-y-4 px-5 py-4">
                <Field label={t("slice.file")}>
                  <Select value={path} onChange={(e) => setPath(e.target.value)}>
                    {models.map((m) => (
                      <option key={m.id} value={m.path}>
                        {m.name}
                      </option>
                    ))}
                  </Select>
                </Field>
                {model && (
                  <dl className="grid grid-cols-2 gap-3 text-[0.82rem]">
                    <div>
                      <dt className="eyebrow">{t("slice.size")}</dt>
                      <dd className="font-mono text-fg">
                        {!metadataComplete
                          ? t("slice.sizePending")
                          : model.multipart_error
                            ? t("slice.multipartSizeUnavailable")
                            : bytes(model.size_bytes)}
                      </dd>
                    </div>
                    <div>
                      <dt className="eyebrow">{t("slice.layers")}</dt>
                      <dd className="font-mono text-fg">
                        {plan ? num(plan.total_layers) : (model.total_layers ?? "—")}
                      </dd>
                    </div>
                  </dl>
                )}
                {model?.multipart_error && (
                  <p className="rounded-xl border border-amber/30 bg-amber/5 px-3.5 py-3 text-[0.75rem] leading-relaxed text-rose">
                    {model.multipart_error}
                  </p>
                )}
                <Field label={t("slice.tpSize")} hint={t("slice.tpSizeHint")}>
                  <Input
                    type="number"
                    min={2}
                    max={64}
                    value={tpSize}
                    inputMode="numeric"
                    aria-label={t("slice.tpSizeAria")}
                    onChange={(e) => setTpSize(e.target.value)}
                  />
                </Field>
                <p className="flex items-start gap-2 text-[0.75rem] leading-relaxed text-muted">
                  <Cpu className="mt-0.5 h-3.5 w-3.5 shrink-0 text-iris" />
                  {t("slice.modeTensorHint")}
                </p>
              </div>
            </Panel>

            <Panel className="self-start">
              <PanelHeader
                eyebrow={t("slice.preview")}
                title={t("slice.result")}
                subtitle={t("slice.tpPreviewDetail")}
                actions={
                  <Button
                    variant="primary"
                    icon={<Play className="h-3.5 w-3.5" />}
                    disabled={!plan || plan.blocked || Boolean(model?.multipart_error)}
                    loading={starting}
                    onClick={() => void start()}
                  >
                    {t("slice.now")}
                  </Button>
                }
              />

              {planError && (
                <div className="p-5">
                  <ErrorState title={t("slice.previewFailed")} error={planError} />
                </div>
              )}

              {!plan && !planError && (
                <div className="space-y-2 p-5">
                  {[0, 1].map((i) => (
                    <Skeleton key={i} className="h-12 w-full" />
                  ))}
                </div>
              )}

              {plan && (
                <>
                  <ScrollX>
                    <table className="w-full min-w-[30rem] border-collapse text-[0.82rem]">
                      <thead>
                        <tr className="border-b border-line text-left">
                          {["#", t("slice.layers"), t("slice.range"), t("slice.actualSize")].map((h) => (
                            <th key={h} className="px-5 py-2.5 font-medium text-muted">
                              {h}
                            </th>
                          ))}
                        </tr>
                      </thead>
                      <tbody>
                        {plan.ranks.map((r) => (
                          <tr key={r.rank} className="border-b border-line last:border-0 hover:bg-raised">
                            <td className="px-5 py-2.5 font-mono text-faint">
                              <span className="text-iris">
                                {r.rank + 1}of{plan.tp_size}
                              </span>
                              <span className="ml-2 text-faint">rank {r.rank}</span>
                            </td>
                            <td className="px-5 py-2.5 font-mono text-fg">{num(r.end_layer - r.start_layer + 1)}</td>
                            <td className="px-5 py-2.5 font-mono text-iris">{layerRange(r.start_layer, r.end_layer)}</td>
                            <td className="px-5 py-2.5 font-mono text-fg">{t("slice.tpAllLayersShort")}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </ScrollX>

                  <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
                    <span className="text-[0.82rem] text-muted">
                      {t("slice.covers", {
                        covered: plan.total_layers,
                        total: plan.total_layers,
                        size: bytes(plan.model_bytes),
                      })}
                    </span>
                    <Badge tone={plan.complete ? "aqua" : "amber"}>
                      {plan.complete ? t("slice.complete") : t("slice.partial")}
                    </Badge>
                    {planning && <span className="text-xs text-faint">{t("slice.recalculating")}</span>}
                  </div>

                  {plan.warnings.length > 0 && (
                    <ul className="space-y-2 border-t border-line px-5 py-3">
                      {plan.warnings.map((w, i) => (
                        <li key={i} className="flex items-start gap-2.5">
                          <TriangleAlert
                            className={`mt-0.5 h-3.5 w-3.5 shrink-0 ${plan.blocked ? "text-rose" : "text-amber"}`}
                          />
                          <span className="text-[0.82rem] leading-relaxed text-muted">{w}</span>
                        </li>
                      ))}
                    </ul>
                  )}

                  <Footnote className="border-t border-line px-5 py-3" icon={<Scissors className="h-3.5 w-3.5" />}>
                    {t("slice.footnote")}
                  </Footnote>
                </>
              )}
            </Panel>
          </div>
        )}
      </div>
    </div>
  );
}
