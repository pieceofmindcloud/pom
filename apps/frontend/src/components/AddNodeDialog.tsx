/**
 * Adds a machine to a model that is already live.
 *
 * The inventory is read after the machine is chosen. That way the dialog does
 * not mix machine A's slices with machine B's, and a whole model remains an
 * explicit option of the same flow.
 */
import { useEffect, useRef, useState } from "react";
import { Route } from "lucide-react";
import { addNodeOptions, api, listDeployments } from "../lib/api";
import type { AddNodeOption, AddNodeOptionsResponse, CacheTypesResponse, KvCacheType, RuntimeParams } from "../lib/types";
import { Confirm } from "./Confirm";
import { DeploymentInspector } from "./DeploymentInspector";
import type { InferenceServiceEntry } from "../lib/inferenceServices";
import { Select } from "./ui";
import { useI18n } from "../i18n/provider";

export interface AddNodePeer {
  node_id: string;
  short_id: string;
  hostname: string;
}

/** Component compatibility with the old list supplied by the Dashboard. */
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
  services = [],
  existingRanges,
  onClose,
  onDone,
}: {
  open: boolean;
  /** Technical identity of the model in the catalog. */
  group?: string;
  /** Active deployment that will receive the new version. */
  deployId?: string;
  peers: AddNodePeer[];
  /** Fallback for old calls and for the initial state of the tests. */
  slices: AddNodeSlice[];
  /** Ranges that are already live, to know what the choice overlaps. */
  existingRanges: [number, number][];
  /** The inference services plugins offer (any machine); the machine picks among those it runs. */
  services?: InferenceServiceEntry[];
  onClose: () => void;
  onDone?: (overlapping: number[]) => void;
}) {
  const { t, locale } = useI18n();
  const [nodeId, setNodeId] = useState(peers[0]?.node_id ?? "");
  const [options, setOptions] = useState<AddNodeOption[]>(group ? [] : toShardOptions(slices));
  const [optionKey, setOptionKey] = useState(() => keyOf(options[0]));
  const [cacheTypes, setCacheTypes] = useState<CacheTypesResponse | null>(null);
  const [visionInfo, setVisionInfo] = useState<AddNodeOptionsResponse["vision"] | null>(null);
  const [vision, setVision] = useState(false);
  // This machine's whole configuration, edited with the deployment inspector. It starts as the deployment's
  // active version, and it is sent to the node as this machine's own override (the version keeps its parameters).
  const [draft, setDraft] = useState<RuntimeParams | null>(null);
  // The machine's engine: "" is llama.cpp, otherwise the code of an inference service the machine runs.
  const [engineCode, setEngineCode] = useState("");
  const [loadingOptions, setLoadingOptions] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Re-seeding is an act of opening the dialog, not of receiving new props. The Dashboard
  // re-polls the peers and must not interrupt the edit in progress.
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open && !wasOpen.current) {
      const firstNode = peers[0]?.node_id ?? "";
      const fallback = group ? [] : toShardOptions(slices);
      setNodeId(firstNode);
      setOptions(fallback);
      setOptionKey(keyOf(fallback[0]));
      setCacheTypes(null);
      setVisionInfo(null);
      setVision(false);
      setDraft(null);
      setEngineCode("");
      setError(null);
    }
    wasOpen.current = open;
  }, [open, peers, slices]);

  // The machine starts with the deployment's configuration; a change of machine starts it over from that same
  // configuration, so one machine's edits never reach another machine.
  useEffect(() => {
    if (!open || !deployId || !nodeId) return;
    let cancelled = false;
    void listDeployments()
      .then((result) => {
        if (cancelled) return;
        const row = result.deployments.find((candidate) => candidate.deployment.deploy_id === deployId);
        const version = row?.deployment.versions.find((candidate) => candidate.version === row.deployment.active_version);
        if (version) setDraft(version.params);
      })
      .catch((reason) => {
        if (!cancelled) setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => {
      cancelled = true;
    };
  }, [open, deployId, nodeId]);

  // The choice is only changed when it disappears. New arrays from polling
  // must not send the operator back to the first machine.
  useEffect(() => {
    if (!open) return;
    if (peers.length > 0 && !peers.some((peer) => peer.node_id === nodeId)) {
      setNodeId(peers[0].node_id);
    }
  }, [open, peers, nodeId]);

  // The catalog and the configuration belong to the chosen machine. The AbortSignal
  // also prevents a slow response from the previous machine from overwriting the
  // new list.
  useEffect(() => {
    if (!open || !group || !nodeId) return;
    const controller = new AbortController();
    setLoadingOptions(true);
    setOptions([]);
    setOptionKey("");
    setCacheTypes(null);
    setVisionInfo(null);
    setError(null);
    void addNodeOptions(group, nodeId, controller.signal)
      .then((result) => {
        setOptions(result.options);
        setCacheTypes(result.cache_types);
        setVisionInfo(result.vision ?? null);
        setVision(Boolean(result.vision?.deployed && result.vision.available));
      })
      .catch((reason) => {
        if (!controller.signal.aborted) {
          setError(reason instanceof Error ? reason.message : String(reason));
        }
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoadingOptions(false);
      });
    return () => controller.abort();
  }, [open, group, nodeId, locale]);

  // The engines this machine runs, and the chosen one (llama.cpp when none).
  const engines = services.filter((service) => service.nodeIds.includes(nodeId));
  const engine = engines.find((service) => service.code === engineCode) ?? null;
  // A service that does not run layer slices takes the whole model on this machine; one that runs slices keeps both.
  const shownOptions = engine && !engine.stages ? options.filter((candidate) => candidate.kind === "whole") : options;
  const option = shownOptions.find((candidate) => keyOf(candidate) === optionKey);
  // The machine's whole configuration, with its engine: what the deployment inspector shows and what is sent.
  const machine: RuntimeParams | null = draft ? { ...draft, inference_service: engine?.code ?? null } : null;

  useEffect(() => {
    if (shownOptions.length > 0 && !shownOptions.some((option) => keyOf(option) === optionKey)) {
      setOptionKey(keyOf(shownOptions[0]));
    }
  }, [shownOptions, optionKey]);

  // A machine that no longer runs the chosen engine goes back to llama.cpp.
  useEffect(() => {
    if (engineCode && !engines.some((service) => service.code === engineCode)) setEngineCode("");
  }, [engines, engineCode]);
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
        ...(machine ? { overrides: { params: machine } } : {}),
        ...(visionInfo ? { vision } : {}),
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
      setError(reason instanceof Error ? reason.message : String(reason));
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

          {engines.length > 0 && (
            <label className="flex flex-col gap-1">
              <span className="text-xs text-muted" title={t("deploy.inferenceServiceHint")}>
                {t("deploy.inferenceService")}
              </span>
              <Select
                name="add_node_engine"
                value={engineCode}
                onChange={(event) => setEngineCode(event.target.value)}
                className="h-9 text-sm"
              >
                <option value="">{t("deploy.inferenceServiceLlama")}</option>
                {engines.map((service) => (
                  <option key={service.code} value={service.code}>{service.name}</option>
                ))}
              </Select>
            </label>
          )}

          <label className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("deploy.addNodeSlice")}</span>
            <select
              className="rounded-md border border-line bg-raised px-2 py-1.5 font-mono text-sm"
              value={optionKey}
              disabled={loadingOptions || shownOptions.length === 0}
              onChange={(event) => setOptionKey(event.target.value)}
            >
              {shownOptions.map((candidate) => (
                <option key={keyOf(candidate)} value={keyOf(candidate)}>
                  {candidate.kind === "whole" ? t("deploy.addNodeWhole") : candidate.label}
                </option>
              ))}
            </select>
            {loadingOptions && <span className="text-xs text-muted">{t("deploy.addNodeLoading")}</span>}
          </label>

          {visionInfo && (visionInfo.available || visionInfo.deployed) && (
            <label className="flex items-start gap-2" title={t("deploy.visionHint")}>
              <input
                type="checkbox"
                className="mt-0.5"
                checked={vision}
                disabled={!visionInfo.available}
                onChange={(event) => setVision(event.target.checked)}
              />
              <span className="flex flex-col gap-0.5">
                <span>{t("deploy.visionToggle")}</span>
                {!visionInfo.available && (
                  <span className="text-xs text-amber">{t("deploy.addNodeVisionMissing")}</span>
                )}
              </span>
            </label>
          )}

          {machine && (
            <div className="max-h-[55vh] overflow-y-auto pr-1">
              <DeploymentInspector
                value={machine}
                onChange={setDraft}
                inferenceService={
                  engine
                    ? { name: engine.name, settings: engine.settings, pomSettings: engine.pomSettings }
                    : undefined
                }
                // a slice carries the hidden state; a whole model takes the prefill/decode role instead
                showHiddenStateDtype={option?.kind !== "whole"}
                cacheTypes={
                  cacheTypes
                    ? {
                        k: cacheTypes.cache_type_k,
                        v: cacheTypes.cache_type_v,
                        choices: choices.length > 0 ? choices : [DEFAULT_CACHE_TYPE],
                        onChange: (part, value) =>
                          setCacheTypes(
                            part === "k"
                              ? { ...cacheTypes, cache_type_k: value }
                              : { ...cacheTypes, cache_type_v: value },
                          ),
                      }
                    : undefined
                }
              />
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

/** An option is identified by the range and the file type. */
function keyOf(option?: Pick<AddNodeOption, "kind" | "start_layer" | "end_layer">): string {
  return option
    ? `${option.kind === "whole" ? "whole:" : ""}${option.start_layer}-${option.end_layer}`
    : "";
}

/** The layers the chosen range shares with those already live. */
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
