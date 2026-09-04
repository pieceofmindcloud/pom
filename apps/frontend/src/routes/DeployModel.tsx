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
 *   no order to choose in a pipeline of one, no per-layer checklist, and the
 *   deployment map is the sole machine choice.
 *
 * The rest of this comment is about the slice case, which is the one with
 * decisions in it.
 *
 * The screen opens on a decision the node made - who would serve which range,
 * and what has to be copied first - and every part of that decision is then
 * editable. Nothing here computes the plan in the browser: the suggestion comes
 * from `/api/ui/deploy/plan`, topology edits are sent back to that route for
 * validation, and pressing the button re-surveys the cluster server-side before
 * acting, because a machine can leave between looking and deciding. Runtime
 * settings stay local until deploy, when the fresh preflight validates them.
 *
 * Three things the operator is choosing, and why they are the shape they are:
 *
 * * **Which layers.** A model is often sliced more than once, with different
 *   cuts — 0-4 / 5-31 next to 0-10 / 11-31. Every range anyone holds is a row
 *   with a checkbox, so mixing two slicings is possible and the ribbon above the
 *   table shows immediately what that leaves covered, uncovered or doubled.
 * * **Which machine.** Each selected row carries a target picker. Assigning an
 *   exact range to another machine preserves the existing target as a parallel
 *   route. Repeating the same range on the same machine is ignored.
 * * **The order.** The pipeline runs in layer order — `pipeline.rs` sweeps from
 *   layer 0 up, so whoever holds layer 0 receives every prompt. There is no
 *   second knob to turn: reordering the pipeline *is* reassigning the ranges,
 *   and the ↑ ↓ buttons do exactly that by changing targets between neighbouring
 *   rows. The rail at the top shows the resulting order.
 *
 * The copies matter enough to stay loud. A 10 GB slice takes about seventeen
 * minutes on the real link, so "3 fatias, 12 GB" is the difference between a
 * deploy that finishes now and one that finishes after lunch.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import {
  Check,
  ChevronLeft,
  Rocket,
  ServerCog,
} from "lucide-react";
import { api } from "../lib/api";
import { useCacheTypes, useCluster, useClusterCatalog, useInvalidate, useJobs } from "../lib/queries";
import { recallDeploy, rememberDeploy, usableSelection } from "../lib/deployMemory";
import { useToast } from "../lib/toast";
import { bytes, deployModelDisplayName, layerRange, megabytes } from "../lib/format";
import { Confirm } from "../components/Confirm";
import { DeploymentInspector, InspectorRow } from "../components/DeploymentInspector";
import { DeployNetworkCanvas } from "../components/DeployNetworkCanvas";
import { trackOf } from "../components/PipelineRail";
import { tensorParallelModelsOf, wholeModelsOf } from "../lib/wholeModels";
import {
  Badge,
  Button,
  ErrorState,
  Input,
  Select,
  Skeleton,
} from "../components/ui";
import type {
  DeployKind,
  DeployPlan,
  DeploySelection,
  DeployStrategy,
  CacheTypesResponse,
  KvCacheType,
  RuntimeParams,
  TpLaunch,
} from "../lib/types";
import { useI18n } from "../i18n/provider";

const rangeKey = (start: number, end: number) => `${start}-${end}`;

const DEFAULT_CACHE_TYPES: KvCacheType[] = [
  "f32",
  "f16",
  "bf16",
  "q8_0",
  "q4_0",
  "q4_1",
  "iq4_nl",
  "q5_0",
  "q5_1",
];

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
  a.start_layer - b.start_layer ||
  a.end_layer - b.end_layer;

/** The planner's expensive survey only depends on topology. Per-machine
 * launch overrides are carried by the selection for deploy, but changing one
 * must not make the node rediscover the same machines and ranges. */
const topologyKey = (rows: DeploySelection[] | null) =>
  rows === null
    ? "auto"
    : JSON.stringify(
        rows
          .map((row) => [row.start_layer, row.end_layer, row.target_node_id])
          .sort((a, b) =>
            Number(a[0]) - Number(b[0]) ||
            Number(a[1]) - Number(b[1]) ||
            String(a[2]).localeCompare(String(b[2])),
          ),
      );

const launchOverridesKey = (rows: DeploySelection[] | null) =>
  rows === null
    ? "auto"
    : JSON.stringify(
        rows
          .map((row) => [
            row.start_layer,
            row.end_layer,
            row.target_node_id,
            row.n_gpu_layers ?? null,
            row.offload_kqv ?? null,
          ])
          .sort((a, b) =>
            Number(a[0]) - Number(b[0]) ||
            Number(a[1]) - Number(b[1]) ||
            String(a[2]).localeCompare(String(b[2])),
          ),
      );

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
  label,
}: {
  /** `null` when no slice declares the model size and none starts at layer 0.
   *  There is no ribbon to draw then, and the plan is blocked anyway. */
  total: number | null;
  rows: DeploySelection[];
  label?: string;
}) {
  const { t } = useI18n();
  if (!total || total <= 0) return null;

  const owner = new Array<number>(total).fill(-1);
  const doubled = new Array<boolean>(total).fill(false);
  rows.forEach((r, i) => {
    for (let l = r.start_layer; l <= Math.min(r.end_layer, total - 1); l++) {
      if (owner[l] === -1) owner[l] = i;
      else {
        const first = rows[owner[l]];
        doubled[l] =
          first.start_layer !== r.start_layer || first.end_layer !== r.end_layer;
      }
    }
  });

  const covered = owner.filter((o) => o !== -1).length;
  const overlapped = doubled.filter(Boolean).length;

  return (
    <div className="min-w-0">
      <div className="mb-2 flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <span className="eyebrow">{label ?? t("deploy.coverage")}</span>
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
      <div className="flex h-1.5 w-full gap-px overflow-hidden rounded-md">
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

export function DeployModel() {
  const { t } = useI18n();
  const cluster = useClusterCatalog();
  const liveCluster = useCluster();
  const [group, setGroup] = useState("");
  const [deploymentName, setDeploymentName] = useState("");
  const [strategy, setStrategy] = useState<DeployStrategy>("simple");
  const [preferredNodeIds, setPreferredNodeIds] = useState<string[]>([]);
  const [inspectedNodeId, setInspectedNodeId] = useState<string | null>(null);
  const [inspectorTab, setInspectorTab] = useState<"global" | "machine">("machine");
  const [inspectorOpen, setInspectorOpen] = useState(true);
  /** The whole model to deploy, when that is the question being asked. */
  const [wholeName, setWholeName] = useState("");
  const [tensorGroupKey, setTensorGroupKey] = useState("");
  /** Tensor parallelism group, when that is the question being asked.
   *  `size` is derived (1 primary + workers), not a field the operator types. */
  const [tp, setTp] = useState<{ primary_node_id: string; worker_node_ids: string[] }>({
    primary_node_id: "",
    worker_node_ids: [],
  });
  const tpSyncKey = useRef<string | null>(null);
  const tpCandidateIds = useRef<string[] | null>(null);
  const seedTpWorkers = useRef(false);
  const preserveEmptyPrimary = useRef(false);
  /** `null` until the operator says — see `kind` below for what that means. */
  const [pickedKind, setPickedKind] = useState<DeployKind | null>(null);
  /** `null` means "whatever the node suggests" — the default, few-clicks path. */
  const [selection, setSelection] = useState<DeploySelection[] | null>(null);
  /** `null` means "the node's own defaults", which is what most deploys want. */
  const [params, setParams] = useState<RuntimeParams | null>(null);
  const nodeParams = useRef<RuntimeParams | null>(null);
  /** `null` means "the node's default" — merged units off. */
  const [merge, setMerge] = useState<boolean | null>(null);
  const [asked, setAsked] = useState(false);
  /** Segundo confirm, separado do de cobertura parcial: a projeção de VRAM
   *  passa da capacidade de alguma máquina. Avisa, não bloqueia - o operador
   *  pode reduzir contexto/sessões ou implantar mesmo assim. */
  const [capacityAsked, setCapacityAsked] = useState(false);
  const [running, setRunning] = useState(false);
  const [cacheTypeK, setCacheTypeK] = useState<KvCacheType>("f16");
  const [cacheTypeV, setCacheTypeV] = useState<KvCacheType>("f16");
  const [cacheTypesLoaded, setCacheTypesLoaded] = useState(false);
  const [savingCacheTypes, setSavingCacheTypes] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);
  const toast = useToast();
  const invalidate = useInvalidate();
  const jobs = useJobs();
  const cacheTypes = useCacheTypes();

  useEffect(() => {
    if (cacheTypesLoaded || !cacheTypes.data) return;
    setCacheTypeK(cacheTypes.data.cache_type_k);
    setCacheTypeV(cacheTypes.data.cache_type_v);
    setCacheTypesLoaded(true);
  }, [cacheTypes.data, cacheTypesLoaded]);
  /** Every slicing round anyone in the cluster holds, named once. */
  const groups = useMemo(() => {
    const seen = new Map<string, { key: string; label: string; pomId: string }>();
    for (const peer of cluster.data?.peers ?? []) {
      for (const set of peer.shard_sets ?? []) {
        if (!seen.has(set.group)) {
          seen.set(set.group, {
            key: set.group,
            label: set.model_label || set.group,
            pomId: set.pom_id ?? "",
          });
        }
      }
    }
    return [...seen.values()].sort((a, b) => a.label.localeCompare(b.label));
  }, [cluster.data]);

  /** Only disambiguate repeated model labels in the picker. */
  const duplicateGroupLabels = useMemo(() => {
    const counts = new Map<string, number>();
    for (const item of groups) counts.set(item.label, (counts.get(item.label) ?? 0) + 1);
    return counts;
  }, [groups]);

  /** Every un-sliced model anyone in the cluster holds, named once. */
  const wholes = useMemo(() => wholeModelsOf(cluster.data?.peers), [cluster.data]);
  const tensorGroups = useMemo(
    () => tensorParallelModelsOf(cluster.data?.peers),
    [cluster.data],
  );

  /**
   * Slices unless the operator says otherwise — except on a cluster that has
   * whole models and no slice sets at all, where "slices" opens on an empty
   * screen that looks like a broken deploy page rather than an unanswered
   * question. The operator's own choice always wins once made.
   */
  const kind: DeployKind =
    pickedKind ?? (groups.length === 0 && wholes.length > 0 ? "whole" : "shards");
  const isWhole = kind === "whole";
  const isTp = kind === "tensor_parallel";
  const tpGroup = useMemo(
    () => tensorGroups.find((entry) => entry.key === tensorGroupKey) ?? tensorGroups[0],
    [tensorGroupKey, tensorGroups],
  );
  const hasSlicedTp = isTp && !!tpGroup;

  useEffect(() => {
    setInspectorTab(kind === "shards" ? "machine" : "global");
  }, [kind]);

  const chosen = isWhole
    ? wholeName || wholes[0]?.name || ""
    : isTp
      ? hasSlicedTp
        ? tpGroup?.group ?? ""
        : wholeName || wholes[0]?.name || ""
      : group || groups[0]?.key || "";

  const defaultDeploymentName = useRef("");
  useEffect(() => {
    if (!chosen) return;
    setDeploymentName((current) => {
      if (!current || current === defaultDeploymentName.current) {
        defaultDeploymentName.current = chosen;
        return chosen;
      }
      return current;
    });
  }, [chosen]);
  /** The whole model on screen, for the facts the plan does not carry. TP
   *  also picks a whole model (each rank holds its own copy), so it reads the
   *  same facts — architecture, holders — to build the primary/worker selects. */
  const wholeModel = isWhole || (isTp && !hasSlicedTp)
    ? wholes.find((w) => w.name === chosen)
    : undefined;
  /** The TpLaunch the node expects: `size` is derived from the workers picked,
   *  never a free number — the select is the single source of truth. */
  const tpPayload = (): TpLaunch => ({
    size: tpGroup?.tp_size ?? tp.worker_node_ids.length + 1,
    // The first TP request must be able to produce the map that lets the
    // operator choose workers. Without a primary yet, use the first holder as
    // the provisional rank 0; workers still remain opt-in map members.
    primary_node_id: tp.primary_node_id
      || tpGroup?.ranks.find((rank) => rank.rank === 0)?.holders[0]
      || wholeModel?.holders[0]
      || "",
    worker_node_ids: tp.worker_node_ids.slice(0, Math.max(0, (tpGroup?.tp_size ?? tp.worker_node_ids.length + 1) - 1)),
  });
  /**
   * Where the last deploy of this is remembered.
   *
   * Namespaced by kind because the two menus are different namespaces: a shard
   * set and a whole model can perfectly well share a name (they usually do —
   * the set is named after the model it was cut from), and one restoring the
   * other's machines would open the screen on a plan nobody chose.
   */
  const memoryKey = isWhole
    ? `whole:${chosen}`
    : isTp
      ? `tensor:${chosen}:${tpGroup?.tp_size ?? tp.worker_node_ids.length + 1}`
      : chosen;

  // Map edits go to the node for validation, but not on every keystroke: the
  // survey behind this route talks to every peer, and the settings are typed
  // digit by digit. Runtime parameters are launch configuration, not topology,
  // so changing them must not restart the full survey and capacity preview.
  const [settled, setSettled] = useState<DeploySelection[] | null>(null);
  /** The topology returned by an automatic plan. An explicit selection that
   * only adds launch overrides must remain on this same query key. */
  const autoTopologyKey = useRef<string | null>(null);
  const queryTopologyKey = (rows: DeploySelection[] | null) => {
    const key = topologyKey(rows);
    return key !== "auto" && key === autoTopologyKey.current ? "auto" : key;
  };
  useEffect(() => {
    const t = setTimeout(() => {
      setSettled(selection);
    }, 260);
    return () => clearTimeout(t);
  }, [selection]);

  const plan = useQuery({
    queryKey: [
      "deploy-plan",
      kind,
      chosen,
      queryTopologyKey(settled),
      merge == null ? "merge-padrão" : `merge-${merge}`,
      strategy,
      JSON.stringify(preferredNodeIds),
      // TP: o plano revalida quando o primary/workers mudam — a checagem de
      // shardabilidade roda no nó, então a troca precisa re-disparar a query.
      isTp ? JSON.stringify(tpPayload()) : "sem-tp",
    ],
    queryFn: () =>
      api.post<DeployPlan>("/api/ui/deploy/plan", {
        group: chosen,
        kind,
        strategy,
        preferred_node_ids: preferredNodeIds,
        ...(settled ? { selection: settled } : {}),
        ...(isTp ? { tp: tpPayload() } : {}),
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
  useEffect(() => {
    if (p?.auto && p.group === chosen && kindOf(p) === kind) {
      autoTopologyKey.current = topologyKey(rowsOf(p));
    }
  }, [chosen, kind, p]);
  useEffect(() => {
    if (p?.params && nodeParams.current == null) nodeParams.current = p.params;
  }, [p]);
  /**
   * O plano é uma fotografia cara do catálogo. As métricas de capacidade vêm
   do cluster, que é atualizado a cada 5 s, para os cards não ficarem presos
   ao momento em que o planner foi consultado.
   */
  const liveMachines = useMemo(() => {
    if (!p) return [];
    return p.machines.map((machine) => {
      const peer = liveCluster.data?.peers.find((item) => item.node_id === machine.node_id);
      const system = peer?.node?.system;
      const gpu = system?.gpus[0];
      const gpuFree = gpu?.vram_total_mb != null && gpu.vram_used_mb != null
        ? Math.max(0, gpu.vram_total_mb - gpu.vram_used_mb)
        : machine.gpu_free_mb;
      return {
        ...machine,
        gpu_free_mb: gpuFree,
        ram_free_mb: system?.ram.available_mb ?? machine.ram_free_mb,
        gpu_used_pct: gpu?.vram_used_pct ?? machine.gpu_used_pct ?? null,
        ram_used_pct: system?.ram.used_pct ?? machine.ram_used_pct ?? null,
        load: peer?.load ?? machine.load,
        // O RTT de uma máquina vem do survey direcional do backend. A rota
        // do cluster é a medição HTTP do gateway e não deve sobrescrevê-lo.
        rtt_ms: machine.rtt_ms,
      };
    });
  }, [liveCluster.data, p]);
  // Capacity is the only part of the plan that depends on RuntimeParams. It
  // cannot be reused after a launch setting changes, but that setting also
  // does not justify a new topology survey. Hide the old projection until the
  // deploy endpoint confirms the fresh state instead of showing stale advice.
  const capacityFresh =
    !params || !p?.params || JSON.stringify(params) === JSON.stringify(p.params);
  const currentRows = selection ?? rowsOf(p);
  const launchOverridesFresh =
    launchOverridesKey(currentRows) === launchOverridesKey(rowsOf(p));
  const displayPlan = p
    ? {
        ...p,
        machines: liveMachines.map((machine) =>
          capacityFresh && launchOverridesFresh
            ? machine
            : { ...machine, capacity: undefined },
        ),
      }
    : undefined;

  useEffect(() => {
    if (inspectedNodeId && p?.machines.some((machine) => machine.node_id === inspectedNodeId)) {
      return;
    }
    const first = p?.steps[0]?.target_node_id ?? p?.machines[0]?.node_id ?? null;
    setInspectedNodeId(first);
  }, [p, inspectedNodeId]);

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
    if (memory.deployment_name) setDeploymentName(memory.deployment_name);
    if (memory.strategy) setStrategy(memory.strategy);
    if (!isWhole) setMerge(memory.merge);
  }, [p, chosen, kind, memoryKey, isWhole]);

  /** What is on screen right now: the operator's edit, or the suggestion. */
  const rows = useMemo(
    () => (selection ?? rowsOf(p)).slice().sort(byLayer),
    [selection, p],
  );
  /** The canvas, TP selectors and card actions all use this same membership.
   *  Complete strategy puts every live machine on the map except in whole mode,
   *  where the map is intentionally limited to one machine. */
  const mapNodeIds = useMemo(
    () => {
      const ids = new Set([...rows.map((row) => row.target_node_id), ...preferredNodeIds]);
      if (strategy === "complete" && !isWhole) {
        for (const machine of liveMachines) ids.add(machine.node_id);
      }
      return ids;
    },
    [isWhole, liveMachines, preferredNodeIds, rows, strategy],
  );
  const mapMachineIds = useMemo(
    () => liveMachines.filter((machine) => mapNodeIds.has(machine.node_id)).map((machine) => machine.node_id),
    [liveMachines, mapNodeIds],
  );
  const tpCandidates = useMemo(
    () => tpGroup ? mapMachineIds : mapMachineIds.filter((nodeId) => wholeModel?.holders.includes(nodeId)),
    [mapMachineIds, tpGroup, wholeModel],
  );
  useEffect(() => {
    if (!isTp || (!wholeModel && !tpGroup)) return;
    const candidates = tpCandidates;
    if (candidates.length === 0) {
      setTp((current) => current.primary_node_id || current.worker_node_ids.length > 0
        ? { primary_node_id: "", worker_node_ids: [] }
        : current);
      return;
    }
    const fresh = tpSyncKey.current !== memoryKey;
    const previousCandidates = tpCandidateIds.current;
    const candidatesExpanded = previousCandidates != null
      && candidates.some((nodeId) => !previousCandidates.includes(nodeId));
    const shouldSeedWorkers = fresh || candidatesExpanded || seedTpWorkers.current;
    tpSyncKey.current = memoryKey;
    tpCandidateIds.current = candidates;
    seedTpWorkers.current = false;
    setTp((current) => {
      const preferredPrimaryChoices = tpGroup?.ranks.find((rank) => rank.rank === 0)?.holders
        .filter((nodeId) => candidates.includes(nodeId)) ?? [];
      const primary = candidates.includes(current.primary_node_id)
        ? current.primary_node_id
        : preserveEmptyPrimary.current
          ? ""
          : fresh
          ? preferredPrimaryChoices[0] ?? candidates[0]
          : "";
      if (!shouldSeedWorkers) {
        const nextWorkers = current.worker_node_ids.filter(
          (nodeId) => nodeId !== primary && candidates.includes(nodeId),
        );
        if (
          primary === current.primary_node_id
          && nextWorkers.length === current.worker_node_ids.length
          && nextWorkers.every((nodeId, index) => nodeId === current.worker_node_ids[index])
        ) {
          return current;
        }
        return { primary_node_id: primary, worker_node_ids: nextWorkers };
      }
      const desiredWorkers = tpGroup?.tp_size
        ? tpGroup.ranks.filter((rank) => rank.rank > 0).map((rank) => rank.holders)
        : [candidates.filter((nodeId) => nodeId !== primary)];
      const used = new Set([primary]);
      const nextWorkers = desiredWorkers.flatMap((holders, index) => {
        const remembered = current.worker_node_ids[index];
        const nodeId = [remembered, ...holders, ...candidates].find(
          (candidate) => !!candidate && candidate !== primary && candidates.includes(candidate) && !used.has(candidate),
        );
        if (!nodeId) return [];
        used.add(nodeId);
        return [nodeId];
      });
      if (
        primary === current.primary_node_id &&
        nextWorkers.length === current.worker_node_ids.length &&
        nextWorkers.every((nodeId, index) => nodeId === current.worker_node_ids[index])
      ) {
        return current;
      }
      return { primary_node_id: primary, worker_node_ids: nextWorkers };
    });
  }, [isTp, tpCandidates, tpGroup, wholeModel]);
  const canvasPlan = useMemo(() => {
    if (!displayPlan || selection == null) return displayPlan;
    const assigned = new Set(rows.map((row) => `${row.target_node_id}:${row.start_layer}-${row.end_layer}`));
    return {
      ...displayPlan,
      steps: displayPlan.steps.filter((step) => assigned.has(`${step.target_node_id}:${step.start_layer}-${step.end_layer}`)),
    };
  }, [displayPlan, rows, selection]);

  /**
   * True while the plan in hand is still the answer to an older question.
   *
   * `isFetching` keeps the indicator tied to the request for the current key,
   * including the tail after React Query has stopped showing placeholder data.
   * `isPlaceholderData` still covers the interval where the previous answer is
   * displayed. The debounce above is the other half - between a map edit and
   * `settled` the query key has not moved yet, so the plan looks current when it
   * is not. Runtime parameter edits do not enter this state because they do not
   * alter topology.
   */
  const syncing =
    plan.isFetching ||
    plan.isPlaceholderData ||
    queryTopologyKey(settled) !== queryTopologyKey(selection);

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

  const tpRankRows = useMemo(() => {
    if (p?.tp_targets && p.tp_targets.length > 0) {
      return p.tp_targets.slice().sort((a, b) => a.rank - b.rank);
    }
    if (!tpGroup) return [];
    return tpGroup.ranks.map((rank) => ({
      rank: rank.rank,
      size: rank.size,
      target_node_id: rank.rank === 0 ? tp.primary_node_id : tp.worker_node_ids[rank.rank - 1] ?? "",
      ready: rank.holders.includes(rank.rank === 0 ? tp.primary_node_id : tp.worker_node_ids[rank.rank - 1] ?? ""),
    }));
  }, [p, tp, tpGroup]);

  /** The settings in force: the operator's edit, or the node's own defaults. */
  const eff: RuntimeParams = params ?? p?.params ?? ({} as RuntimeParams);
  /** The server's GGUF metadata is the only source of truth for MoE. */
  const isMoePlan = p?.is_moe === true;
  /**
   * Runtime mode is checked locally from the already-surveyed model shape.
   * Changing this selector must not make the planner survey the cluster again;
   * the deploy preflight repeats the same check with its fresh survey.
   */
  const inferenceModeReason = !p
    ? null
    : eff.inference_mode === "moe" && !isMoePlan
      ? "MoE exige metadata real de experts no GGUF deste modelo."
      : isTp && isMoePlan
        ? "Tensor Parallelism não suporta MoE nesta versão."
        : null;
  const inferenceModeSupported = inferenceModeReason == null;
  /** Merged units in force: the operator's toggle, or the node's default. */
  const effMerge = merge ?? p?.merge ?? false;
  /**
   * `hidden_state_dtype` só faz algo quando o estado oculto atravessa a
   * fronteira de um peer - ou seja, num pipeline de fatias com faixas
   * adjacentes distintas. Modelo inteiro (uma máquina), Tensor Parallelism
   * (troca por coletiva, não por frame) e um plano de fatias que caiu numa
   * máquina só nunca serializam o estado, então o seletor seria um controle
   * sem efeito. Duas máquinas na mesma faixa são réplicas, não hop.
   */
  const crossesPeerBoundary =
    kind === "shards" &&
    new Set(rows.map((r) => `${r.start_layer}-${r.end_layer}`)).size >= 2;

  function edit(next: (base: DeploySelection[]) => DeploySelection[]) {
    setSelection(next(rows.map((r) => ({ ...r }))).slice().sort(byLayer));
  }

  function inspectMachine(nodeId: string) {
    setInspectedNodeId(nodeId);
    setInspectorTab("machine");
  }

  function togglePrimary(nodeId: string) {
    preserveEmptyPrimary.current = tp.primary_node_id === nodeId;
    seedTpWorkers.current = tp.primary_node_id !== nodeId;
    setTp((current) => ({
      primary_node_id: current.primary_node_id === nodeId ? "" : nodeId,
      worker_node_ids: current.worker_node_ids.filter((candidate) => candidate !== nodeId),
    }));
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

  /** Adiciona uma rota para a faixa, preservando réplicas existentes. */
  function assignRangeToMachine(startLayer: number, endLayer: number, nodeId: string) {
    const candidate = p?.available.find(
      (item) => item.start_layer === startLayer && item.end_layer === endLayer,
    );
    if (!candidate) return;
    setSelection((current) => {
      const base = (current ?? rows)
        .filter(
          (row) =>
            row.start_layer !== candidate.start_layer ||
            row.end_layer !== candidate.end_layer ||
            row.target_node_id !== "",
        )
        .map((row) => ({ ...row }));
      const existing = base.find((row) => row.target_node_id === nodeId);
      const alreadyAssigned = base.some(
        (row) =>
          row.target_node_id === nodeId &&
          row.start_layer === candidate.start_layer &&
          row.end_layer === candidate.end_layer,
      );
      if (alreadyAssigned) return base;
      // Atribuir uma faixa a outro peer cria uma réplica explícita. Nunca
      // remova silenciosamente as faixas que o peer já serve.
      return [
        ...base,
        {
          start_layer: candidate.start_layer,
          end_layer: candidate.end_layer,
          target_node_id: nodeId,
          n_gpu_layers: existing?.n_gpu_layers ?? null,
          offload_kqv: existing?.offload_kqv ?? null,
        },
      ].sort(byLayer);
    });
  }

  /**
   * Tira a faixa desta máquina - o inverso exato de `assignRangeToMachine`.
   *
   * Remove só a linha que o operador clicou (faixa **e** máquina), então as
   * réplicas da mesma faixa em outras máquinas ficam onde estavam, pelo mesmo
   * motivo que atribuir preserva as vizinhas. A máquina continua no canvas
   * mesmo sem faixa nenhuma: um peer sem faixa ainda é uma possibilidade, e
   * tirar uma camada não é o mesmo gesto que "Remover da implantação".
   */
  function unassignRangeFromMachine(startLayer: number, endLayer: number, nodeId: string) {
    edit((base) =>
      base.filter(
        (row) =>
          !(
            row.target_node_id === nodeId &&
            row.start_layer === startLayer &&
            row.end_layer === endLayer
          ),
      ),
    );
  }

  async function deploy() {
    setRunning(true);
    setAsked(false);
    setCapacityAsked(false);
    try {
      const started = await api.post<{ job_id: string }>("/api/ui/deploy", {
        group: chosen,
        kind,
        deployment_name: deploymentName || chosen,
        strategy,
        preferred_node_ids: preferredNodeIds,
        ...(selection ? { selection: rows } : {}),
        ...(params ? { params } : {}),
        ...(merge != null && !isWhole ? { merge } : {}),
        ...(isTp ? { tp: tpPayload() } : {}),
      });
      setJobId(started.job_id);
      // Written from `rows` and `eff`, not from the possibly-null edit state:
      // what comes back next time has to be what was actually deployed,
      // including the parts the operator left on the node's suggestion.
      rememberDeploy(memoryKey, {
        selection: rows,
        params: eff,
        merge: effMerge,
        deployment_name: deploymentName || chosen,
        strategy,
      });
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

  async function updateCacheTypes(part: "k" | "v", value: KvCacheType) {
    const previousK = cacheTypeK;
    const previousV = cacheTypeV;
    const nextK = part === "k" ? value : cacheTypeK;
    const nextV = part === "v" ? value : cacheTypeV;
    setCacheTypeK(nextK);
    setCacheTypeV(nextV);
    setSavingCacheTypes(true);
    try {
      const saved = await api.post<CacheTypesResponse>("/api/ui/cache-types", {
        cache_type_k: nextK,
        cache_type_v: nextV,
      });
      setCacheTypeK(saved.cache_type_k);
      setCacheTypeV(saved.cache_type_v);
      setCacheTypesLoaded(true);
    } catch (e) {
      setCacheTypeK(previousK);
      setCacheTypeV(previousV);
      toast.fromError(t("deploy.cacheUpdateFailed"), e);
    } finally {
      setSavingCacheTypes(false);
    }
  }

  function restoreNodePlan() {
    setSelection(null);
    setSettled(null);
    const defaults = nodeParams.current ?? p?.params ?? null;
    setParams(defaults);
    setMerge(p?.merge ?? null);
    setPreferredNodeIds([]);
    preserveEmptyPrimary.current = true;
    seedTpWorkers.current = false;
    setTp({ primary_node_id: "", worker_node_ids: [] });
    setDeploymentName(chosen);
  }

  function addPeerToDeployment(nodeId: string) {
    if (isWhole && mapMachineIds.length > 0 && !mapMachineIds.includes(nodeId)) {
      toast.warning(
        t("deploy.wholeSingleMachineTitle"),
        t("deploy.wholeSingleMachineDetail"),
      );
      return;
    }
    inspectMachine(nodeId);
    setPreferredNodeIds((current) =>
      current.includes(nodeId) ? current : [...current, nodeId],
    );
    if (isWhole && mapMachineIds.length === 0) {
      // A resposta de fatias anterior pode continuar visível enquanto a
      // consulta `whole` chega. Nunca use essa faixa velha para montar o
      // modelo inteiro: o catálogo é a fonte do intervalo completo.
      const candidate = wholeModel?.total_layers != null
        ? { start_layer: 0, end_layer: wholeModel.total_layers - 1 }
        : p?.kind === "whole"
          ? p.available[0]
          : undefined;
      if (candidate) {
        setSelection([{
          start_layer: candidate.start_layer,
          end_layer: candidate.end_layer,
          target_node_id: nodeId,
          n_gpu_layers: null,
          offload_kqv: null,
        }]);
      }
    }
    // Um peer sem faixa ainda é apenas uma possibilidade no canvas. A seleção
    // só muda quando o operador atribui uma camada, evitando redesenhar a
    // sugestão por causa de um simples arrastar e soltar.
  }

  function removePeerFromDeployment(nodeId: string) {
    const next = rows.filter((row) => row.target_node_id !== nodeId);
    setSelection(next.slice().sort(byLayer));
    setPreferredNodeIds((current) => current.filter((id) => id !== nodeId));
    preserveEmptyPrimary.current = true;
    seedTpWorkers.current = false;
    setTp((current) => ({
      primary_node_id: current.primary_node_id === nodeId ? "" : current.primary_node_id,
      worker_node_ids: current.worker_node_ids.filter((id) => id !== nodeId),
    }));
    const fallback = next[0]?.target_node_id ?? liveMachines.find((machine) => machine.node_id !== nodeId)?.node_id ?? null;
    setInspectedNodeId(fallback);
    setInspectorTab("machine");
  }

  const job = jobs.data?.jobs.find((j) => j.id === jobId);
  const canDeploy =
    validated &&
    !p?.blocked &&
    inferenceModeSupported &&
    rows.length > 0;
  const bottomMachines = liveMachines.filter((machine) => !mapNodeIds.has(machine.node_id));
  /** Máquinas cuja projeção estoura a VRAM (rósea) ou que só cabem porque o
   *  "Ajustar à memória" apararia o pedido (amber) - as duas pedem um aviso
   *  antes de implantar. */
  const capacityWarnMachines = (displayPlan?.machines ?? []).filter(
    (machine) =>
      machine.capacity && (!machine.capacity.fits || machine.capacity.degraded),
  );
  const anyOverCapacity = capacityWarnMachines.some((m) => !m.capacity?.fits);

  /** O gesto de implantar, com os confirms possíveis no caminho: cobertura
   *  parcial (o modelo não fica inteiro no ar) e a projeção de capacidade
   *  (estoura a VRAM, ou o fit apararia o pedido). O primeiro é mais
   *  fundamental, então vem antes. */
  function attemptDeploy() {
    if (!p?.complete) {
      setAsked(true);
    } else if (capacityWarnMachines.length > 0) {
      setCapacityAsked(true);
    } else {
      void deploy();
    }
  }

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
          : !inferenceModeSupported
            ? t("deploy.whyNotBlocked")
          : !validated
            ? t("deploy.whyNotMismatch")
            : rows.length === 0
              ? t("deploy.whyNotNoModel")
              : null;
  const cacheTypeChoices = cacheTypes.data?.choices?.length
    ? cacheTypes.data.choices
    : DEFAULT_CACHE_TYPES;

  function selectKind(value: DeployKind) {
    if (value === kind) return;
    setPickedKind(value);
    // Trocar de fatias para um GGUF inteiro muda a unidade da decisão. A
    // seleção anterior não pode escolher uma máquina por acidente enquanto o
    // novo plano chega: [] significa mapa vazio e deixa a próxima escolha ao
    // operador. Os outros modos continuam usando a sugestão do nó.
    setSelection(value === "whole" ? [] : null);
    setSettled(value === "whole" ? [] : null);
    setPreferredNodeIds([]);
    setJobId(null);
  }

  return (
    <>
      <section
        className="-mx-1 flex h-full min-h-0 flex-col overflow-hidden rounded-2xl border border-line bg-ink/80 shadow-[0_20px_70px_rgba(0,0,0,0.18)]"
        onDragOver={(event) => event.preventDefault()}
        onDrop={(event) => {
          event.preventDefault();
          const nodeId = event.dataTransfer.getData("application/x-pom-node");
          if (nodeId) addPeerToDeployment(nodeId);
        }}
      >
        <div className="border-b border-line bg-surface/90 px-4 py-3 sm:px-5">
          <div className="flex flex-wrap items-center gap-2">
            <div className="mr-auto flex items-center gap-1 rounded-lg border border-line bg-ink p-1">
              {([
                ["shards", "Fatias"],
                ["whole", "Modelo inteiro"],
                ["tensor_parallel", "Tensor"],
              ] as [DeployKind, string][]).map(([value, label]) => (
                <button
                  key={value}
                  type="button"
                  role="radio"
                  aria-label={value === "whole" ? "Um modelo inteiro" : label}
                  aria-checked={kind === value}
                  onClick={() => selectKind(value)}
                  className={`rounded-md px-3 py-1.5 text-[0.72rem] font-medium transition-colors ${
                    kind === value ? "bg-iris text-white" : "text-muted hover:text-fg"
                  }`}
                >
                  {label}
                </button>
              ))}
            </div>
            <div className="flex min-w-[15rem] flex-1 gap-2">
              <Select
                aria-label="Modelo técnico"
                name="model"
                value={isTp && hasSlicedTp ? (tpGroup?.key ?? "") : chosen}
                onChange={(event) => {
                  if (isWhole) setWholeName(event.target.value);
                  else if (isTp && hasSlicedTp) setTensorGroupKey(event.target.value);
                  else if (isTp) setWholeName(event.target.value);
                  else setGroup(event.target.value);
                  setSelection(null);
                  setSettled(null);
                  setPreferredNodeIds([]);
                }}
                className="h-9 min-w-0 flex-1 text-[0.78rem]"
              >
                {isTp && hasSlicedTp
                  ? tensorGroups.map((entry) => (
                      <option key={entry.key} value={entry.key}>
                        {deployModelDisplayName(entry.label)} · TP {entry.tp_size}
                      </option>
                    ))
                  : isWhole || isTp
                  ? wholes.map((item) => (
                      <option key={item.name} value={item.name}>{deployModelDisplayName(item.name)}</option>
                    ))
                  : groups.map((item) => (
                      <option key={item.key} value={item.key}>
                        {deployModelDisplayName(item.label)}
                        {duplicateGroupLabels.get(item.label)! > 1 && item.pomId
                          ? ` · ${item.pomId.slice(0, 8)}`
                          : ""}
                      </option>
                    ))}
              </Select>
              <Input
                aria-label="Nome da implantação"
                name="deployment_name"
                value={deploymentName}
                onChange={(event) => setDeploymentName(event.target.value)}
                placeholder={chosen || "Nome da implantação"}
                className="h-9 min-w-0 flex-1 text-[0.78rem]"
              />
            </div>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-ink p-1">
              {(["simple", "complete"] as DeployStrategy[]).map((value) => (
                <button
                  key={value}
                  type="button"
                  aria-pressed={strategy === value}
                  onClick={() => {
                    setStrategy(value);
                    setSelection(null);
                    setSettled(null);
                    setPreferredNodeIds((current) =>
                      isWhole ? [] : isTp ? current : value === "complete" ? liveMachines.map((machine) => machine.node_id) : [],
                    );
                  }}
                  className={`rounded-md px-2.5 py-1.5 text-[0.7rem] font-medium ${
                    strategy === value ? "bg-aqua/15 text-aqua" : "text-muted hover:text-fg"
                  }`}
                >
                  {value === "simple" ? "Simples" : "Completa"}
                </button>
              ))}
            </div>
            <Button
              variant="outline"
              size="sm"
              type="button"
              disabled={!p || running}
              onClick={restoreNodePlan}
            >
              Voltar ao plano do nó
            </Button>
            <Button
              variant="primary"
              size="sm"
              icon={<Rocket className="h-3.5 w-3.5" />}
              loading={running}
              disabled={!canDeploy}
              onClick={attemptDeploy}
            >
              Implantar
            </Button>
          </div>
          {whyNot && <p className="mt-2 text-right text-[0.7rem] text-muted">{whyNot}</p>}
        </div>

        {answered && p?.blocked && (
          <div className="shrink-0 border-b border-rose/25 bg-rose/[0.06] px-4 py-2 text-[0.68rem] text-rose sm:px-5">
            <span className="font-semibold">Não dá para implantar deste jeito:</span>{" "}{p.blocked}
          </div>
        )}
        {answered && p && !p.blocked && !p.complete && p.missing.length > 0 && (
          <div className="shrink-0 border-b border-amber/25 bg-amber/[0.06] px-4 py-2 text-[0.68rem] text-amber">
            Camadas sem máquina: {p.missing.map(([start, end]) => layerRange(start, end)).join(", ")}
          </div>
        )}
        {job && (
          <div className={`shrink-0 border-b px-4 py-1.5 text-[0.62rem] sm:px-5 ${job.state === "failed" ? "border-rose/40 bg-rose/[0.06]" : "border-line bg-ink/70"}`}>
            <div className="flex items-center justify-between gap-2 text-muted">
              <span className="truncate">{job.step || job.message}</span>
              <span className="shrink-0 font-mono text-faint">
                {job.progress == null ? job.state : `${job.progress.toFixed(0)}%`}
              </span>
            </div>
            {job.state === "failed" && job.message && (
              <p role="alert" className="mt-1 break-words leading-relaxed text-rose">
                {job.message}
              </p>
            )}
            {job.state === "running" && (
              <div className="mt-1 h-1 overflow-hidden rounded-full bg-line">
                <div
                  className="h-full rounded-full bg-iris transition-[width]"
                  style={{ width: `${Math.max(4, Math.min(100, job.progress ?? 4))}%` }}
                />
              </div>
            )}
          </div>
        )}

        <div className={`relative grid min-h-0 flex-1 ${inspectorOpen ? "lg:grid-cols-[minmax(0,1fr)_19rem]" : "lg:grid-cols-[minmax(0,1fr)_2rem]"}`}>
          <div className="flex min-h-[24rem] min-w-0 p-3 sm:p-4 lg:min-h-0">
            {plan.isError ? (
              <div className="flex min-h-[22rem] flex-1 items-center justify-center rounded-xl border border-dashed border-rose/40 p-4">
                <ErrorState error={plan.error} onRetry={() => void plan.refetch()} />
              </div>
            ) : p ? (
              <DeployNetworkCanvas
                plan={canvasPlan ?? p}
                rows={rows}
                inspectedNodeId={inspectedNodeId}
                onInspect={inspectMachine}
                onAddPeer={addPeerToDeployment}
                onRemovePeer={removePeerFromDeployment}
                onDropPeer={addPeerToDeployment}
                labelOf={labelOf}
                mapNodeIds={mapMachineIds}
                primaryCandidateIds={isTp ? tpCandidates : undefined}
                primaryNodeId={isTp ? tp.primary_node_id : undefined}
                onTogglePrimary={isTp ? togglePrimary : undefined}
              />
            ) : (
              <div className="flex min-h-[22rem] flex-1 items-center justify-center rounded-xl border border-dashed border-line-strong p-4">
                <Skeleton className="h-full min-h-[18rem] w-full" />
              </div>
            )}
          </div>

          {inspectorOpen ? <>
            <aside className="relative min-h-0 overflow-y-auto border-l border-line bg-surface/45">
            <div role="tablist" aria-label="Propriedades da implantação" className="sticky top-0 z-10 flex border-b border-line bg-surface/95 backdrop-blur-sm">
              <button
                type="button"
                role="tab"
                aria-selected={inspectorTab === "global"}
                onClick={() => setInspectorTab("global")}
                className={`flex-1 border-b-2 px-2 py-2 text-[0.65rem] font-semibold ${inspectorTab === "global" ? "border-iris text-iris" : "border-transparent text-muted hover:text-fg"}`}
              >
                Globais
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={inspectorTab === "machine"}
                disabled={!inspectedNodeId}
                onClick={() => setInspectorTab("machine")}
                className={`flex-1 border-b-2 px-2 py-2 text-[0.65rem] font-semibold ${inspectorTab === "machine" ? "border-aqua text-aqua" : "border-transparent text-muted hover:text-fg"} disabled:cursor-not-allowed disabled:opacity-40`}
              >
                Máquina: {liveMachines.find((machine) => machine.node_id === inspectedNodeId)?.label ?? "-"}
              </button>
            </div>

            {inspectorTab === "global" ? <>
            <DeploymentInspector
              value={eff}
              onChange={setParams}
              capabilities={p?.capabilities}
              cacheTypes={{
                k: cacheTypeK,
                v: cacheTypeV,
                choices: cacheTypeChoices,
                busy: cacheTypes.isLoading || savingCacheTypes,
                onChange: (part, next) => void updateCacheTypes(part, next),
              }}
              merge={isWhole ? undefined : { value: effMerge, onChange: setMerge }}
              showInferenceMode={isMoePlan}
              inferenceModeAlert={
                inferenceModeSupported
                  ? undefined
                  : (inferenceModeReason ?? "Este caminho não pode honrar o motor escolhido.")
              }
              showHiddenStateDtype={crossesPeerBoundary}
            />
            {isMoePlan && !inferenceModeSupported && (
              <p role="alert" className="border-b border-amber/25 bg-amber/10 px-2 py-1.5 text-[0.6rem] leading-relaxed text-amber">
                {inferenceModeReason ?? "Este caminho não pode honrar o motor escolhido."}
              </p>
            )}
            {isTp && tpGroup && (
              <section className="border-b border-line px-2 py-3">
                <p className="mb-1.5 text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-faint">{hasSlicedTp ? "Grupo Tensor Parallelism" : "Modelo inteiro"}</p>
                <dl className="grid grid-cols-3 gap-2 text-[0.6rem]">
                  <div><dt className="text-faint">Quantização</dt><dd className="font-mono text-muted">{wholeModel?.quant ?? tpGroup?.quant ?? "-"}</dd></div>
                  <div><dt className="text-faint">Camadas</dt><dd className="font-mono text-muted">{wholeModel?.total_layers ?? tpGroup?.total_layers ?? "-"}</dd></div>
                  <div><dt className="text-faint">Tamanho</dt><dd className="font-mono text-muted">{bytes(wholeModel?.size_bytes ?? tpGroup?.size_bytes ?? 0)}</dd></div>
                </dl>
                <p className="mt-1.5 text-[0.6rem] text-faint"><span>{isTp && hasSlicedTp ? "Ranks disponíveis em" : "Está em"}</span>{" "}<span>{(wholeModel?.holders ?? tpGroup?.holders ?? []).map(labelOf).join(", ") || "nenhuma máquina"}</span></p>
                {isTp && hasSlicedTp && (
                  <div className="mt-3 border-t border-line pt-2">
                    <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-aqua">Ranks do grupo TP</p>
                    <div className="mt-1.5 grid gap-1.5 sm:grid-cols-2">
                      {tpRankRows.map((rank) => (
                        <div key={`${rank.rank}-${rank.size}`} className="flex items-center justify-between gap-2 rounded-md border border-aqua/20 bg-aqua/[0.04] px-2 py-1.5 text-[0.62rem]">
                          <span className="font-mono text-aqua">{rank.rank + 1}of{rank.size}</span>
                          <span className="truncate text-muted">{rank.target_node_id ? labelOf(rank.target_node_id) : "não atribuído"}</span>
                          <Badge tone={rank.ready ? "aqua" : "amber"} className="px-1.5 py-0 text-[0.56rem]">{rank.ready ? "pronto" : "aguardando"}</Badge>
                        </div>
                      ))}
                    </div>
                  </div>
                )}
              </section>
            )}

            {isTp && (wholeModel || tpGroup) && (
              <section className="border-b border-line px-2 py-3">
                <p className="mb-1.5 text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-faint">Tensor paralelo</p>
                <label className="grid grid-cols-[minmax(0,1fr)_7rem] items-center gap-2 text-[0.65rem]">
                  <span className="truncate text-muted">Principal</span>
                  <Select
                    aria-label="Máquina principal do Tensor Parallelism"
                    name="tp_primary_node"
                    value={tp.primary_node_id}
                    onChange={(event) => {
                      preserveEmptyPrimary.current = false;
                      setTp((current) => ({
                        ...current,
                        primary_node_id: event.target.value,
                        worker_node_ids: current.worker_node_ids.filter((nodeId) => nodeId !== event.target.value),
                      }));
                    }}
                    compact
                  >
                    <option value="">não atribuído</option>
                    {tpCandidates.map((nodeId) => (
                      <option key={nodeId} value={nodeId}>{labelOf(nodeId)}</option>
                    ))}
                  </Select>
                </label>
                <label className="mt-2 block text-[0.65rem]">
                  <span className="mb-1 block text-muted">Workers</span>
                  <Select
                    aria-label="Workers do Tensor Parallelism"
                    name="tp_worker_node_ids"
                    multiple
                    value={tp.worker_node_ids}
                    onChange={(event) => setTp((current) => ({
                      ...current,
                      worker_node_ids: [...event.target.selectedOptions]
                        .map((option) => option.value)
                        .filter((nodeId) => nodeId !== current.primary_node_id),
                    }))}
                    className="h-16 rounded-md px-1 text-[0.6rem]"
                  >
                    {tpCandidates.filter((nodeId) => nodeId !== tp.primary_node_id).map((nodeId) => (
                      <option key={nodeId} value={nodeId}>{labelOf(nodeId)}</option>
                    ))}
                  </Select>
                </label>
                <p className="mt-1.5 font-mono text-[0.58rem] text-faint">{tpPayload().size} ranks · {hasSlicedTp ? "partes TP pré-fatiadas" : "modelo inteiro em cada peer"}</p>
              </section>
            )}
            </> : <>
            {(() => {
              const machine = liveMachines.find((item) => item.node_id === inspectedNodeId);
              const taking = rows.filter((row) => row.target_node_id === machine?.node_id);
              const first = taking[0];
              if (!machine) return <p className="py-4 text-[0.68rem] text-faint">Selecione uma máquina no canvas.</p>;
              const availableForMachine = p?.available.filter((candidate) => candidate.holders.includes(machine.node_id)) ?? [];
              return (
                <section className="border border-aqua/20 bg-aqua/[0.035] pt-0">
                  <div className="flex items-center justify-between gap-2 border-b border-aqua/20 bg-aqua/10 px-2 py-1.5">
                    <div className="flex min-w-0 items-center gap-2">
                      <p className="truncate text-[0.7rem] font-semibold text-fg">{machine.label}</p>
                      {!taking.length && <Badge tone="neutral" className="px-1.5 py-0 text-[0.58rem]">disponível</Badge>}
                    </div>
                  </div>
                  <div className="px-2 pb-2">
                  <dl className="mt-2 grid grid-cols-2 gap-x-2 gap-y-1.5 text-[0.62rem]">
                    <div><dt className="text-faint">GPU uso / livre</dt><dd className="font-mono text-muted">{machine.gpu_used_pct == null ? "-" : `${machine.gpu_used_pct.toFixed(0)}%`} / {machine.gpu_free_mb > 0 ? megabytes(machine.gpu_free_mb) : "-"}</dd></div>
                    <div><dt className="text-faint">RAM uso / livre</dt><dd className="font-mono text-muted">{machine.ram_used_pct == null ? "-" : `${machine.ram_used_pct.toFixed(0)}%`} / {machine.ram_free_mb > 0 ? megabytes(machine.ram_free_mb) : "-"}</dd></div>
                    <div><dt className="text-faint">Disco livre</dt><dd className="font-mono text-muted">{machine.disk_free_mb > 0 ? megabytes(machine.disk_free_mb) : "-"}</dd></div>
                    <div><dt className="text-faint">RTT local</dt><dd className="font-mono text-muted">{machine.rtt_ms == null ? "-" : `${machine.rtt_ms.toFixed(1)} ms`}</dd></div>
                  </dl>
                  {isTp ? (
                    <>
                      <div className="mt-3 border-t border-line pt-2">
                        <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-faint">Ranks nesta máquina</p>
                        <div className="mt-1.5 flex flex-wrap gap-1">
                          {tpRankRows.filter((rank) => rank.target_node_id === machine.node_id).map((rank) => (
                            <Badge key={`${rank.rank}-${rank.size}`} tone="iris" mono className="px-1.5 py-0 text-[0.62rem]">{rank.rank + 1}of{rank.size}</Badge>
                          ))}
                          {!tpRankRows.some((rank) => rank.target_node_id === machine.node_id) && <span className="text-[0.64rem] text-faint">Nenhum rank atribuído</span>}
                        </div>
                      </div>
                      <div className="mt-3 border-t border-aqua/15 pt-2">
                        <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-aqua">Ranks disponíveis neste peer</p>
                        <div className="mt-1.5 flex flex-wrap gap-1">
                          {(tpGroup?.ranks.filter((rank) => rank.holders.includes(machine.node_id)) ?? []).map((rank) => (
                            <Badge key={`${rank.rank}-${rank.size}-available`} tone="aqua" mono className="px-1.5 py-0 text-[0.62rem]">{rank.rank + 1}of{rank.size}</Badge>
                          ))}
                        </div>
                      </div>
                    </>
                  ) : (
                    <>
                      <div className="mt-3 border-t border-line pt-2">
                        <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-faint">Camadas nesta máquina</p>
                        <div className="mt-1.5 flex flex-wrap gap-1">
                          {taking.length > 0 ? taking.map((row) => (
                            <Badge key={`${row.start_layer}-${row.end_layer}`} tone="iris" mono className="px-1.5 py-0 text-[0.62rem]">{layerRange(row.start_layer, row.end_layer)}</Badge>
                          )) : <span className="text-[0.64rem] text-faint">Nenhuma camada atribuída</span>}
                        </div>
                      </div>
                      <div className="mt-3 border-t border-aqua/15 pt-2">
                        <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-aqua">Camadas disponíveis neste peer</p>
                        <div className="mt-1.5 flex flex-wrap gap-1">
                          {availableForMachine.length > 0 ? availableForMachine.map((candidate) => {
                            const assigned = rows.some((row) => row.start_layer === candidate.start_layer && row.end_layer === candidate.end_layer && row.target_node_id === machine.node_id);
                            const range = layerRange(candidate.start_layer, candidate.end_layer);
                            return (
                              <button
                                key={`${candidate.start_layer}-${candidate.end_layer}`}
                                type="button"
                                aria-label={assigned ? `Remover ${range} de ${machine.label}` : `Atribuir ${range} a ${machine.label}`}
                                title={assigned ? `Tirar ${range} de ${machine.label}` : `Dar ${range} a ${machine.label}`}
                                onClick={() => (assigned
                                  ? unassignRangeFromMachine(candidate.start_layer, candidate.end_layer, machine.node_id)
                                  : assignRangeToMachine(candidate.start_layer, candidate.end_layer, machine.node_id))}
                                className={`border px-1.5 py-0.5 font-mono text-[0.6rem] ${assigned ? "border-aqua/30 bg-aqua/10 text-aqua hover:border-rose/50 hover:bg-rose/10 hover:text-rose" : "border-line bg-ink text-muted hover:border-aqua/50 hover:text-fg"}`}
                              >
                                {assigned ? `${range} ✓` : range}
                              </button>
                            );
                          }) : <span className="text-[0.64rem] text-faint">Nenhuma fatia disponível</span>}
                        </div>
                      </div>
                    </>
                  )}
                  {/* As duas propriedades por máquina usam a mesma linha das
                      globais: o `[i]` antes do nome, na mesma coluna, e o
                      controle à direita. A explicação do KV cache era um
                      parágrafo fixo debaixo do interruptor; agora está no `[i]`,
                      como todas as outras. */}
                  {first && (
                    <div className="mt-3 space-y-1 border-t border-line pt-2">
                      <InspectorRow label="Camadas GPU" tip={t("deploy.knob.n_gpu_layersHint")}>
                        <Input aria-label="Camadas GPU" name={`n_gpu_layers_${machine.node_id}`} type="number" min={0} value={first.n_gpu_layers ?? ""} placeholder={String(eff.n_gpu_layers)} onChange={(event) => setPeerGpu(machine.node_id, event.target.value === "" ? null : Math.max(0, Number(event.target.value) || 0))} compact className="text-right font-mono" />
                      </InspectorRow>
                      <InspectorRow label="KV cache na GPU" tip={t("deploy.knob.offload_kqvHint")}>
                        <Tick on={first.offload_kqv ?? eff.offload_kqv} label="KV cache na GPU" onToggle={() => setPeerKqv(machine.node_id, !(first.offload_kqv ?? eff.offload_kqv))} />
                      </InspectorRow>
                    </div>
                  )}
                  </div>
                </section>
              );
            })()}
            </>}
            </aside>
            <button
              type="button"
              aria-label="Ocultar inspetor"
              title="Ocultar inspetor"
              onClick={() => setInspectorOpen(false)}
              className="group absolute inset-y-0 right-0 z-30 w-6 cursor-pointer focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris focus-visible:ring-offset-1 focus-visible:ring-offset-surface lg:right-[19rem]"
            >
              <span
                aria-hidden="true"
                className="pointer-events-none absolute inset-y-0 right-0 w-[3px] bg-line transition-colors duration-150 group-hover:bg-muted group-focus-visible:bg-muted"
              />
            </button>
          </> : (
            <button
              type="button"
              aria-label="Mostrar inspetor"
              title="Mostrar inspetor"
              onClick={() => setInspectorOpen(true)}
              className="flex h-full w-8 items-center justify-center border-l border-line bg-surface/70 text-faint hover:text-fg"
            >
              <ChevronLeft className="h-4 w-4" />
            </button>
          )}
        </div>

        <div className="shrink-0 border-t border-line bg-surface/75 px-3 py-1.5 sm:px-4">
          <div className="flex min-w-0 items-center gap-1.5 overflow-x-auto">
            <span className="mr-1 shrink-0 text-[0.6rem] font-semibold uppercase tracking-[0.12em] text-faint">Peers</span>
            {bottomMachines.map((machine) => (
              <button key={machine.node_id} type="button" draggable onDragStart={(event) => event.dataTransfer.setData("application/x-pom-node", machine.node_id)} onClick={() => addPeerToDeployment(machine.node_id)} onDoubleClick={() => addPeerToDeployment(machine.node_id)} className="flex min-w-[8rem] shrink-0 items-center gap-1.5 rounded-md border border-line bg-ink px-2 py-1 text-left hover:border-iris/60">
                <ServerCog className="h-3 w-3 shrink-0 text-faint" />
                <span className="min-w-0"><span className="block truncate text-[0.62rem] text-fg">{machine.label}</span><span className="block font-mono text-[0.54rem] text-faint">{machine.rtt_ms == null ? "RTT -" : `${machine.rtt_ms.toFixed(1)} ms`} · carga {machine.load}</span></span>
              </button>
            ))}
            {p && !isWhole && (
              <div className="ml-auto min-w-[12rem] max-w-[24rem] shrink-0 border-l border-line pl-3">
                <Coverage label="Cobertura do modelo" total={p.total_layers} rows={rows} />
              </div>
            )}
          </div>
        </div>

      </section>


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

      <Confirm
        open={capacityAsked}
        title={t(anyOverCapacity ? "deploy.capacityWarnTitle" : "deploy.capacityDegradedTitle")}
        description={
          <>
            {t(anyOverCapacity ? "deploy.capacityWarnDetail" : "deploy.capacityDegradedDetail", {
              machines: capacityWarnMachines.map((machine) => machine.label).join(", "),
            })}
            {capacityWarnMachines.some((machine) => machine.capacity?.note) && (
              <ul className="mt-2 list-disc space-y-1 pl-4 text-[0.72rem] text-muted">
                {capacityWarnMachines
                  .filter((machine) => machine.capacity?.note)
                  .map((machine) => (
                    <li key={machine.node_id}>
                      <span className="font-medium text-fg">{machine.label}:</span>{" "}
                      {machine.capacity?.note}
                    </li>
                  ))}
              </ul>
            )}
          </>
        }
        confirmLabel={t("deploy.deploy")}
        busy={running}
        onConfirm={() => void deploy()}
        onCancel={() => setCapacityAsked(false)}
      />
    </>
  );
}
