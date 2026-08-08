/**
 * Cutting a model into slices, by size.
 *
 * Slices are deliberately **not** equal: what decides a slice's size is the
 * memory of the machine that will hold it, and the machines are not alike. So
 * the list is one target in GB per slice, the last one may simply take "the
 * rest", and the rounding to whole layers is shown rather than hidden — the
 * asked-for size, the layer range it became and the real size, side by side.
 */
import { AnimatePresence, motion } from "framer-motion";
import { Layers, Play, Plus, Scissors, Trash2, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useNavigate } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Field,
  Footnote,
  Input,
  PageHeader,
  Panel,
  PanelHeader,
  ScrollX,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { bytes, layerRange, num } from "../lib/format";
import { useInvalidate, useLocalCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { SizePlan } from "../lib/types";

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
function asPlan(value: unknown): SizePlan {
  const plan = value as SizePlan;
  if (!plan || !Array.isArray(plan.ranges) || !Array.isArray(plan.warnings)) {
    throw new Error(
      "O nó respondeu 200 em /api/ui/slice/plan, mas o corpo não é um plano de fatiamento.",
    );
  }
  return plan;
}

export function SliceModels() {
  const [params] = useSearchParams();
  const catalog = useLocalCatalog();
  const toast = useToast();
  const navigate = useNavigate();
  const invalidate = useInvalidate();

  const models = catalog.data?.models ?? [];
  const [path, setPath] = useState(params.get("path") ?? "");
  const [slices, setSlices] = useState<SliceInput[]>([row("4"), row()]);
  const [plan, setPlan] = useState<SizePlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [planning, setPlanning] = useState(false);
  const [starting, setStarting] = useState(false);

  // Fall back to the first model found, so the screen is never a dead end.
  useEffect(() => {
    if (!path && models.length > 0) setPath(models[0].path);
  }, [models, path]);

  const model = models.find((m) => m.path === path);

  // The preview is remade on every keystroke; the node caches the tensor table
  // per file, so typing a size stays cheap.
  useEffect(() => {
    if (!path) return;
    const body = {
      model_path: path,
      slices: slices.map((s) => {
        const gb = Number.parseFloat(s.gb.replace(",", "."));
        return Number.isFinite(gb) && gb > 0 ? { target_mb: gb * 1024 } : {};
      }),
    };
    const timer = window.setTimeout(async () => {
      setPlanning(true);
      try {
        setPlan(asPlan(await api.post<SizePlan>("/api/ui/slice/plan", body)));
        setPlanError(null);
      } catch (e) {
        setPlan(null);
        setPlanError(e instanceof Error ? e.message : String(e));
      } finally {
        setPlanning(false);
      }
    }, 260);
    return () => window.clearTimeout(timer);
  }, [path, slices]);

  async function fillEqual(parts: number) {
    if (!path) return;
    try {
      const equal = asPlan(
        await api.post<SizePlan>("/api/ui/slice/plan", {
          model_path: path,
          equal_parts: parts,
        }),
      );
      // The shortcut only fills the form — the sizes stay editable, which is
      // the whole point of asymmetric slicing.
      setSlices(
        equal.ranges.map((r) => row((r.bytes / 1024 ** 3).toFixed(2).replace(".", ","))),
      );
    } catch (e) {
      toast.fromError("Não deu para dividir em partes iguais", e);
    }
  }

  async function start() {
    if (!path || !plan || plan.blocked) return;
    setStarting(true);
    try {
      const { job_id } = await api.post<{ job_id: string }>("/api/ui/slice", {
        model_path: path,
        name: model?.name,
        slices: slices.map((s) => {
          const gb = Number.parseFloat(s.gb.replace(",", "."));
          return Number.isFinite(gb) && gb > 0 ? { target_mb: gb * 1024 } : {};
        }),
      });
      if (!job_id) throw new Error("O nó aceitou o pedido mas não devolveu um job.");
      toast.success(
        "Fatiamento começou",
        `Acompanhe no sino ou em Modelos fatiados. Job ${job_id.slice(0, 8)}.`,
      );
      invalidate();
      navigate("/fatias/andamento");
    } catch (e) {
      toast.fromError("O fatiamento não começou", e);
    } finally {
      setStarting(false);
    }
  }

  return (
    <>
      <PageHeader
        title="Fatiar modelos"
        subtitle="Escolha o modelo, diga quanto cada fatia deve pesar e confira a faixa de camadas que sai disso antes de gravar."
      />

      {catalog.isLoading && <Skeleton className="h-64 w-full" />}
      {catalog.isError && (
        <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
      )}

      {catalog.data && models.length === 0 && (
        <Panel>
          <Empty icon={<Layers className="h-5 w-5" />} title="Nenhum modelo para fatiar">
            Não há nenhum GGUF completo nesta máquina. Confira as pastas em Configurações.
          </Empty>
        </Panel>
      )}

      {models.length > 0 && (
        <div className="grid gap-5 xl:grid-cols-[minmax(0,22rem)_minmax(0,1fr)]">
          <div className="space-y-5">
            <Panel>
              <PanelHeader title="Modelo de origem" />
              <div className="space-y-4 px-5 py-4">
                <Field label="Arquivo nesta máquina">
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
                      <dt className="eyebrow">Tamanho</dt>
                      <dd className="font-mono text-fg">{bytes(model.size_bytes)}</dd>
                    </div>
                    <div>
                      <dt className="eyebrow">Camadas</dt>
                      <dd className="font-mono text-fg">
                        {plan ? num(plan.total_layers) : (model.total_layers ?? "—")}
                      </dd>
                    </div>
                  </dl>
                )}
              </div>
            </Panel>

            <Panel>
              <PanelHeader
                title="Fatias"
                subtitle="Um alvo em GB por fatia. Deixe em branco para levar o resto do modelo."
                actions={
                  <Select
                    className="w-[8.5rem]"
                    value=""
                    onChange={(e) =>
                      e.target.value && void fillEqual(Number(e.target.value))
                    }
                    aria-label="Dividir em partes iguais"
                  >
                    <option value="">Dividir em…</option>
                    {[2, 3, 4, 5, 6, 8].map((n) => (
                      <option key={n} value={n}>
                        {n} iguais
                      </option>
                    ))}
                  </Select>
                }
              />
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
                      <Input
                        value={s.gb}
                        inputMode="decimal"
                        placeholder="o resto"
                        aria-label={`Tamanho da fatia ${i + 1} em GB`}
                        onChange={(e) =>
                          setSlices((list) =>
                            list.map((x) =>
                              x.id === s.id ? { ...x, gb: e.target.value } : x,
                            ),
                          )
                        }
                      />
                      <span className="w-6 shrink-0 text-[0.78rem] text-muted">GB</span>
                      <button
                        onClick={() =>
                          setSlices((list) => list.filter((x) => x.id !== s.id))
                        }
                        disabled={slices.length <= 1}
                        aria-label={`Remover fatia ${i + 1}`}
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
                  Mais uma fatia
                </Button>
              </div>
            </Panel>
          </div>

          <Panel className="self-start">
            <PanelHeader
              eyebrow="Prévia"
              title="O que sai disso"
              subtitle="O arredondamento é sempre para camada inteira, e aparece aqui: o alvo pedido, a faixa que saiu e o peso real."
              actions={
                <Button
                  variant="primary"
                  icon={<Play className="h-3.5 w-3.5" />}
                  disabled={!plan || plan.blocked}
                  loading={starting}
                  onClick={() => void start()}
                >
                  Fatiar agora
                </Button>
              }
            />

            {planError && (
              <div className="p-5">
                <ErrorState title="A prévia não fechou" error={planError} />
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
                        {["#", "Alvo", "Camadas", "Faixa", "Peso real"].map((h) => (
                          <th key={h} className="px-5 py-2.5 font-medium text-muted">
                            {h}
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {plan.ranges.map((r) => (
                        <tr
                          key={r.index}
                          className="border-b border-line last:border-0 hover:bg-raised"
                        >
                          <td className="px-5 py-2.5 font-mono text-faint">
                            {r.index + 1}
                          </td>
                          <td className="px-5 py-2.5 text-muted">
                            {r.is_rest ? "o resto" : `${num(r.target_mb! / 1024, 2)} GB`}
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
                                pequena demais
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
                    Cobre{" "}
                    <span className="font-mono text-fg">
                      {plan.covered_layers}/{plan.total_layers}
                    </span>{" "}
                    camadas · {bytes(plan.model_bytes)} no total
                  </span>
                  <Badge tone={plan.complete ? "aqua" : "amber"}>
                    {plan.complete ? "modelo inteiro coberto" : "cobertura parcial"}
                  </Badge>
                  {planning && <span className="text-xs text-faint">recalculando…</span>}
                </div>

                {plan.warnings.length > 0 && (
                  <ul className="space-y-2 border-t border-line px-5 py-3">
                    {plan.warnings.map((w, i) => (
                      <li key={i} className="flex items-start gap-2.5">
                        <TriangleAlert
                          className={`mt-0.5 h-3.5 w-3.5 shrink-0 ${plan.blocked ? "text-rose" : "text-amber"}`}
                        />
                        <span className="text-[0.82rem] leading-relaxed text-muted">
                          {w}
                        </span>
                      </li>
                    ))}
                  </ul>
                )}

                <Footnote
                  className="border-t border-line px-5 py-3"
                  icon={<Scissors className="h-3.5 w-3.5" />}
                >
                  A maior parte do tempo é antes de gravar: empacotar o cabeçalho passa
                  duas vezes pelo tokenizer. A barra só vira percentual quando a escrita
                  começa.
                </Footnote>
              </>
            )}
          </Panel>
        </div>
      )}
    </>
  );
}
