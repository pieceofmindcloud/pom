/**
 * Cutting a model into slices, by size.
 *
 * Slices are deliberately **not** equal: what decides a slice's size is the
 * memory of the machine that will hold it, and the machines are not alike. So
 * the list is one target in GB per slice, the last one may simply take "the
 * rest", and the rounding to whole layers is shown rather than hidden — the
 * asked-for size, the layer range it became and the real size, side by side.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { Cpu, Layers, Play, Plus, Scissors, Trash2, TriangleAlert } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
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
import {
  isWholeModel,
  type SizePlan,
  type SliceLayerOptions,
  type SliceMode,
  type SlicePlan,
  type TpSlicePlan,
} from "../lib/types";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";
import { SlicingJobsPanel } from "../components/SlicingJobsPanel";

/** One row of the builder. Empty means "the rest of the model". */
interface SliceInput {
  id: number;
  gb: string;
}

let nextId = 1;
const row = (gb = ""): SliceInput => ({ id: nextId++, gb });

/**
 * A 200 that is not a plan is not a plan.
 *
 * The preview is the one screen that renders straight off a POST body, so a
 * node that answers this route with something else - an older build, a proxy
 * that swallowed the body - used to take the whole window down on
 * `plan.ranges.map`. Refusing it here turns that into the error panel this
 * screen already knows how to show.
 */
function asPlan(value: unknown, invalidPlanMessage: string): SlicePlan {
  const plan = value as SlicePlan;
  if (
    !plan ||
    !Array.isArray(plan.warnings) ||
    (plan.mode === "tensor_parallel"
      ? !Array.isArray((plan as TpSlicePlan).ranks)
      : !Array.isArray((plan as SizePlan).ranges))
  ) {
    throw new Error(
      invalidPlanMessage,
    );
  }
  return plan;
}

export function SliceModels() {
  const { t, locale } = useI18n();
  const [params] = useSearchParams();
  const catalog = useLocalCatalog();
  const node = useNode();
  const toast = useToast();
  const invalidate = useInvalidate();
  // Tensor Parallelism é um recurso de desenvolvimento: o backend só o marca
  // disponível com `POM_DESENV=1` (`NodeInfo.pom_desenv`). Fora disso a tela
  // nem oferece a opção, e o modo nunca sai de "pipeline".
  const pomDesenv = node.data?.pom_desenv === true;

  // Fatiar é sobre modelos inteiros: fatias têm `shard_range` e não podem ser
  // fatiadas de novo.
  const models = (catalog.data?.models ?? []).filter(isWholeModel);
  const [path, setPath] = useState(params.get("path") ?? "");
  const [mode, setMode] = useState<SliceMode>("pipeline");
  const [tpSize, setTpSize] = useState("2");
  const [slices, setSlices] = useState<SliceInput[]>([row("4"), row()]);
  const [singleRange, setSingleRange] = useState(false);
  const [layerOptions, setLayerOptions] = useState<SliceLayerOptions | null>(null);
  const [rangeStart, setRangeStart] = useState(0);
  const [rangeEnd, setRangeEnd] = useState(0);
  const [plan, setPlan] = useState<SlicePlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [planning, setPlanning] = useState(false);
  const [starting, setStarting] = useState(false);

  // Fall back to the first model found, so the screen is never a dead end.
  useEffect(() => {
    if (!path && models.length > 0) setPath(models[0].path);
  }, [models, path]);

  // A flag pode chegar depois do primeiro render (a leitura de `/api/ui/node`
  // é assíncrona); um modo TP escolhido antes disso não pode sobreviver à
  // confirmação de que o desenvolvimento está desligado.
  useEffect(() => {
    if (!pomDesenv && mode === "tensor_parallel") setMode("pipeline");
  }, [pomDesenv, mode]);

  const model = models.find((m) => m.path === path);
  const metadataComplete = catalog.data?.metadata_complete ?? true;
  const tensorMode = mode === "tensor_parallel";
  const tpSizeNumber = Math.max(2, Math.min(64, Number.parseInt(tpSize, 10) || 2));

  useEffect(() => {
    if (!singleRange || !path || tensorMode) return;
    let alive = true;
    void api
      .post<SliceLayerOptions>("/api/ui/model-layer-options", { path })
      .then((options) => {
        if (!alive) return;
        setLayerOptions(options);
        setRangeStart(options.valid_starts?.[0] ?? 0);
        setRangeEnd(options.total_layers - 1);
      })
      .catch(() => {
        if (alive) setLayerOptions(null);
      });
    return () => {
      alive = false;
    };
  }, [path, singleRange, tensorMode]);

  const validStarts = layerOptions?.valid_starts ?? [];
  const validEndLayers = useMemo(() => {
    const total = layerOptions?.total_layers ?? plan?.total_layers ?? 0;
    if (!total) return [];
    const nextBoundary = validStarts.find((start) => start > rangeStart);
    const firstRunEnd = nextBoundary == null ? total - 1 : nextBoundary - 1;
    const ends = Array.from(
      { length: Math.max(0, firstRunEnd - rangeStart + 1) },
      (_, index) => rangeStart + index,
    );
    for (const start of validStarts) {
      if (start > rangeStart && start < total) ends.push(start - 1);
    }
    ends.push(total - 1);
    return [...new Set(ends)].sort((a, b) => a - b);
  }, [layerOptions, plan, rangeStart, validStarts]);

  useEffect(() => {
    if (validEndLayers.length > 0 && !validEndLayers.includes(rangeEnd)) {
      setRangeEnd(validEndLayers[validEndLayers.length - 1]);
    }
  }, [rangeEnd, validEndLayers]);

  const slicePayload = useMemo(
    () =>
      slices.map((s, i) => {
        if (i === slices.length - 1) return {};
        const gb = Number.parseFloat(s.gb.replace(",", "."));
        return Number.isFinite(gb) && gb > 0 ? { target_mb: gb * 1024 } : {};
      }),
    [slices],
  );

  // The preview is remade on every keystroke; the node caches the tensor table
  // per file, so typing a size stays cheap.
  useEffect(() => {
    if (!path) return;
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
    const body = tensorMode
      ? { model_path: path, mode, tp_size: tpSizeNumber }
      : singleRange
        ? { model_path: path, mode, range: { start_layer: rangeStart, end_layer: rangeEnd } }
        : { model_path: path, mode, slices: slicePayload };
    const timer = window.setTimeout(async () => {
      setPlanning(true);
      try {
        setPlan(asPlan(await api.post<SlicePlan>("/api/ui/slice/plan", body), t("slice.invalidPlan")));
        setPlanError(null);
      } catch (e) {
        setPlan(null);
        setPlanError(e instanceof Error ? e.message : String(e));
      } finally {
        setPlanning(false);
      }
    }, 260);
    return () => window.clearTimeout(timer);
  }, [
    metadataComplete,
    mode,
    model?.multipart_error,
    path,
    rangeEnd,
    rangeStart,
    slicePayload,
    singleRange,
    tensorMode,
    tpSizeNumber,
  ]);

  async function fillEqual(parts: number) {
    if (!path) return;
    try {
      const equal = asPlan(
        await api.post<SizePlan>("/api/ui/slice/plan", {
          model_path: path,
          mode: "pipeline",
          equal_parts: parts,
        }),
        t("slice.invalidPlan"),
      );
      // The shortcut only fills the form — the sizes stay editable, which is
      // the whole point of asymmetric slicing.
      if (equal.mode === "tensor_parallel") return;
      setSlices(
        equal.ranges.map((r) =>
          r.is_rest ? row() : row((r.bytes / 1024 ** 3).toFixed(2).replace(".", ",")),
        ),
      );
    } catch (e) {
      toast.fromError(t("slice.equalFailed"), e);
    }
  }

  async function start() {
    if (!path || !plan || plan.blocked || model?.multipart_error) return;
    setStarting(true);
    try {
      const body = tensorMode
        ? {
            model_path: path,
            name: model?.name,
            mode,
            tp_size: tpSizeNumber,
          }
        : singleRange
          ? {
              model_path: path,
              name: model?.name,
              mode,
              range: { start_layer: rangeStart, end_layer: rangeEnd },
            }
          : {
              model_path: path,
              name: model?.name,
              mode,
              slices: slicePayload,
            };
      const { job_id } = await api.post<{ job_id: string }>("/api/ui/slice", body);
      if (!job_id) throw new Error(t("slice.noJob"));
      toast.success(
        t("slice.started"),
        t("slice.startedDetail", { job: job_id.slice(0, 8) }),
      );
      invalidate();
    } catch (e) {
      toast.fromError(t("slice.startFailed"), e);
    } finally {
      setStarting(false);
    }
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader
        title={t("slice.title")}
        subtitle={t("slice.subtitle")}
      />

      <div className="min-h-0 min-w-0 flex-1 overflow-auto p-4 sm:p-5">
      <div className="mb-5">
        <SlicingJobsPanel />
      </div>

      {catalog.isLoading && <Skeleton className="h-64 w-full" />}
      {catalog.isError && (
        <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
      )}

      {catalog.data && models.length === 0 && (
        <Panel>
          <Empty icon={<Layers className="h-5 w-5" />} title={t("slice.empty")}>
            {t("slice.emptyDetail")}
          </Empty>
        </Panel>
      )}

      {models.length > 0 && (
        <div className="grid gap-5 xl:grid-cols-[minmax(0,22rem)_minmax(0,1fr)]">
          <div className="space-y-5">
            <Panel>
              <PanelHeader title={t("slice.source")} />
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
                {model?.multipart && (
                  <div className="space-y-3 rounded-xl border border-amber/30 bg-amber/5 px-3.5 py-3">
                    <div className="flex items-center gap-2">
                      <Badge tone="amber">{t("slice.multipartBadge")}</Badge>
                      <span className="text-[0.75rem] text-muted">
                        {t("slice.multipartParts", {
                          count: model.multipart_parts?.length ?? 0,
                        })}
                      </span>
                    </div>
                    <p className="text-[0.75rem] leading-relaxed text-muted">
                      {t("slice.multipartNeedsSlicing")}
                    </p>
                    {model.multipart_parts && model.multipart_parts.length > 0 && (
                      <ul className="space-y-1.5 border-t border-amber/20 pt-2 text-[0.7rem] text-faint">
                        {model.multipart_parts.map((part) => (
                          <li key={part.index} className="flex justify-between gap-3">
                            <span className="min-w-0 truncate">
                              {part.index + 1}. {part.path}
                            </span>
                            <span className="shrink-0 font-mono">{bytes(part.bytes)}</span>
                          </li>
                        ))}
                      </ul>
                    )}
                    {model.multipart_error && (
                      <p className="border-t border-amber/20 pt-2 text-[0.75rem] leading-relaxed text-rose">
                        {model.multipart_error}
                      </p>
                    )}
                  </div>
                )}
                {/*
                  Tensor Parallelism é um recurso de desenvolvimento: o botão
                  só existe quando o backend confirma `POM_DESENV=1`. Uma vista
                  só não é escolha, então sem a flag este grupo nem aparece e o
                  modo nunca sai de "pipeline" (mesmo padrão de
                  `DeploymentInspector`/`RuntimeParamsForm`).
                */}
                {pomDesenv && (
                  <div
                    role="tablist"
                    aria-label={t("slice.modeGroupAria")}
                    className="flex items-center gap-1 rounded-lg border border-line bg-raised p-1"
                  >
                    <button
                      type="button"
                      role="tab"
                      aria-selected={!tensorMode}
                      onClick={() => setMode("pipeline")}
                      className={clsx(
                        "flex flex-1 items-center justify-center gap-1.5 rounded-md px-2.5 py-1.5 text-[0.78rem] font-medium transition-colors",
                        !tensorMode
                          ? "bg-iris/15 text-fg ring-1 ring-inset ring-iris/25"
                          : "text-muted hover:text-fg",
                      )}
                    >
                      <Layers className="h-3.5 w-3.5" />
                      {t("slice.modePipeline")}
                    </button>
                    <button
                      type="button"
                      role="tab"
                      aria-selected={tensorMode}
                      onClick={() => setMode("tensor_parallel")}
                      className={clsx(
                        "flex flex-1 items-center justify-center gap-1.5 rounded-md px-2.5 py-1.5 text-[0.78rem] font-medium transition-colors",
                        tensorMode
                          ? "bg-iris/15 text-fg ring-1 ring-inset ring-iris/25"
                          : "text-muted hover:text-fg",
                      )}
                    >
                      <Cpu className="h-3.5 w-3.5" />
                      {t("slice.modeTensorShort")}
                    </button>
                  </div>
                )}
                {tensorMode && (
                  <p className="flex items-start gap-2 text-[0.75rem] leading-relaxed text-muted">
                    <Cpu className="mt-0.5 h-3.5 w-3.5 shrink-0 text-iris" />
                    {t("slice.modeTensorHint")}
                  </p>
                )}
                {!tensorMode && (
                  <>
                    <label className="flex cursor-pointer items-start gap-3 rounded-xl border border-line bg-raised/60 px-3.5 py-3 transition-colors hover:border-iris/50">
                      <input
                        type="checkbox"
                        checked={singleRange}
                        onChange={(e) => setSingleRange(e.target.checked)}
                        className="mt-0.5 h-4 w-4 accent-iris"
                        aria-label={t("slice.singleRange")}
                      />
                      <span className="min-w-0">
                        <span className="text-[0.82rem] font-medium text-fg">
                          {t("slice.singleRange")}
                        </span>
                        <span className="mt-1 block text-[0.75rem] leading-relaxed text-muted">
                          {t("slice.singleRangeHint")}
                        </span>
                      </span>
                    </label>
                    {singleRange && (
                  <div className="grid grid-cols-2 gap-3 rounded-xl border border-iris/20 bg-iris/5 px-3.5 py-3">
                    <Field label={t("slice.rangeStart")}>
                      <Select
                        value={String(rangeStart)}
                        disabled={validStarts.length === 0}
                        aria-label={t("slice.rangeStart")}
                        onChange={(e) => {
                          setRangeStart(Number(e.target.value));
                          setRangeEnd(0);
                        }}
                      >
                        {validStarts.map((start) => (
                          <option key={start} value={start}>L{start}</option>
                        ))}
                      </Select>
                    </Field>
                    <Field label={t("slice.rangeEnd")}>
                      <Select
                        value={String(rangeEnd)}
                        disabled={validEndLayers.length === 0}
                        aria-label={t("slice.rangeEnd")}
                        onChange={(e) => setRangeEnd(Number(e.target.value))}
                      >
                        {validEndLayers.map((end) => (
                          <option key={end} value={end}>L{end}</option>
                        ))}
                      </Select>
                    </Field>
                    {layerOptions && (
                      <p className="col-span-2 text-[0.7rem] leading-relaxed text-faint">
                        {layerOptions.rule}
                      </p>
                    )}
                  </div>
                    )}
                  </>
                )}
              </div>
            </Panel>

            <Panel>
              <PanelHeader
                title={tensorMode ? t("slice.tpParts") : t("slice.shards")}
                subtitle={tensorMode ? t("slice.tpPartsDetail") : t("slice.shardsDetail")}
                actions={!tensorMode && !singleRange ? (
                  <Select
                    className="w-[8.5rem]"
                    value=""
                    onChange={(e) =>
                      e.target.value && void fillEqual(Number(e.target.value))
                    }
                    aria-label={t("slice.equalAria")}
                  >
                    <option value="">{t("slice.equalPlaceholder")}</option>
                    {[2, 3, 4, 5, 6, 8].map((n) => (
                      <option key={n} value={n}>
                        {t("slice.equal", { count: n })}
                      </option>
                    ))}
                  </Select>
                ) : undefined}
              />
              {tensorMode ? (
                <div className="space-y-3 px-5 py-4">
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
                    {t("slice.tpAllLayers")}
                  </p>
                </div>
              ) : singleRange ? (
                <p className="px-5 py-4 text-[0.75rem] leading-relaxed text-muted">
                  {t("slice.singleRangeActive")}
                </p>
              ) : (
                <div className="space-y-2 px-5 py-4">
                <AnimatePresence initial={false}>
                  {slices.map((s, i) => (
                    <motion.div
                      key={s.id}
                      layout
                      initial={{ opacity: 0, height: 0 }}
                      animate={{ opacity: 1, height: "auto" }}
                      exit={{ opacity: 0, height: 0 }}
                      transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
                      className="flex items-center gap-2 overflow-hidden"
                    >
                      <span className="w-6 shrink-0 font-mono text-[0.72rem] text-faint">
                        {i + 1}
                      </span>
                      {i === slices.length - 1 ? (
                        <div className="flex min-h-9 min-w-0 flex-1 items-center rounded-lg border border-dashed border-line-strong px-3">
                          <Badge tone="aqua">{t("slice.rest")}</Badge>
                        </div>
                      ) : (
                        <>
                          <Input
                            value={s.gb}
                            inputMode="decimal"
                            placeholder={t("slice.sizeAria", { count: i + 1 })}
                            aria-label={t("slice.sizeAria", { count: i + 1 })}
                            onChange={(e) =>
                              setSlices((list) =>
                                list.map((x) =>
                                  x.id === s.id ? { ...x, gb: e.target.value } : x,
                                ),
                              )
                            }
                          />
                          <span className="w-6 shrink-0 text-[0.78rem] text-muted">GB</span>
                        </>
                      )}
                      <button
                        onClick={() =>
                          setSlices((list) => list.filter((x) => x.id !== s.id))
                        }
                        disabled={slices.length <= 1}
                        aria-label={t("slice.removeAria", { count: i + 1 })}
                        className="rounded p-1.5 text-faint transition-colors hover:text-rose disabled:pointer-events-none disabled:opacity-30"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </button>
                    </motion.div>
                  ))}
                </AnimatePresence>
                <Button
                  size="sm"
                  variant="ghost"
                  icon={<Plus className="h-3.5 w-3.5" />}
                  onClick={() => setSlices((l) => [...l, row()])}
                  disabled={slices.length >= 64}
                >
                  {t("slice.add")}
                </Button>
                </div>
              )}
            </Panel>
          </div>

          <Panel className="self-start">
            <PanelHeader
              eyebrow={t("slice.preview")}
              title={t("slice.result")}
              subtitle={tensorMode ? t("slice.tpPreviewDetail") : t("slice.previewDetail")}
              actions={
                <Button
                  variant="primary"
                  icon={<Play className="h-3.5 w-3.5" />}
                  disabled={!plan || plan.blocked}
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
                  <table className="w-full min-w-[36rem] border-collapse text-[0.82rem]">
                    <thead>
                      <tr className="border-b border-line text-left">
                        {["#", t("slice.target"), t("slice.layers"), t("slice.range"), t("slice.actualSize")].map((h) => (
                          <th key={h} className="px-5 py-2.5 font-medium text-muted">
                            {h}
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {plan.mode === "tensor_parallel" ? plan.ranks.map((r) => (
                        <tr
                          key={r.rank}
                          className="border-b border-line last:border-0 hover:bg-raised"
                        >
                          <td className="px-5 py-2.5 font-mono text-faint">
                            <span className="text-iris">{r.rank + 1}of{plan.tp_size}</span>
                            <span className="ml-2 text-faint">rank {r.rank}</span>
                          </td>
                          <td className="px-5 py-2.5 text-muted">{t("slice.tpRankCount", { size: plan.tp_size })}</td>
                          <td className="px-5 py-2.5 font-mono text-fg">{num(r.end_layer - r.start_layer + 1)}</td>
                          <td className="px-5 py-2.5 font-mono text-iris">
                            {layerRange(r.start_layer, r.end_layer)}
                          </td>
                          <td className="px-5 py-2.5 font-mono text-fg">{t("slice.tpAllLayersShort")}</td>
                        </tr>
                      )) : plan.ranges.map((r) => (
                        <tr
                          key={r.index}
                          className="border-b border-line last:border-0 hover:bg-raised"
                        >
                          <td className="px-5 py-2.5 font-mono text-faint">
                            {r.index + 1}
                          </td>
                          <td className="px-5 py-2.5 text-muted">
                            {r.is_rest ? t("slice.rest") : `${num(r.target_mb! / 1024, 2)} GB`}
                          </td>
                          <td className="px-5 py-2.5 font-mono text-fg">
                            {r.empty ? "—" : num(r.layers)}
                          </td>
                          <td className="px-5 py-2.5 font-mono text-iris">
                            {r.empty ? "—" : layerRange(r.start_layer, r.end_layer)}
                          </td>
                          <td className="px-5 py-2.5 font-mono text-fg">
                            {r.empty ? "—" : bytes(r.bytes)}
                            {r.too_small && (
                              <Badge tone="rose" className="ml-2">
                                {t("slice.tooSmall")}
                              </Badge>
                            )}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </ScrollX>

                <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
                  <span className="text-[0.82rem] text-muted">
                    {t("slice.covers", {
                      covered: plan.mode === "tensor_parallel" ? plan.total_layers : plan.covered_layers,
                      total: plan.total_layers,
                      size: bytes(plan.model_bytes),
                    })}
                  </span>
                  <Badge tone={plan.complete ? "aqua" : "amber"}>
                    {plan.complete ? t("slice.complete") : t("slice.partial")}
                  </Badge>
                  {planning && <span className="text-xs text-faint">{t("slice.recalculating")}</span>}
                </div>

                {plan.mode !== "tensor_parallel" && plan.required_head_bytes != null && (
                  <div className="mx-5 mb-3 rounded-xl border border-iris/20 bg-iris/5 px-3.5 py-3">
                    <p className="text-[0.75rem] leading-relaxed text-muted">
                      {t("slice.requiredHead", { size: bytes(plan.required_head_bytes) })}
                    </p>
                  </div>
                )}

                {plan.space_status && (
                  <div
                    role={plan.space_status === "insufficient" ? "alert" : "status"}
                    className={`mx-5 mb-3 rounded-xl border px-3.5 py-3 ${
                      plan.space_status === "insufficient"
                        ? "border-rose/30 bg-rose/5"
                        : plan.space_status === "tight"
                          ? "border-amber/30 bg-amber/5"
                          : "border-line bg-raised/50"
                    }`}
                  >
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <span className="text-[0.78rem] font-medium text-fg">
                        {t("slice.spaceTitle")}
                      </span>
                      <Badge tone={plan.space_status === "insufficient" ? "rose" : plan.space_status === "tight" ? "amber" : "aqua"}>
                        {t(`slice.space.${plan.space_status}`)}
                      </Badge>
                    </div>
                    <p className="break-path mt-1 text-[0.75rem] leading-relaxed text-muted">
                      {plan.destination_path}
                    </p>
                    <div className="mt-2 flex flex-wrap gap-x-4 gap-y-1 font-mono text-[0.7rem] text-faint">
                      <span>{t("slice.spaceAvailable", { value: bytes(plan.destination_free_bytes) })}</span>
                      <span>{t("slice.spaceRequired", { value: bytes(plan.model_bytes) })}</span>
                      {plan.space_margin_bytes != null && (
                        <span>{t("slice.spaceMargin", { value: bytes(Math.max(0, plan.space_margin_bytes)) })}</span>
                      )}
                    </div>
                  </div>
                )}

                {plan.warnings.length > 0 && (
                  <ul className="space-y-2 border-t border-line px-5 py-3">
                    {plan.warnings.map((w, i) => (
                      <li key={i} className="flex items-start gap-2.5">
                        <TriangleAlert
                          className={`mt-0.5 h-3.5 w-3.5 shrink-0 ${plan.blocked ? "text-rose" : "text-amber"}`}
                        />
                        <span className="text-[0.82rem] leading-relaxed text-muted">
                          {localizeServerText(locale, w)}
                        </span>
                      </li>
                    ))}
                  </ul>
                )}

                <Footnote
                  className="border-t border-line px-5 py-3"
                  icon={<Scissors className="h-3.5 w-3.5" />}
                >
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
