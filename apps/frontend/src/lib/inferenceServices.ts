/**
 * Whole-model inference services that plugins offer (`kind: "inference"`,
 * for example Strata), as the deploy screen offers them.
 *
 * A machine publishes its plugins' services in `node.plugin_services`. A
 * service of kind `inference` runs a whole model on that machine instead of
 * POM's own engine, and names the GGUF architectures it runs. The deploy
 * screen offers it next to llama.cpp, limited to the machines that run it and
 * to the models of those architectures.
 */
import type { EngineSetting, PeerView, PluginService } from "./types";

export interface InferenceServiceEntry {
  /** The plugin code, what `RuntimeParams.inference_service` carries. */
  code: string;
  name: string;
  architectures: string[];
  /** The settings its engine takes (the first machine that publishes them). */
  settings: EngineSetting[];
  /** Machines that run it and can take a model (not in error). */
  nodeIds: string[];
}

const USABLE: PluginService["status"][] = ["ready", "starting", "installing"];

export function inferenceServicesOf(peers: PeerView[] | undefined): InferenceServiceEntry[] {
  const byCode = new Map<string, InferenceServiceEntry>();
  for (const peer of peers ?? []) {
    for (const service of peer.node?.plugin_services ?? []) {
      if (service.kind !== "inference") continue;
      const entry = byCode.get(service.plugin_code) ?? {
        code: service.plugin_code,
        name: service.name,
        architectures: [],
        settings: [],
        nodeIds: [],
      };
      if (entry.settings.length === 0 && service.settings?.length) entry.settings = service.settings;
      for (const architecture of service.architectures ?? []) {
        if (!entry.architectures.includes(architecture)) entry.architectures.push(architecture);
      }
      if (USABLE.includes(service.status) && !entry.nodeIds.includes(peer.node_id)) {
        entry.nodeIds.push(peer.node_id);
      }
      byCode.set(service.plugin_code, entry);
    }
  }
  return [...byCode.values()].sort((a, b) => a.name.localeCompare(b.name));
}

/** Whether a whole model can run on the service: its architecture is one the
 *  engine declares. A model whose header gave no architecture is not offered. */
export function serviceRunsModel(service: InferenceServiceEntry, architecture: string | null): boolean {
  return architecture != null && service.architectures.includes(architecture);
}
