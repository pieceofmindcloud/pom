/**
 * Putting a model that already exists somewhere in the cluster into service.
 *
 * Two shapes of the same act, chosen at the top of the screen:
 *
 * * **A set of slices** — the original case, described in detail below: many
 *   machines, one range each, and every question about coverage and order is
 *   real.
 * * **A whole model** — one un-sliced GGUF on one machine. The node reads it as
 *   a single range covering every layer, so the plan that comes back has the
 *   same shape and the same route validates it; nothing here recomputes it.
 *   What the screen drops is the questions that have only one answer. There is
 *   no order to choose in a pipeline of one, and no subset of layers to tick:
 *   the coverage ribbon would be a solid bar and the "included" column a single
 *   permanently-ticked box, which is noise dressed as a choice.
 *
 * The rest of this comment is about the slice case, which is the one with
 * decisions in it.
 *
 * The screen opens on a decision the node made — who would serve which range,
 * and what has to be copied first — and every part of that decision is then
 * editable. Nothing here computes the plan in the browser: the suggestion comes
 * from `/api/ui/deploy/plan`, every edit is sent straight back to the same route
 * for validation, and pressing the button re-surveys the cluster server-side
 * before acting, because a machine can leave between looking and deciding.
 *
 * Three things the operator is choosing, and why they are the shape they are:
 *
 * * **Which layers.** A model is often sliced more than once, with different
 *   cuts — 0-4 / 5-31 next to 0-10 / 11-31. Every range anyone holds is a row
 *   with a checkbox, so mixing two slicings is possible and the ribbon above the
 *   table shows immediately what that leaves covered, uncovered or doubled.
 * * **Which machine.** Each selected row carries a target picker. Choosing a
 *   machine that already has another range swaps the two, because that is what
 *   the operator meant and a node can only serve one range.
 * * **The order.** The pipeline runs in layer order — `pipeline.rs` sweeps from
 *   layer 0 up, so whoever holds layer 0 receives every prompt. There is no
 *   second knob to turn: reordering the pipeline *is* reassigning the ranges,
 *   and the ↑ ↓ buttons do exactly that by swapping machines between
 *   neighbouring rows. The rail at the top shows the resulting order.
 *
 * The copies matter enough to stay loud. A 10 GB slice takes about seventeen
 * minutes on the real link, so "3 fatias, 12 GB" is the difference between a
 * deploy that finishes now and one that finishes after lunch.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { AnimatePresence, motion } from "framer-motion";
import {
  ArrowDown,
  ArrowRight,
  ArrowUp,
  Boxes,
  Check,
  CircleAlert,
  Download,
  Layers,
  MessageSquareText,
  Rocket,
  ServerCog,
  Sparkles,
  TriangleAlert,
  Wand2,
} from "lucide-react";
import { api } from "../lib/api";
import { useClusterCatalog, useInvalidate, useJobs } from "../lib/queries";
import { recallDeploy, rememberDeploy, usableSelection } from "../lib/deployMemory";
import { useToast } from "../lib/toast";
import { bytes, layerRange, megabytes, num } from "../lib/format";
import { Confirm } from "../components/Confirm";
import { trackOf } from "../components/PipelineRail";
import { wholeModelsOf } from "../lib/wholeModels";
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
  Progress,
  ScrollX,
  Select,
  Skeleton,
} from "../components/ui";
import type {
  DeployCandidate,
  DeployKind,
  DeployPlan,
  DeploySelection,
  DeployStep,
  RuntimeParams,
} from "../lib/types";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

const rangeKey = (start: number, end: number) => `${start}-${end}`;

/** The plan on screen, in the shape the routes accept it back.
 *
 *  A merged step is the union of its parts, so it has to expand back into the
 *  per-slice rows the screen edits — otherwise the plan could never match what
 *  the operator sees. */
function rowsOf(plan: DeployPlan | undefined): DeploySelection[] {
  return (plan?.steps ?? []).flatMap((s) =>
    s.merged
      ? s.parts.map((pt) => ({
          start_layer: pt.start_layer,
          end_layer: pt.end_layer,
          target_node_id: s.target_node_id,
          // Per-machine, so every part of a merged step carries the same pair.
          n_gpu_layers: s.n_gpu_layers,
          offload_kqv: s.offload_kqv,
        }))
      : [
          {
            start_layer: s.start_layer,
            end_layer: s.end_layer,
            target_node_id: s.target_node_id,
            n_gpu_layers: s.n_gpu_layers,
            offload_kqv: s.offload_kqv,
          },
        ],
  );
}

const sameRows = (a: DeploySelection[], b: DeploySelection[]) =>
  a.length === b.length &&
  a.every(
    (r, i) =>
      r.start_layer === b[i].start_layer &&
      r.end_layer === b[i].end_layer &&
      r.target_node_id === b[i].target_node_id,
  );

const byLayer = (a: DeploySelection, b: DeploySelection) =>
  a.start_layer - b.start_layer || a.end_layer - b.end_layer;

/** Which question the node answered. An older peer does not say, and an older
 *  peer only ever answers about shards. */
const kindOf = (plan: DeployPlan | undefined): DeployKind => plan?.kind ?? "shards";

/**
 * The include box.
 *
 * A native `<input type=checkbox>` was the obvious thing and the wrong one: the
 * unchecked state is painted by the platform, and against these surfaces it
 * lands as a pale filled square that reads as *disabled* — the opposite of what
 * an unticked row means. This is the same control drawn with the panel's own
 * border and accent.
 */
function Tick({
  on,
  label,
  onToggle,
}: {
  on: boolean;
  label: string;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={on}
      aria-label={label}
      onClick={onToggle}
      className={`flex h-[18px] w-[18px] items-center justify-center rounded-[5px] border transition-colors duration-150 ${
        on
          ? "border-iris bg-iris text-white"
          : "border-line-strong bg-ink text-transparent hover:border-iris/60"
      }`}
    >
      <Check className="h-3 w-3" strokeWidth={3} />
    </button>
  );
}

/**
 * One tick per layer of the model, coloured by the machine that would run it.
 *
 * This is the part that makes a mixed selection legible: a hole shows as a
 * hollow tick and a layer claimed by two slices shows as a striped one, so
 * "0-10 plus 5-31" is visibly wrong before the node has to say so in words.
 */
function Coverage({
  total,
  rows,
}: {
  /** `null` when no slice declares the model size and none starts at layer 0.
   *  There is no ribbon to draw then, and the plan is blocked anyway. */
  total: number | null;
  rows: DeploySelection[];
}) {
  const { t } = useI18n();
  if (!total || total <= 0) return null;

  const owner = new Array<number>(total).fill(-1);
  const doubled = new Array<boolean>(total).fill(false);
  rows.forEach((r, i) => {
    for (let l = r.start_layer; l <= Math.min(r.end_layer, total - 1); l++) {
      if (owner[l] === -1) owner[l] = i;
      else doubled[l] = true;
    }
  });

  const covered = owner.filter((o) => o !== -1).length;
  const overlapped = doubled.filter(Boolean).length;

  return (
    <div className="min-w-0">
      <div className="mb-2 flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <span className="eyebrow">{t("deploy.coverage")}</span>
        <span className="font-mono text-[0.72rem] text-muted">
          {covered}/{total}
          {covered < total && (
            <span className="ml-2 text-amber">{t("deploy.missing", { count: total - covered })}</span>
          )}
          {overlapped > 0 && (
            <span className="ml-2 text-rose">{t("deploy.overlapped", { count: overlapped })}</span>
          )}
        </span>
      </div>
      <div className="flex h-6 w-full gap-[2px] overflow-hidden rounded-md">
        {owner.map((o, l) => (
          <div
            key={l}
            title={
              doubled[l]
                ? t("deploy.overlapTitle", { layer: l })
                : o === -1
                  ? t("deploy.excludedTitle", { layer: l })
                  : t("deploy.layerTitle", { layer: l, range: rangeKey(rows[o].start_layer, rows[o].end_layer) })
            }
            className={
              doubled[l]
                ? "min-w-0 flex-1 rounded-[2px] bg-rose/30 ring-1 ring-inset ring-rose"
                : o === -1
                  ? "min-w-0 flex-1 rounded-[2px] bg-line ring-1 ring-inset ring-amber/50"
                  : `min-w-0 flex-1 rounded-[2px] ${trackOf(o).fill}`
            }
            style={{ opacity: o === -1 || doubled[l] ? 1 : 0.85 }}
          />
        ))}
      </div>
    </div>
  );
}

/**
 * The execution settings, in the order and wording the old "Ajustes avançados"
 * block used — the launch has always carried all of these, and the rewritten
 * panel simply had nowhere to type them.
 *
 * They are cluster-wide because the pipeline needs them to be: the context
 * window, the batch and the chunk decide the prompt budget the gateway enforces
 * on every request, and the sampling only ever runs on the last peer. The two
 * that are genuinely per-machine — GPU offload and where the KV cache lives —
 * are edited in the machines table instead, next to the machine they belong to.
 */
const KNOBS: Array<{
  key: keyof RuntimeParams;
  min: number;
  max?: number;
  step?: number;
}> = [
  {
    key: "n_gpu_layers",
    min: 0,
  },
  {
    key: "n_ctx",
    min: 512,
    max: 250000,
    step: 512,
  },
  {
    key: "n_batch",
    min: 32,
    step: 32,
  },
  { key: "max_new_tokens", min: 16 },
  {
    key: "max_sessions",
    min: 1,
    max: 16,
  },
  { key: "prefill_chunk_tokens", min: 32, step: 32 },
  { key: "temperature", min: 0, max: 2, step: 0.05 },
  { key: "top_p", min: 0, max: 1, step: 0.05 },
  { key: "top_k", min: 0 },
  { key: "repeat_penalty", min: 1, max: 2, step: 0.01 },
  { key: "repeat_last_n", min: 0 },
];

/// Só aparecem com a especulação ligada: fora dali são dois números sem efeito
/// nenhum, e a aba já tem onze campos.
const SPECULATIVE_KNOBS: Array<{
  key: keyof RuntimeParams;
  min: number;
  max?: number;
  step?: number;
}> = [
  { key: "speculative_k", min: 1, max: 16 },
  { key: "speculative_lookup_window", min: 1, max: 8 },
];

/// Só aparecem com o MTP ligado. `mtp_draft_tokens` tem o mesmo teto sensato
/// da largura do rascunho por lookup; `mtp_min_probability` é um piso de
/// confiança, então vive em [0,1] com passo de centésimo.
const MTP_KNOBS: Array<{
  key: keyof RuntimeParams;
  min: number;
  max?: number;
  step?: number;
}> = [
  { key: "mtp_draft_tokens", min: 1, max: 8 },
  { key: "mtp_min_probability", min: 0, max: 1, step: 0.01 },
];

function Switch({
  checked,
  onChange,
  label,
  hint,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  hint?: string;
  disabled?: boolean;
}) {
  return (
    <div className={`flex items-start gap-2.5 ${disabled ? "opacity-55" : ""}`}>
      <span className="mt-0.5">
        {disabled ? (
          <Tick on={false} label={label} onToggle={() => {}} />
        ) : (
          <Tick on={checked} label={label} onToggle={() => onChange(!checked)} />
        )}
      </span>
      <span className="min-w-0">
        <span className={`block text-[0.82rem] ${disabled ? "text-muted" : "text-fg"}`}>{label}</span>
        {hint && <span className="mt-0.5 block text-xs text-faint">{hint}</span>}
      </span>
    </div>
  );
}

function Terminus({ icon, label }: { icon: React.ReactNode; label: string }) {
  return (
    <div className="flex w-[76px] shrink-0 flex-col items-center gap-1.5 text-center">
      <div className="flex h-8 w-8 items-center justify-center rounded-full border border-dashed border-line-strong text-faint">
        {icon}
      </div>
      <span className="text-[0.72rem] text-muted">{label}</span>
    </div>
  );
}

function Arrow() {
  return (
    <div className="flex w-8 shrink-0 items-center justify-center self-center pb-5 text-faint">
      <ArrowRight className="h-4 w-4" />
    </div>
  );
}

/** The order the request will actually travel, read left to right. */
function OrderRail({
  rows,
  labelOf,
}: {
  rows: DeploySelection[];
  labelOf: (id: string) => string;
}) {
  const { t } = useI18n();
  return (
    <div className="-mx-1 overflow-x-auto px-1 pb-1">
      <div className="flex items-stretch gap-1 py-1">
        <Terminus icon={<MessageSquareText className="h-4 w-4" />} label={t("deploy.prompt")} />
        <Arrow />
        {rows.map((r, i) => {
          const track = trackOf(i);
          return (
            <div key={rangeKey(r.start_layer, r.end_layer)} className="flex items-stretch">
              <div className="relative flex w-[164px] shrink-0 flex-col gap-2 rounded-xl border border-line bg-surface p-3">
                <span
                  className={`absolute inset-x-3.5 top-0 h-[2px] rounded-full ${track.fill}`}
                />
                {/* Fixed height so the layer range sits on the same baseline in
                    every card, badge or no badge. */}
                <div className="flex h-5 items-center gap-1.5">
                  <span className="font-mono text-[0.7rem] text-faint">{t("deploy.orderPosition", { count: i + 1 })}</span>
                  {i === 0 && (
                    <Badge tone="iris" className="px-1.5 py-0 text-[0.62rem] leading-4">
                      {t("deploy.processesPrompt")}
                    </Badge>
                  )}
                </div>
                <div className={`font-mono text-[0.9rem] font-semibold ${track.text}`}>
                  {layerRange(r.start_layer, r.end_layer)}
                </div>
                <div className="flex min-w-0 items-center gap-1.5 border-t border-line pt-2">
                  <ServerCog className="h-3.5 w-3.5 shrink-0 text-faint" />
                  <span className="truncate text-[0.78rem] text-fg">
                    {r.target_node_id ? labelOf(r.target_node_id) : t("deploy.noMachine")}
                  </span>
                </div>
              </div>
              {i < rows.length - 1 && <Arrow />}
            </div>
          );
        })}
        <Arrow />
        <Terminus icon={<Sparkles className="h-4 w-4" />} label={t("deploy.response")} />
      </div>
    </div>
  );
}

/**
 * Slices or a whole model, and nothing in between.
 *
 * Two buttons rather than a dropdown: it is the first decision on the screen
 * and it changes everything under it, so both answers are readable without
 * opening anything.
 */
function KindPicker({
  value,
  onChange,
  labels,
}: {
  value: DeployKind;
  onChange: (k: DeployKind) => void;
  labels: Record<DeployKind, { label: string; hint: string }>;
}) {
  return (
    <div className="grid gap-2 sm:grid-cols-2">
      {(["shards", "whole"] as DeployKind[]).map((k) => {
        const on = value === k;
        return (
          <button
            key={k}
            type="button"
            role="radio"
            aria-checked={on}
            onClick={() => onChange(k)}
            className={`rounded-xl border px-4 py-3 text-left transition-colors duration-150 ${
              on
                ? "border-iris bg-iris/[0.08]"
                : "border-line bg-surface hover:border-line-strong"
            }`}
          >
            <span className={`block text-[0.86rem] font-medium ${on ? "text-fg" : "text-muted"}`}>
              {labels[k].label}
            </span>
            <span className="mt-1 block text-xs text-faint">{labels[k].hint}</span>
          </button>
        );
      })}
    </div>
  );
}

export function DeployModel() {
  const { t, locale } = useI18n();
  const cluster = useClusterCatalog();
  const [group, setGroup] = useState("");
  /** The whole model to deploy, when that is the question being asked. */
  const [wholeName, setWholeName] = useState("");
  /** `null` until the operator says — see `kind` below for what that means. */
  const [pickedKind, setPickedKind] = useState<DeployKind | null>(null);
  /** `null` means "whatever the node suggests" — the default, few-clicks path. */
  const [selection, setSelection] = useState<DeploySelection[] | null>(null);
  /** `null` means "the node's own defaults", which is what most deploys want. */
  const [params, setParams] = useState<RuntimeParams | null>(null);
  /** `null` means "the node's default" — merged units off. */
  const [merge, setMerge] = useState<boolean | null>(null);
  /** Half-typed numbers, so clearing a field does not snap back to a digit. */
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [asked, setAsked] = useState(false);
  const [running, setRunning] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);
  const toast = useToast();
  const invalidate = useInvalidate();
  const jobs = useJobs();

  /** Every slice set anyone in the cluster holds, named once. */
  const groups = useMemo(() => {
    const seen = new Set<string>();
    for (const peer of cluster.data?.peers ?? []) {
      for (const set of peer.shard_sets ?? []) seen.add(set.group);
    }
    return [...seen].sort();
  }, [cluster.data]);

  /** Every un-sliced model anyone in the cluster holds, named once. */
  const wholes = useMemo(() => wholeModelsOf(cluster.data?.peers), [cluster.data]);

  /**
   * Slices unless the operator says otherwise — except on a cluster that has
   * whole models and no slice sets at all, where "slices" opens on an empty
   * screen that looks like a broken deploy page rather than an unanswered
   * question. The operator's own choice always wins once made.
   */
  const kind: DeployKind =
    pickedKind ?? (groups.length === 0 && wholes.length > 0 ? "whole" : "shards");
  const isWhole = kind === "whole";

  const chosen = isWhole
    ? wholeName || wholes[0]?.name || ""
    : group || groups[0] || "";
  /** The whole model on screen, for the facts the plan does not carry. */
  const wholeModel = isWhole ? wholes.find((w) => w.name === chosen) : undefined;
  /**
   * Where the last deploy of this is remembered.
   *
   * Namespaced by kind because the two menus are different namespaces: a shard
   * set and a whole model can perfectly well share a name (they usually do —
   * the set is named after the model it was cut from), and one restoring the
   * other's machines would open the screen on a plan nobody chose.
   */
  const memoryKey = isWhole ? `whole:${chosen}` : chosen;

  // Edits go to the node for validation, but not on every keystroke: the survey
  // behind this route talks to every peer, and the settings are typed digit by
  // digit.
  const [settled, setSettled] = useState<DeploySelection[] | null>(null);
  const [settledParams, setSettledParams] = useState<RuntimeParams | null>(null);
  useEffect(() => {
    const t = setTimeout(() => {
      setSettled(selection);
      setSettledParams(params);
    }, 260);
    return () => clearTimeout(t);
  }, [selection, params]);

  const plan = useQuery({
    queryKey: [
      "deploy-plan",
      kind,
      chosen,
      settled ? JSON.stringify(settled) : "auto",
      settledParams ? JSON.stringify(settledParams) : "padrão",
      merge == null ? "merge-padrão" : `merge-${merge}`,
    ],
    queryFn: () =>
      api.post<DeployPlan>("/api/ui/deploy/plan", {
        group: chosen,
        kind,
        ...(settled ? { selection: settled } : {}),
        ...(settledParams ? { params: settledParams } : {}),
        // A whole model is one machine, so there is nothing to join and the
        // toggle is not on screen. Sending it anyway would be sending a value
        // the operator was never shown.
        ...(merge != null && !isWhole ? { merge } : {}),
      }),
    enabled: chosen.length > 0,
    staleTime: 10_000,
    placeholderData: keepPreviousData,
  });

  const p = plan.data;

  /**
   * Bring back the last deploy of this model, once we know what still exists.
   *
   * Deliberately after the first plan and not before it: the memory is from
   * another day, and restoring a range nobody holds or a machine that left
   * would open the screen on a blocked plan the operator never chose. The plan
   * is what says which of both are still real, so the pruning happens against
   * it (`usableSelection`) and what does not survive is simply not restored.
   *
   * Once per model per visit — after that the screen belongs to whatever the
   * operator is editing, and re-restoring would undo their work on every
   * refetch.
   */
  const restored = useRef<Set<string>>(new Set());
  useEffect(() => {
    if (!p || p.group !== chosen || kindOf(p) !== kind || restored.current.has(memoryKey)) return;
    restored.current.add(memoryKey);

    const memory = recallDeploy(memoryKey);
    if (!memory) return;

    const usable = usableSelection(
      memory.selection,
      p.available,
      p.machines.map((m) => m.node_id),
    );
    if (!usable) return;

    setSelection(usable.slice().sort(byLayer));
    if (memory.params) setParams(memory.params);
    if (!isWhole) setMerge(memory.merge);
  }, [p, chosen, kind, memoryKey, isWhole]);

  /** What is on screen right now: the operator's edit, or the suggestion. */
  const rows = useMemo(
    () => (selection ?? rowsOf(p)).slice().sort(byLayer),
    [selection, p],
  );

  /**
   * True while the plan in hand is still the answer to an older question.
   *
   * `isPlaceholderData` is exactly that and nothing else: react-query sets it
   * only while showing a previous key's data, so a background refetch of the
   * same selection does not make the screen blink. The debounce above is the
   * other half - between the keystroke and `settled` the query key has not
   * moved yet, so the plan looks current when it is not.
   */
  const syncing =
    plan.isPlaceholderData || settled !== selection || settledParams !== params;

  /** True while the node's answer still describes exactly these rows.
   *
   *  Order-insensitive on purpose. The screen keeps its rows in layer order and
   *  the node builds its answer its own way; comparing position by position
   *  made a plan that described exactly these rows look like a different plan,
   *  and everything below - the refusal, the coverage warning, the button -
   *  hung off that comparison. What matters is the set of (range, machine)
   *  pairs, not the sequence they arrived in. */
  const validated = !syncing && !!p && sameRows(rows, rowsOf(p).slice().sort(byLayer));

  /**
   * True when the node has answered about what is on screen right now.
   *
   * The explanations are gated on this rather than on `validated`, because the
   * moment the node refuses part of a selection is the moment its answer stops
   * matching the rows - and it is exactly then that the operator needs to read
   * the reason. Gating the reason on the match meant the screen went silent
   * precisely when it had something to say.
   */
  const answered = !!p && p.group === chosen && kindOf(p) === kind && !syncing;

  const labelOf = (id: string) =>
    p?.machines.find((m) => m.node_id === id)?.label ?? id.slice(0, 8);

  /** The settings in force: the operator's edit, or the node's own defaults. */
  const eff: RuntimeParams = params ?? p?.params ?? ({} as RuntimeParams);
  /** Merged units in force: the operator's toggle, or the node's default. */
  const effMerge = merge ?? p?.merge ?? false;

  /** The step that owns a row.
   *
   *  Plain steps match the row's range exactly; a merged step is the union of
   *  several ranges, so a row inside one of its parts belongs to it too. */
  const stepOf = (c: DeployCandidate, target: string | undefined): DeployStep | undefined => {
    const exact = p?.steps.find(
      (s) =>
        s.start_layer === c.start_layer &&
        s.end_layer === c.end_layer &&
        s.target_node_id === target,
    );
    if (exact) return exact;
    return p?.steps.find(
      (s) => s.merged && s.target_node_id === target && s.parts.some((pt) => pt.start_layer === c.start_layer && pt.end_layer === c.end_layer),
    );
  };

  /** Edit one knob. The draft keeps the field usable while it is half-typed. */
  function setKnob(key: keyof RuntimeParams, text: string) {
    setDrafts((d) => ({ ...d, [key]: text }));
    const value = Number(text);
    if (text !== "" && Number.isFinite(value)) {
      setParams({ ...eff, [key]: value });
    }
  }

  function edit(next: (base: DeploySelection[]) => DeploySelection[]) {
    setSelection(next(rows.map((r) => ({ ...r }))).slice().sort(byLayer));
  }

  function toggle(start: number, end: number) {
    edit((base) => {
      const at = base.findIndex((r) => r.start_layer === start && r.end_layer === end);
      if (at !== -1) return base.filter((_, i) => i !== at);

      // A newly included range needs a machine, and the useful default is one
      // that already has the file and is not busy with another range.
      const taken = new Set(base.map((r) => r.target_node_id).filter(Boolean));
      const candidate = p?.available.find(
        (c) => c.start_layer === start && c.end_layer === end,
      );
      const target =
        candidate?.holders.find((h) => !taken.has(h)) ??
        p?.machines.find((m) => !taken.has(m.node_id))?.node_id ??
        "";
      return [
        ...base,
        { start_layer: start, end_layer: end, target_node_id: target, n_gpu_layers: null },
      ];
    });
  }

  /** Give a range to a machine. Taking an occupied machine swaps the two —
   *  unless merged units are on, where two ranges on the same machine is
   *  exactly what the operator is asking for. */
  function assign(index: number, nodeId: string) {
    edit((base) => {
      const previous = base[index].target_node_id;
      if (!effMerge) {
        const busy = base.findIndex((r, i) => i !== index && r.target_node_id === nodeId);
        if (busy !== -1) base[busy].target_node_id = previous;
      }
      base[index].target_node_id = nodeId;
      return base;
    });
  }

  /**
   * How many layers this machine offloads, or `null` to take the cluster's
   * number. One number per machine, so it lands on every row the machine
   * holds — a merged machine can't have two of them.
   */
  function setPeerGpu(nodeId: string, value: number | null) {
    edit((base) =>
      base.map((r) => (r.target_node_id === nodeId ? { ...r, n_gpu_layers: value } : r)),
    );
  }

  /**
   * Where this machine's KV cache lives. Same shape as the offload above and
   * for the same reason: the cache is sized by the context window, and the
   * machines in one pipeline have wildly different amounts of VRAM and RAM, so
   * one switch for the whole cluster made the smallest machine decide for the
   * biggest.
   */
  function setPeerKqv(nodeId: string, value: boolean) {
    edit((base) =>
      base.map((r) => (r.target_node_id === nodeId ? { ...r, offload_kqv: value } : r)),
    );
  }

  /**
   * Give the whole model to a machine.
   *
   * The same `selection` the slice path sends, with the one range the node
   * offered — a whole model is a candidate like any other, and writing its
   * range here from the plan rather than from `total_layers` keeps the screen
   * from having a second opinion about how big the model is.
   */
  function assignWhole(nodeId: string) {
    const c = p?.available[0];
    if (!c) return;
    const base = rows[0];
    setSelection([
      {
        start_layer: c.start_layer,
        end_layer: c.end_layer,
        target_node_id: nodeId,
        n_gpu_layers: base?.n_gpu_layers ?? null,
        offload_kqv: base?.offload_kqv ?? null,
      },
    ]);
  }

  /** Move a machine one place along the pipeline, which is a swap of ranges. */
  function move(index: number, delta: number) {
    const to = index + delta;
    if (to < 0 || to >= rows.length) return;
    edit((base) => {
      const keep = base[index].target_node_id;
      base[index].target_node_id = base[to].target_node_id;
      base[to].target_node_id = keep;
      return base;
    });
  }

  async function deploy() {
    setRunning(true);
    setAsked(false);
    try {
      const started = await api.post<{ job_id: string }>("/api/ui/deploy", {
        group: chosen,
        kind,
        ...(selection ? { selection: rows } : {}),
        ...(params ? { params } : {}),
        ...(merge != null && !isWhole ? { merge } : {}),
      });
      setJobId(started.job_id);
      // Written from `rows` and `eff`, not from the possibly-null edit state:
      // what comes back next time has to be what was actually deployed,
      // including the parts the operator left on the node's suggestion.
      rememberDeploy(memoryKey, { selection: rows, params: eff, merge: effMerge });
      toast.success(
        t("deploy.started"),
        t("deploy.startedDetail"),
      );
      invalidate();
    } catch (e) {
      toast.fromError(t("deploy.startFailed"), e);
    } finally {
      setRunning(false);
    }
  }

  const job = jobs.data?.jobs.find((j) => j.id === jobId);
  // A range with no machine is a blocked plan, not traffic: counting it made
  // the footer promise "1 cópia · 3,3 GB" for a row that says "sem máquina".
  // A merged step is a copy per missing part, not one per row.
  const copies = rows.filter((r) => {
    if (!r.target_node_id) return false;
    const s = p?.steps.find(
      (x) =>
        (x.merged
          ? x.parts.some((pt) => pt.start_layer === r.start_layer && pt.end_layer === r.end_layer)
          : x.start_layer === r.start_layer && x.end_layer === r.end_layer) &&
        x.target_node_id === r.target_node_id,
    );
    if (!s) return false;
    if (s.merged) {
      const part = s.parts.find(
        (pt) => pt.start_layer === r.start_layer && pt.end_layer === r.end_layer,
      );
      return part ? !part.ready : false;
    }
    return !s.ready;
  });
  const copyBytes = copies.reduce((sum, r) => {
    const c = p?.available.find(
      (x) => x.start_layer === r.start_layer && x.end_layer === r.end_layer,
    );
    return sum + (c?.size_bytes ?? 0);
  }, 0);
  const canDeploy = validated && !p?.blocked && rows.length > 0;

  /**
   * Why the button is grey, in one line, next to the button.
   *
   * A disabled control with no reason beside it is the defect this screen was
   * reported for: the operator sees grey and has nowhere to look. The panels
   * above carry the long version; this says which of them to read, and covers
   * the two states that have no panel at all - nothing selected, and an answer
   * still on its way.
   */
  const whyNot: string | null = canDeploy
    ? null
    : rows.length === 0 && !isWhole
      ? t("deploy.whyNotEmpty")
      : syncing || !p
        ? t("deploy.whyNotChecking")
        : p.blocked
          ? t("deploy.whyNotBlocked")
          : !validated
            ? t("deploy.whyNotMismatch")
            : rows.length === 0
              ? t("deploy.whyNotNoModel")
              : null;

  /** The one step of a whole-model plan, when the node has answered about it. */
  const wholeStep = isWhole ? p?.steps[0] : undefined;
  const nothingToDeploy = groups.length === 0 && wholes.length === 0;

  return (
    <>
      <PageHeader
        title={t("deploy.title")}
        subtitle={t("deploy.subtitle")}
      />

      {cluster.isLoading && <Skeleton className="h-40 w-full" />}
      {cluster.isError && (
        <ErrorState error={cluster.error} onRetry={() => void cluster.refetch()} />
      )}

      {cluster.data && nothingToDeploy && (
        <Panel>
          <Empty icon={<Layers className="h-5 w-5" />} title={t("deploy.empty")}>
            {t("deploy.emptyDetail")}
          </Empty>
        </Panel>
      )}

      {!nothingToDeploy && (
        <div className="space-y-5">
          <Panel>
            <PanelHeader
              title={t("deploy.model")}
              subtitle={isWhole ? t("deploy.wholeDetail") : t("deploy.modelDetail")}
              actions={
                (selection || params) && (
                  <Button
                    size="sm"
                    icon={<Wand2 className="h-3.5 w-3.5" />}
                    onClick={() => {
                      setSelection(null);
                      setParams(null);
                      setDrafts({});
                    }}
                  >
                    {t("deploy.restore")}
                  </Button>
                )
              }
            />
            <div className="space-y-4 px-5 py-4">
              {/* The first decision, and the one that changes everything
                  below it: a set of slices across machines, or one whole
                  file on one machine. */}
              <div role="radiogroup" aria-label={t("deploy.kindAria")}>
                <KindPicker
                  value={kind}
                  onChange={(k) => {
                    setPickedKind(k);
                    setSelection(null);
                    setJobId(null);
                  }}
                  labels={{
                    shards: { label: t("deploy.kindShards"), hint: t("deploy.kindShardsHint") },
                    whole: { label: t("deploy.kindWhole"), hint: t("deploy.kindWholeHint") },
                  }}
                />
              </div>

              {isWhole ? (
                wholes.length === 0 ? (
                  <Empty icon={<Boxes className="h-5 w-5" />} title={t("deploy.wholeEmpty")}>
                    {t("deploy.wholeEmptyDetail")}
                  </Empty>
                ) : (
                  <div className="space-y-3">
                    <Select
                      aria-label={t("deploy.wholeAria")}
                      name="modelo-inteiro"
                      value={chosen}
                      onChange={(e) => {
                        setWholeName(e.target.value);
                        setSelection(null);
                        setJobId(null);
                      }}
                      className="max-w-md"
                    >
                      {wholes.map((w) => (
                        <option key={w.name} value={w.name}>
                          {w.name}
                        </option>
                      ))}
                    </Select>
                    {wholeModel && (
                      <dl className="flex flex-wrap gap-x-8 gap-y-2">
                        <div>
                          <dt className="eyebrow">{t("models.size")}</dt>
                          <dd className="font-mono text-[0.82rem] text-fg">
                            {bytes(wholeModel.size_bytes)}
                          </dd>
                        </div>
                        <div>
                          <dt className="eyebrow">{t("models.layers")}</dt>
                          <dd className="font-mono text-[0.82rem] text-fg">
                            {wholeModel.total_layers != null ? (
                              num(wholeModel.total_layers)
                            ) : (
                              <span className="text-rose">{t("deploy.wholeNoLayers")}</span>
                            )}
                          </dd>
                        </div>
                        <div>
                          <dt className="eyebrow">{t("models.quant")}</dt>
                          <dd className="font-mono text-[0.82rem] text-fg">
                            {wholeModel.quant ?? "—"}
                          </dd>
                        </div>
                        <div>
                          <dt className="eyebrow">{t("models.architecture")}</dt>
                          <dd className="text-[0.82rem] text-fg">
                            {wholeModel.architecture ?? "—"}
                          </dd>
                        </div>
                        <div className="min-w-0">
                          <dt className="eyebrow">{t("deploy.wholePresentOn")}</dt>
                          <dd className="truncate text-[0.82rem] text-fg">
                            {wholeModel.holders.map(labelOf).join(", ")}
                          </dd>
                        </div>
                      </dl>
                    )}
                  </div>
                )
              ) : groups.length === 0 ? (
                <Empty icon={<Layers className="h-5 w-5" />} title={t("deploy.shardsEmpty")}>
                  {t("deploy.shardsEmptyDetail")}
                </Empty>
              ) : (
                <Select
                  aria-label={t("deploy.groupAria")}
                  name="conjunto-de-fatias"
                  value={chosen}
                  onChange={(e) => {
                    setGroup(e.target.value);
                    setSelection(null);
                    setJobId(null);
                  }}
                  className="max-w-md"
                >
                  {groups.map((g) => (
                    <option key={g} value={g}>
                      {g}
                    </option>
                  ))}
                </Select>
              )}
            </div>
          </Panel>

          {plan.isLoading && <Skeleton className="h-52 w-full" />}
          {plan.isError && (
            <ErrorState error={plan.error} onRetry={() => void plan.refetch()} />
          )}

          {/* `chosen` empty means the menu the operator is looking at has
              nothing in it. The plan below would then be the *previous*
              answer — react-query keeps it deliberately, so switching models
              does not blank the screen — and showing it next to "there is
              nothing here" describes a deploy of something the operator can no
              longer see. */}
          <AnimatePresence mode="wait">
            {p && chosen.length > 0 && (
              <motion.div
                key={`${kindOf(p)}:${p.group}`}
                initial={{ opacity: 0, y: 8 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -6 }}
                transition={{ duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
                className="space-y-5"
              >
                {answered && p.blocked && (
                  <div className="flex items-start gap-3 rounded-xl border border-rose/30 bg-rose/[0.07] px-4 py-3.5">
                    <CircleAlert className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
                    <div className="min-w-0">
                      <p className="text-sm font-medium text-fg">
                        {t("deploy.blocked")}
                      </p>
                      <p className="mt-1 text-sm text-muted">{p.blocked}</p>
                    </div>
                  </div>
                )}

                {answered && !p.blocked && !p.complete && (
                  <div className="flex items-start gap-3 rounded-xl border border-amber/30 bg-amber/[0.07] px-4 py-3.5">
                    <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
                    <div className="min-w-0">
                      <p className="text-sm font-medium text-fg">
                        {t("deploy.incomplete")}
                      </p>
                      <p className="mt-1 text-sm text-muted">
                        {p.missing
                          .map(([a, b]) => (a === b ? `L${a}` : `L${a}–${b}`))
                          .join(", ")}{" "}
                        {t("deploy.excludedLayers", { count: p.missing.length })}. {t("deploy.incompleteDetail")}
                      </p>
                    </div>
                  </div>
                )}

                {/* A deduced total is not a defect, but it is the one number
                    here nobody can check: the slices reach layer N and there is
                    no metadata saying whether the model ends there. Said before
                    the button, next to the coverage warning it belongs with. */}
                {answered && p.total_layers_inferred && (
                  <div className="flex items-start gap-3 rounded-xl border border-amber/30 bg-amber/[0.07] px-4 py-3.5">
                    <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
                    <div className="min-w-0">
                      <p className="text-sm font-medium text-fg">
                        {t("deploy.inferredTitle")}
                      </p>
                      <p className="mt-1 text-sm text-muted">
                        {t("deploy.inferredDetail", { count: p.total_layers ?? 0 })}
                      </p>
                    </div>
                  </div>
                )}

                {answered &&
                  p.warnings
                    .filter((w) => w.includes("memória"))
                    .map((w) => (
                      <div
                        key={w}
                        className="flex items-start gap-3 rounded-xl border border-amber/30 bg-amber/[0.07] px-4 py-3"
                      >
                        <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
                        <p className="min-w-0 text-sm text-muted">{localizeServerText(locale, w)}</p>
                      </div>
                    ))}

                {/* The plan on the left, the settings it will launch with on
                    the right and always in view: the operator decides both at
                    once, and one of them used to be behind a button. Stacks on
                    anything narrower than a desktop, settings last. */}
                <div className="grid min-w-0 gap-5 xl:grid-cols-[minmax(0,1fr)_21rem] xl:items-start">
                  <div className="min-w-0 space-y-5">
                {/* A whole model has one machine and every layer, so neither
                    question the two panels below ask has a second answer: the
                    order rail would be one card, the coverage ribbon a solid
                    bar, and the "included" column a box that can never be
                    unticked. What replaces them is the one decision that is
                    real — which machine. */}
                {isWhole ? (
                  <Panel>
                    <PanelHeader
                      eyebrow={p.auto && !selection ? t("deploy.orderSuggestion") : t("deploy.yourChoice")}
                      title={t("deploy.wholeTargetTitle")}
                      subtitle={t("deploy.wholeTargetDetail")}
                      actions={
                        <div className="flex flex-wrap items-center justify-end gap-x-3 gap-y-1">
                          {whyNot && (
                            <span className="text-[0.78rem] text-muted">{whyNot}</span>
                          )}
                          <Button
                            variant="primary"
                            icon={<Rocket className="h-4 w-4" />}
                            loading={running}
                            disabled={!canDeploy}
                            onClick={() => void deploy()}
                          >
                            {t("deploy.deploy")}
                          </Button>
                        </div>
                      }
                    />
                    <div className="space-y-4 px-5 py-4">
                      <Field label={t("deploy.wholeMachine")} hint={t("deploy.wholeMachineHint")}>
                        <Select
                          aria-label={t("deploy.wholeMachineAria")}
                          name="maquina-do-modelo"
                          className="max-w-md"
                          value={rows[0]?.target_node_id ?? ""}
                          onChange={(e) => assignWhole(e.target.value)}
                        >
                          <option value="">{t("deploy.chooseMachine")}</option>
                          {p.machines.map((m) => (
                            <option key={m.node_id} value={m.node_id}>
                              {m.label}
                              {p.available[0]?.holders.includes(m.node_id)
                                ? t("deploy.alreadyHas")
                                : t("deploy.copy")}
                            </option>
                          ))}
                        </Select>
                      </Field>

                      <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-t border-line pt-4">
                        <Badge tone="iris" mono>
                          {p.total_layers != null
                            ? layerRange(0, p.total_layers - 1)
                            : t("deploy.unknownModelSize")}
                        </Badge>
                        <span className="text-[0.82rem] text-muted">
                          {t("deploy.wholeOneMachine")}
                        </span>
                      </div>

                      <div className="text-[0.82rem]">
                        {!wholeStep ? (
                          <span className="text-faint">{t("deploy.checking")}</span>
                        ) : wholeStep.problem ? (
                          <span className="flex items-start gap-1.5 text-rose">
                            <CircleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                            {wholeStep.problem}
                          </span>
                        ) : wholeStep.ready ? (
                          <span className="text-muted">{t("deploy.alreadyThere")}</span>
                        ) : (
                          <span className="flex items-start gap-1.5 text-amber">
                            <Download className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                            {t("deploy.copyFrom", { host: wholeStep.source_host ?? "—" })}
                            {wholeStep.note ? ` · ${wholeStep.note}` : ""}
                          </span>
                        )}
                      </div>
                    </div>
                  </Panel>
                ) : (
                  <>
                <Panel>
                  <PanelHeader
                    eyebrow={p.auto && !selection ? t("deploy.orderSuggestion") : t("deploy.yourChoice")}
                    title={t("deploy.orderTitle")}
                    subtitle={t("deploy.orderDetail")}
                  />
                  <div className="px-5 py-4">
                    {rows.length === 0 ? (
                      <p className="py-6 text-center text-sm text-muted">
                        {t("deploy.noSelection")}
                      </p>
                    ) : (
                      <OrderRail rows={rows} labelOf={labelOf} />
                    )}
                  </div>
                  <div className="border-t border-line px-5 py-4">
                    <Coverage total={p.total_layers} rows={rows} />
                  </div>
                </Panel>

                <Panel>
                  <PanelHeader
                    title={t("deploy.shardSelection")}
                    subtitle={t("deploy.allRanges")}
                    actions={
                      <div className="flex flex-wrap items-center justify-end gap-x-3 gap-y-1">
                        {whyNot && (
                          <span className="text-[0.78rem] text-muted">{whyNot}</span>
                        )}
                        <Button
                          variant="primary"
                          icon={<Rocket className="h-4 w-4" />}
                          loading={running}
                          disabled={!canDeploy}
                          onClick={() => (p.complete ? void deploy() : setAsked(true))}
                        >
                          {t("deploy.deploy")}
                        </Button>
                      </div>
                    }
                  />

                  <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3 border-b border-line px-5 py-4">
                    <Switch
                      checked={effMerge}
                      onChange={(v) => setMerge(v)}
                      label={t("deploy.merge")}
                      hint={t("deploy.mergeHint")}
                    />
                  </div>

                  <ScrollX>
                    <table className="w-full min-w-[64rem] text-sm">
                      <thead>
                        <tr className="border-b border-line text-left">
                          {[
                            t("deploy.included"),
                            t("deploy.layers"),
                            t("deploy.shard"),
                            t("deploy.size"),
                            t("deploy.destination"),
                            t("deploy.order"),
                            t("deploy.happens"),
                          ].map((h) => (
                            <th key={h} className="px-5 py-2.5 text-xs font-medium text-faint">
                              {h}
                            </th>
                          ))}
                        </tr>
                      </thead>
                      <tbody className="divide-y divide-line">
                        {p.available.map((c) => {
                          const at = rows.findIndex(
                            (r) =>
                              r.start_layer === c.start_layer && r.end_layer === c.end_layer,
                          );
                          const on = at !== -1;
                          const row = on ? rows[at] : null;
                          const step = stepOf(c, row?.target_node_id);
                          const part = step?.merged
                            ? step.parts.find(
                                (pt) =>
                                  pt.start_layer === c.start_layer &&
                                  pt.end_layer === c.end_layer,
                              )
                            : null;
                          const track = on ? trackOf(at) : null;

                          return (
                            <tr
                              key={rangeKey(c.start_layer, c.end_layer)}
                              className={on ? "" : "opacity-55"}
                            >
                              <td className="py-3 pl-5 pr-2">
                                <Tick
                                  on={on}
                                  label={t("deploy.include", { range: layerRange(c.start_layer, c.end_layer) })}
                                  onToggle={() => toggle(c.start_layer, c.end_layer)}
                                />
                              </td>
                              <td className="whitespace-nowrap py-3 pl-3 pr-5">
                                <span className="flex items-center gap-2">
                                  {/* Kept in the flow even when empty so every
                                      range starts at the same x. */}
                                  <span
                                    className={`h-3.5 w-[3px] rounded-full ${
                                      track ? track.fill : "bg-transparent"
                                    }`}
                                  />
                                  <span
                                    className={`font-mono text-[0.82rem] ${
                                      track ? track.text : "text-muted"
                                    }`}
                                  >
                                    {layerRange(c.start_layer, c.end_layer)}
                                  </span>
                                </span>
                              </td>
                              <td className="max-w-[15rem] px-5 py-3">
                                <span className="block truncate text-[0.82rem] text-fg">
                                  {c.name}
                                </span>
                              </td>
                              <td className="whitespace-nowrap px-5 py-3 font-mono text-[0.8rem] text-muted">
                                {bytes(c.size_bytes)}
                              </td>
                              {/* The width lives on the cell: `Select` is
                                  `w-full`, and a width class on it loses to
                                  that in the generated CSS whatever the order
                                  in the string. */}
                              <td className="w-[18rem] px-5 py-3">
                                {on ? (
                                  <Select
                                    aria-label={t("deploy.machineFor", { range: layerRange(c.start_layer, c.end_layer) })}
                                    name={`destino-${rangeKey(c.start_layer, c.end_layer)}`}
                                    className="h-8 text-[0.82rem]"
                                    value={row?.target_node_id ?? ""}
                                    onChange={(e) => assign(at, e.target.value)}
                                  >
                                    <option value="">{t("deploy.chooseMachine")}</option>
                                    {p.machines.map((m) => (
                                      <option key={m.node_id} value={m.node_id}>
                                        {/* The cost of the choice is spelled out
                                            in the last column; here it only has
                                            to say whether it means a copy. */}
                                        {m.label}
                                        {c.holders.includes(m.node_id) ? t("deploy.alreadyHas") : t("deploy.copy")}
                                      </option>
                                    ))}
                                  </Select>
                                ) : (
                                  <span className="text-[0.8rem] text-faint">—</span>
                                )}
                              </td>
                              <td className="whitespace-nowrap px-5 py-3">
                                {on ? (
                                  <span className="flex items-center gap-1">
                                    <button
                                      type="button"
                                      aria-label={t("deploy.moveUp", { range: layerRange(c.start_layer, c.end_layer) })}
                                      disabled={at === 0}
                                      onClick={() => move(at, -1)}
                                      className="flex h-7 w-7 items-center justify-center rounded-md border border-line text-muted transition-colors hover:border-line-strong hover:text-fg disabled:pointer-events-none disabled:opacity-35"
                                    >
                                      <ArrowUp className="h-3.5 w-3.5" />
                                    </button>
                                    <button
                                      type="button"
                                      aria-label={t("deploy.moveDown", { range: layerRange(c.start_layer, c.end_layer) })}
                                      disabled={at === rows.length - 1}
                                      onClick={() => move(at, 1)}
                                      className="flex h-7 w-7 items-center justify-center rounded-md border border-line text-muted transition-colors hover:border-line-strong hover:text-fg disabled:pointer-events-none disabled:opacity-35"
                                    >
                                      <ArrowDown className="h-3.5 w-3.5" />
                                    </button>
                                  </span>
                                ) : (
                                  <span className="text-[0.8rem] text-faint">—</span>
                                )}
                              </td>
                              <td className="px-5 py-3">
                                {!on ? (
                                  <span className="text-[0.8rem] text-faint">
                                    {c.holders.length === 0
                                      ? t("deploy.noneMachine")
                                      : c.holders.length > 2
                                        ? t("deploy.heldOnMachines", { count: c.holders.length })
                                        : t("deploy.heldOn", { machines: c.holders.map(labelOf).join(", ") })}
                                  </span>
                                ) : !step ? (
                                  <span className="text-[0.8rem] text-faint">{t("deploy.checking")}</span>
                                ) : step.problem ? (
                                  <span className="flex items-start gap-1.5 text-[0.8rem] text-rose">
                                    <CircleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                                    {step.problem}
                                  </span>
                                ) : step.merged ? (
                                  part?.ready ? (
                                    <span className="text-[0.8rem] text-muted">
                                      {t("deploy.mergedThere", { range: layerRange(step.start_layer, step.end_layer) })}
                                    </span>
                                  ) : (
                                    <span className="flex items-start gap-1.5 text-[0.8rem] text-amber">
                                      <Download className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                                      {t("deploy.copyAndMerge", {
                                        host: part?.source_host ?? "—",
                                        range: layerRange(step.start_layer, step.end_layer),
                                      })}
                                      {step.note ? ` · ${step.note}` : ""}
                                    </span>
                                  )
                                ) : step.ready ? (
                                  <span className="text-[0.8rem] text-muted">{t("deploy.alreadyThere")}</span>
                                ) : (
                                  <span className="flex items-start gap-1.5 text-[0.8rem] text-amber">
                                    <Download className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                                    {t("deploy.copyFrom", { host: step.source_host ?? "—" })}
                                    {step.note ? ` · ${step.note}` : ""}
                                  </span>
                                )}
                              </td>
                            </tr>
                          );
                        })}
                      </tbody>
                    </table>
                  </ScrollX>

                  <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
                    <span className="text-[0.82rem] text-muted">
                      {t("deploy.selectedShards", { count: rows.length, total: p.available.length })} ·{" "}
                      {p.total_layers === null ? (
                        <span className="text-rose">
                          {t("deploy.unknownModelSize")}
                        </span>
                      ) : (
                        <>
                          {t("deploy.modelLayers", { count: p.total_layers })}
                          {p.total_layers_inferred && (
                            <span className="text-amber"> {t("deploy.inferred")}</span>
                          )}
                        </>
                      )}
                    </span>
                    {copies.length === 0 ? (
                      <Badge tone="aqua">{t("deploy.nothingToCopy")}</Badge>
                    ) : (
                      <Badge tone="amber">
                        {t("deploy.copySummary", { count: copies.length, size: bytes(copyBytes) })}
                      </Badge>
                    )}
                  </div>
                </Panel>
                  </>
                )}

                {job && (
                  <Panel>
                    <PanelHeader
                      eyebrow={t("deploy.progress")}
                      title={job.title ?? t("deploy.deployment")}
                      subtitle={job.message}
                    />
                    <div className="space-y-2.5 px-5 py-4">
                      <div className="flex items-baseline justify-between gap-3">
                        <span className="text-[0.82rem] text-fg">{job.step}</span>
                        <span className="font-mono text-[0.78rem] tabular-nums text-muted">
                          {job.progress == null ? "…" : `${Math.round(job.progress)}%`}
                        </span>
                      </div>
                      <Progress
                        value={job.progress}
                        tone={
                          job.state === "failed"
                            ? "rose"
                            : job.state === "done"
                              ? "aqua"
                              : "iris"
                        }
                      />
                    </div>
                  </Panel>
                )}

                <Panel>
                  <PanelHeader
                    eyebrow={t("deploy.machines")}
                    title={t("deploy.machineTitle")}
                    subtitle={t("deploy.machineDetail")}
                  />
                  <ScrollX>
                    <table className="w-full min-w-[52rem] text-sm">
                      <thead>
                        <tr className="border-b border-line text-left">
                          {[
                            t("deploy.machine"),
                            t("deploy.freeMemory"),
                            t("deploy.freeDisk"),
                            t("deploy.heldShards"),
                            t("deploy.thisDeployment"),
                            t("deploy.gpuHere"),
                            t("deploy.kvHere"),
                          ].map(
                            (h) => (
                              <th key={h} className="px-5 py-2.5 text-xs font-medium text-faint">
                                {h}
                              </th>
                            ),
                          )}
                        </tr>
                      </thead>
                      <tbody className="divide-y divide-line">
                        {p.machines.map((m) => {
                          const taking = rows.filter((r) => r.target_node_id === m.node_id);
                          const union =
                            taking.length > 0
                              ? layerRange(
                                  taking[0].start_layer,
                                  taking[taking.length - 1].end_layer,
                                )
                              : null;
                          return (
                            <tr key={m.node_id}>
                              <td className="whitespace-nowrap px-5 py-3">
                                <span className="flex items-center gap-1.5 text-[0.82rem] text-fg">
                                  <ServerCog className="h-3.5 w-3.5 text-faint" />
                                  {m.label}
                                </span>
                              </td>
                              <td className="whitespace-nowrap px-5 py-3 font-mono text-[0.8rem] text-muted">
                                {m.mem_free_mb > 0 ? megabytes(m.mem_free_mb) : "—"}
                              </td>
                              <td className="whitespace-nowrap px-5 py-3 font-mono text-[0.8rem] text-muted">
                                {m.disk_free_mb > 0 ? megabytes(m.disk_free_mb) : "—"}
                              </td>
                              {/* `whitespace-nowrap` because a range is one
                                  token to read: wrapped, "L20-39" breaks after
                                  the dash and the second line reads as a layer
                                  number of its own. The table scrolls instead. */}
                              <td className="whitespace-nowrap px-5 py-3 font-mono text-[0.8rem] text-muted">
                                {m.holds.length === 0
                                  ? t("deploy.none")
                                  : m.holds.map(([a, b]) => layerRange(a, b)).join(" · ")}
                              </td>
                              <td className="whitespace-nowrap px-5 py-3">
                                {taking.length > 0 ? (
                                  <Badge tone="iris" mono>
                                    {union}
                                    {taking.length > 1 && (
                                      <span className="ml-1.5 normal-case text-iris/80">
                                        +{taking.length - 1}
                                      </span>
                                    )}
                                  </Badge>
                                ) : (
                                  <span className="text-[0.8rem] text-faint">{t("deploy.excluded")}</span>
                                )}
                              </td>
                              {/* Offload and KV placement are what a machine
                                  gets to disagree about, so they are edited
                                  next to the machine and not in the
                                  cluster-wide block. Everything else in
                                  `RuntimeParams` has to agree across the
                                  pipeline. */}
                              <td className="whitespace-nowrap px-5 py-3">
                                {taking.length > 0 ? (
                                  <Input
                                    type="number"
                                    min={0}
                                    aria-label={t("deploy.gpuAria", { machine: m.label })}
                                    name={`gpu-${m.node_id}`}
                                    className="h-8 w-24 text-[0.82rem]"
                                    placeholder={String(eff.n_gpu_layers)}
                                    value={taking[0].n_gpu_layers ?? ""}
                                    onChange={(e) =>
                                      setPeerGpu(
                                        m.node_id,
                                        e.target.value === ""
                                          ? null
                                          : Math.max(0, Number(e.target.value) || 0),
                                      )
                                    }
                                  />
                                ) : (
                                  <span className="text-[0.8rem] text-faint">—</span>
                                )}
                              </td>
                              <td className="whitespace-nowrap px-5 py-3">
                                {taking.length > 0 ? (
                                  <span className="flex items-center gap-2">
                                    <Tick
                                      on={taking[0].offload_kqv ?? eff.offload_kqv ?? true}
                                      label={t("deploy.kvAria", { machine: m.label })}
                                      onToggle={() =>
                                        setPeerKqv(
                                          m.node_id,
                                          !(taking[0].offload_kqv ?? eff.offload_kqv ?? true),
                                        )
                                      }
                                    />
                                    <span className="text-[0.8rem] text-muted">
                                      {(taking[0].offload_kqv ?? eff.offload_kqv ?? true)
                                        ? t("deploy.kvOnGpu")
                                        : t("deploy.kvInRam")}
                                    </span>
                                  </span>
                                ) : (
                                  <span className="text-[0.8rem] text-faint">—</span>
                                )}
                              </td>
                            </tr>
                          );
                        })}
                      </tbody>
                    </table>
                  </ScrollX>
                </Panel>

                  </div>

                  {/* Sticky so it stays readable while the shard table
                      scrolls; `top` clears the toolbar. */}
                  <aside className="min-w-0 space-y-5 xl:sticky xl:top-4">
                  <Panel>
                    <PanelHeader
                      eyebrow={t("deploy.execution")}
                      title={t("deploy.properties")}
                      subtitle={t("deploy.advancedDetail", {
                        prompt: num(p.prompt_tokens),
                        chunk: num(p.chunk_tokens),
                      })}
                    />
                    {/* Never folded away. These eleven numbers decide the prompt
                        budget every request is measured against, and a deploy
                        made without reading them is a deploy made blind — the
                        operator was choosing machines on the left with the
                        settings that governed them hidden behind a button. */}
                    <div className="space-y-5 px-5 py-4">
                        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-1">
                          {KNOBS.map((k) => (
                            <Field key={k.key} label={t("deploy.knob." + k.key)}>
                              <Input
                                type="number"
                                name={k.key}
                                min={k.min}
                                max={k.max}
                                step={k.step}
                                value={drafts[k.key] ?? String(eff[k.key] ?? "")}
                                onChange={(e) => setKnob(k.key, e.target.value)}
                                onBlur={() =>
                                  setDrafts((d) => {
                                    const { [k.key]: _drop, ...rest } = d;
                                    return rest;
                                  })
                                }
                              />
                            </Field>
                          ))}
                        </div>

                        {/* `offload_kqv` used to sit here. It is a property of a
                            machine's memory, not of the pipeline, so it moved to
                            the machines table where the machine is. */}
                        <div className="grid gap-4 border-t border-line pt-4 sm:grid-cols-2 xl:grid-cols-1">
                          <Switch
                            checked={eff.no_thinking ?? true}
                            onChange={(v) => setParams({ ...eff, no_thinking: v })}
                            label={t("deploy.noThinking")}
                            hint={t("deploy.noThinkingHint")}
                          />
                          <Switch
                            checked={eff.no_embeddings ?? false}
                            onChange={(v) => setParams({ ...eff, no_embeddings: v })}
                            label={t("deploy.noEmbeddings")}
                            hint={t("deploy.noEmbeddingsHint")}
                          />
                          <Switch
                            checked={eff.speculative_decoding_enabled ?? false}
                            onChange={(v) =>
                              setParams({ ...eff, speculative_decoding_enabled: v })
                            }
                            label={t("deploy.speculative")}
                            hint={t("deploy.speculativeHint")}
                          />
                          <Switch
                            checked={eff.mtp_enabled ?? false}
                            onChange={(v) =>
                              setParams({ ...eff, mtp_enabled: v })
                            }
                            label={t("deploy.mtp")}
                            hint={
                              isWhole
                                ? t("deploy.mtpHint")
                                : t("deploy.mtpWholeOnly")
                            }
                            disabled={!isWhole}
                          />
                        </div>

                        {(eff.speculative_decoding_enabled ?? false) && (
                          <div className="grid gap-4 border-t border-line pt-4 sm:grid-cols-2 xl:grid-cols-1">
                            {SPECULATIVE_KNOBS.map((k) => (
                              <Field
                                key={k.key}
                                label={t("deploy.knob." + k.key)}
                                hint={t("deploy.knob." + k.key + "Hint")}
                              >
                                <Input
                                  type="number"
                                  name={k.key}
                                  min={k.min}
                                  max={k.max}
                                  step={k.step}
                                  value={drafts[k.key] ?? String(eff[k.key] ?? "")}
                                  onChange={(e) => setKnob(k.key, e.target.value)}
                                  onBlur={() =>
                                    setDrafts((d) => {
                                      const { [k.key]: _drop, ...rest } = d;
                                      return rest;
                                    })
                                  }
                                />
                              </Field>
                            ))}
                          </div>
                        )}

                        {(eff.mtp_enabled ?? false) && isWhole && (
                          <div className="grid gap-4 border-t border-line pt-4 sm:grid-cols-2 xl:grid-cols-1">
                            {MTP_KNOBS.map((k) => (
                              <Field
                                key={k.key}
                                label={t("deploy.knob." + k.key)}
                                hint={t("deploy.knob." + k.key + "Hint")}
                              >
                                <Input
                                  type="number"
                                  name={k.key}
                                  min={k.min}
                                  max={k.max}
                                  step={k.step}
                                  value={drafts[k.key] ?? String(eff[k.key] ?? "")}
                                  onChange={(e) => setKnob(k.key, e.target.value)}
                                  onBlur={() =>
                                    setDrafts((d) => {
                                      const { [k.key]: _drop, ...rest } = d;
                                      return rest;
                                    })
                                  }
                                />
                              </Field>
                            ))}
                          </div>
                        )}
                    </div>
                  </Panel>
                  </aside>
                </div>

                <Footnote icon={<ArrowRight className="h-3.5 w-3.5" />}>
                  {t("deploy.transferNote")}
                </Footnote>
              </motion.div>
            )}
          </AnimatePresence>
        </div>
      )}

      <Confirm
        open={asked}
        title={t("deploy.partialTitle")}
        description={
          <>
            {p?.missing
              .map(([a, b]) => (a === b ? `L${a}` : `L${a}–${b}`))
              .join(", ")}{" "}
            {t("deploy.partialConfirm")}
          </>
        }
        confirmLabel={t("deploy.deploy")}
        busy={running}
        onConfirm={() => void deploy()}
        onCancel={() => setAsked(false)}
      />
    </>
  );
}
