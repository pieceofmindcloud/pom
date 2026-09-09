/**
 * Soma uma máquina a um modelo que já está no ar.
 *
 * O inventário é lido depois que a máquina é escolhida. Assim o diálogo não
 * mistura as fatias da máquina A com as da máquina B, e um modelo inteiro
 * continua sendo uma opção explícita do mesmo fluxo.
 */
import { useEffect, useRef, useState } from "react";
import { Route } from "lucide-react";
import { addNodeOptions, api } from "../lib/api";
import type { AddNodeOption, AddNodeOptionsResponse, CacheTypesResponse, KvCacheType } from "../lib/types";
import { Confirm } from "./Confirm";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

export interface AddNodePeer {
  node_id: string;
  short_id: string;
  hostname: string;
}

/** Compatibilidade do componente com a lista antiga fornecida pelo Painel. */
export interface AddNodeSlice {
  start_layer: number;
  end_layer: number;
  label: string;
}

const DEFAULT_CACHE_TYPE: KvCacheType = "f16";

export function AddNodeDialog({
  open,
  group,
  deployId,
  peers,
  slices,
  existingRanges,
  onClose,
  onDone,
}: {
  open: boolean;
  /** Identidade técnica do modelo no catálogo. */
  group?: string;
  /** Deployment ativo que receberá a nova versão. */
  deployId?: string;
  peers: AddNodePeer[];
  /** Fallback para chamadas antigas e para o estado inicial dos testes. */
  slices: AddNodeSlice[];
  /** Faixas que já estão no ar, para saber o que a escolha sobrepõe. */
  existingRanges: [number, number][];
  onClose: () => void;
  onDone?: (overlapping: number[]) => void;
}) {
  const { t, locale } = useI18n();
  const [nodeId, setNodeId] = useState(peers[0]?.node_id ?? "");
  const [options, setOptions] = useState<AddNodeOption[]>(toShardOptions(slices));
  const [optionKey, setOptionKey] = useState(() => keyOf(options[0]));
  const [cacheTypes, setCacheTypes] = useState<CacheTypesResponse | null>(null);
  const [nGpuLayers, setNGpuLayers] = useState(99);
  const [offloadKqv, setOffloadKqv] = useState(true);
  const [loadingOptions, setLoadingOptions] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Re-semear é ato de abrir o diálogo, não de receber props novas. O Painel
  // repolla os peers e não pode interromper a edição em andamento.
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open && !wasOpen.current) {
      const firstNode = peers[0]?.node_id ?? "";
      const fallback = toShardOptions(slices);
      setNodeId(firstNode);
      setOptions(fallback);
      setOptionKey(keyOf(fallback[0]));
      setCacheTypes(null);
      setNGpuLayers(99);
      setOffloadKqv(true);
      setError(null);
    }
    wasOpen.current = open;
  }, [open, peers, slices]);

  // A escolha só é trocada quando desaparece. Arrays novos do polling não
  // devem devolver o operador para a primeira máquina.
  useEffect(() => {
    if (!open) return;
    if (peers.length > 0 && !peers.some((peer) => peer.node_id === nodeId)) {
      setNodeId(peers[0].node_id);
    }
  }, [open, peers, nodeId]);

  // O catálogo e a configuração pertencem à máquina escolhida. O AbortSignal
  // também impede que uma resposta lenta da máquina anterior sobrescreva a
  // lista nova.
  useEffect(() => {
    if (!open || !group || !nodeId) return;
    const controller = new AbortController();
    setLoadingOptions(true);
    setOptions([]);
    setOptionKey("");
    setCacheTypes(null);
    setError(null);
    void addNodeOptions(group, nodeId, controller.signal)
      .then((result) => {
        setOptions(result.options);
        setCacheTypes(result.cache_types);
        setNGpuLayers(result.defaults.n_gpu_layers);
        setOffloadKqv(result.defaults.offload_kqv);
      })
      .catch((reason) => {
        if (!controller.signal.aborted) {
          setError(localizeServerText(locale, reason instanceof Error ? reason.message : String(reason)));
        }
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoadingOptions(false);
      });
    return () => controller.abort();
  }, [open, group, nodeId, locale]);

  useEffect(() => {
    if (options.length > 0 && !options.some((option) => keyOf(option) === optionKey)) {
      setOptionKey(keyOf(options[0]));
    }
  }, [options, optionKey]);

  const option = options.find((candidate) => keyOf(candidate) === optionKey);
  const overlapping = option
    ? rangeOverlap(existingRanges, [option.start_layer, option.end_layer])
    : [];
  const choices = cacheTypes?.choices ?? [];

  async function addNode() {
    if (!option || !nodeId) return;
    setBusy(true);
    setError(null);
    try {
      const result = await api.post<{
        overlapping_layers: number[];
      }>("/api/ui/deploy/node", {
        group: group ?? "",
        ...(deployId ? { deploy_id: deployId } : {}),
        kind: option.kind,
        target_node_id: nodeId,
        start_layer: option.start_layer,
        end_layer: option.end_layer,
        n_gpu_layers: nGpuLayers,
        offload_kqv: offloadKqv,
        ...(cacheTypes
          ? {
              cache_type_k: cacheTypes.cache_type_k,
              cache_type_v: cacheTypes.cache_type_v,
            }
          : {}),
      });
      onDone?.(result.overlapping_layers ?? []);
      onClose();
    } catch (reason) {
      setError(localizeServerText(locale, reason instanceof Error ? reason.message : String(reason)));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Confirm
      open={open}
      busy={busy}
      title={t("deploy.addNodeTitle")}
      confirmLabel={t("deploy.addNode")}
      onCancel={onClose}
      onConfirm={() => void addNode()}
      description={
        <div className="flex flex-col gap-3 text-sm">
          <label className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("deploy.addNodeMachine")}</span>
            <select
              className="rounded-md border border-line bg-raised px-2 py-1.5 text-sm"
              value={nodeId}
              onChange={(event) => setNodeId(event.target.value)}
            >
              {peers.map((peer) => (
                <option key={peer.node_id} value={peer.node_id}>
                  {peer.hostname || peer.short_id}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("deploy.addNodeSlice")}</span>
            <select
              className="rounded-md border border-line bg-raised px-2 py-1.5 font-mono text-sm"
              value={optionKey}
              disabled={loadingOptions || options.length === 0}
              onChange={(event) => setOptionKey(event.target.value)}
            >
              {options.map((candidate) => (
                <option key={keyOf(candidate)} value={keyOf(candidate)}>
                  {candidate.kind === "whole" ? t("deploy.addNodeWhole") : candidate.label}
                </option>
              ))}
            </select>
            {loadingOptions && <span className="text-xs text-muted">{t("deploy.addNodeLoading")}</span>}
          </label>

          {cacheTypes && (
            <div className="grid gap-2 rounded-lg border border-line bg-raised/40 p-3">
              <div className="eyebrow">{t("deploy.addNodeMachineOptions")}</div>
              <label className="flex items-center justify-between gap-3 text-xs text-muted">
                <span>{t("deploy.knob.n_gpu_layers")}</span>
                <input
                  className="w-24 rounded-md border border-line bg-surface px-2 py-1 text-right font-mono text-xs text-fg"
                  type="number"
                  min={0}
                  value={nGpuLayers}
                  onChange={(event) => setNGpuLayers(Math.max(0, Number(event.target.value) || 0))}
                />
              </label>
              <label className="flex items-center justify-between gap-3 text-xs text-muted">
                <span>{t("deploy.addNodeKvPlacement")}</span>
                <select
                  className="rounded-md border border-line bg-surface px-2 py-1 text-xs text-fg"
                  value={offloadKqv ? "gpu" : "cpu"}
                  onChange={(event) => setOffloadKqv(event.target.value === "gpu")}
                >
                  <option value="gpu">{t("deploy.kvOnGpu")}</option>
                  <option value="cpu">{t("deploy.kvInRam")}</option>
                </select>
              </label>
              <div className="grid grid-cols-2 gap-2">
                <label className="flex flex-col gap-1 text-xs text-muted">
                  <span>{t("deploy.addNodeCacheK")}</span>
                  <select
                    className="rounded-md border border-line bg-surface px-2 py-1 text-xs text-fg"
                    value={cacheTypes.cache_type_k}
                    onChange={(event) => setCacheTypes({ ...cacheTypes, cache_type_k: event.target.value as KvCacheType })}
                  >
                    {(choices.length > 0 ? choices : [DEFAULT_CACHE_TYPE]).map((choice) => (
                      <option key={choice} value={choice}>{choice}</option>
                    ))}
                  </select>
                </label>
                <label className="flex flex-col gap-1 text-xs text-muted">
                  <span>{t("deploy.addNodeCacheV")}</span>
                  <select
                    className="rounded-md border border-line bg-surface px-2 py-1 text-xs text-fg"
                    value={cacheTypes.cache_type_v}
                    onChange={(event) => setCacheTypes({ ...cacheTypes, cache_type_v: event.target.value as KvCacheType })}
                  >
                    {(choices.length > 0 ? choices : [DEFAULT_CACHE_TYPE]).map((choice) => (
                      <option key={choice} value={choice}>{choice}</option>
                    ))}
                  </select>
                </label>
              </div>
            </div>
          )}

          {overlapping.length > 0 && (
            <span className="flex items-start gap-2 text-xs text-iris">
              <Route className="mt-0.5 h-4 w-4 shrink-0" />
              <span>{t("deploy.parallelRoute")}</span>
            </span>
          )}
          {error && <span className="text-xs text-amber">{error}</span>}
        </div>
      }
    />
  );
}

function toShardOptions(slices: AddNodeSlice[]): AddNodeOption[] {
  return slices.map((slice) => ({ ...slice, kind: "shards" as const }));
}

/** Uma opção é identificada pela faixa e pelo tipo de arquivo. */
function keyOf(option?: Pick<AddNodeOption, "kind" | "start_layer" | "end_layer">): string {
  return option
    ? `${option.kind === "whole" ? "whole:" : ""}${option.start_layer}-${option.end_layer}`
    : "";
}

/** As camadas que a faixa escolhida compartilha com as que já estão no ar. */
export function rangeOverlap(
  existing: [number, number][],
  range: [number, number],
): number[] {
  const shared: number[] = [];
  for (let layer = range[0]; layer <= range[1]; layer++) {
    if (existing.some(([start, end]) => layer >= start && layer <= end)) {
      shared.push(layer);
    }
  }
  return shared;
}

// Kept as a local type assertion boundary for older test doubles that return
// the JSON shape without importing the response type.
export type AddNodeOptions = AddNodeOptionsResponse;
