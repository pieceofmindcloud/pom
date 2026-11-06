/**
 * Inference services that plugins offer (`kind: "inference"`), as the deploy
 * screen offers them.
 *
 * A machine publishes its plugins' services in `node.plugin_services`. A
 * service of kind `inference` runs a whole model on that machine instead of
 * POM's own engine, and names the GGUF architectures it runs; one that
 * publishes `stages` also runs layer slices, one stage per machine. The
 * deploy screen offers it next to llama.cpp, limited to the machines that run
 * it and to the models of those architectures.
 */
import type { EngineSetting, PeerView, PluginService, PomSetting } from "./types";

export interface InferenceServiceEntry {
  /** The plugin code, what `RuntimeParams.inference_service` carries. */
  code: string;
  name: string;
  architectures: string[];
  /** The settings its engine takes (the first machine that publishes them). */
  settings: EngineSetting[];
  /** The POM deployment settings that apply to its engine, as it declares them. */
  pomSettings: PomSetting[];
  /** Machines that run it and can take a model (not in error). */
  nodeIds: string[];
  /** Its engine also runs layer slices across machines. */
  stages: boolean;
  /** The architectures it runs in slices, when the service names them (empty: every one it runs). */
  stageArchitectures: string[];
  /** The model architectures whose drafter file its engine takes from the deployment. */
  draftArchitectures: string[];
  /** Its engine reads images through the deployment's vision projector, as the plugin declares. */
  vision: boolean;
}

const USABLE: PluginService["status"][] = ["ready", "starting", "installing"];

/** The services a deployment on them can run now; see `inferenceServiceCatalog`. */
export function inferenceServicesOf(peers: PeerView[] | undefined): InferenceServiceEntry[] {
  // A service no machine can run now is not offered: the deploy screen then
  // shows no engine choice at all and deploys on llama.cpp.
  return inferenceServiceCatalog(peers).filter((entry) => entry.nodeIds.length > 0);
}

/** Every inference service the cluster announces, usable now or not: what a
 *  deployment made with one of them is shown with (name and settings). */
export function inferenceServiceCatalog(peers: PeerView[] | undefined): InferenceServiceEntry[] {
  const byCode = new Map<string, InferenceServiceEntry>();
  for (const peer of peers ?? []) {
    for (const service of peer.node?.plugin_services ?? []) {
      if (service.kind !== "inference") continue;
      const entry = byCode.get(service.plugin_code) ?? {
        code: service.plugin_code,
        name: service.name,
        architectures: [],
        settings: [],
        pomSettings: [],
        nodeIds: [],
        stages: false,
        stageArchitectures: [],
        draftArchitectures: [],
        vision: false,
      };
      if (service.stages) entry.stages = true;
      if (service.vision) entry.vision = true;
      for (const architecture of service.stage_architectures ?? []) {
        if (!entry.stageArchitectures.includes(architecture)) entry.stageArchitectures.push(architecture);
      }
      for (const architecture of service.draft_architectures ?? []) {
        if (!entry.draftArchitectures.includes(architecture)) entry.draftArchitectures.push(architecture);
      }
      if (entry.settings.length === 0 && service.settings?.length) entry.settings = service.settings;
      for (const setting of service.pom_settings ?? []) {
        if (!entry.pomSettings.includes(setting)) entry.pomSettings.push(setting);
      }
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

/** Whether a slice set of `architecture` can run on the service as layer slices: it runs stages, and its
 *  `stageArchitectures`, when it names any, include that architecture (Strata runs Qwen3.6 whole only). */
export function serviceRunsSlices(service: InferenceServiceEntry, architecture: string | null): boolean {
  if (!service.stages || !serviceRunsModel(service, architecture)) return false;
  return service.stageArchitectures.length === 0 || (architecture != null && service.stageArchitectures.includes(architecture));
}

/** Whether the deploy screen offers a drafter file for a model of `architecture` on the service. Another
 *  service's engine, or another family on this one, would ignore the choice. */
export function serviceTakesDraft(service: InferenceServiceEntry, architecture: string | null): boolean {
  return architecture != null && service.draftArchitectures.includes(architecture);
}

/** The engine a deployment's parameters name, for showing it: the plugin's
 *  name and settings when the cluster announces it, its code otherwise;
 *  `null` for POM's own engine (llama.cpp). */
export function engineOfParams(
  inferenceService: string | null | undefined,
  catalog: InferenceServiceEntry[],
): { code: string; name: string; settings: EngineSetting[]; pomSettings: PomSetting[] } | null {
  if (!inferenceService) return null;
  const entry = catalog.find((service) => service.code === inferenceService);
  return {
    code: inferenceService,
    name: entry?.name ?? inferenceService,
    settings: entry?.settings ?? [],
    pomSettings: entry?.pomSettings ?? [],
  };
}
