import { describe, expect, it } from "vitest";
import { inferenceServicesOf, serviceRunsModel, serviceRunsSlices } from "./inferenceServices";
import type { PeerView } from "./types";

const peer = (node_id: string, services: unknown[]) =>
  ({ node_id, node: { plugin_services: services } }) as unknown as PeerView;

describe("inferenceServicesOf", () => {
  it("groups inference services by plugin and keeps the machines that can take a model", () => {
    const services = inferenceServicesOf([
      peer("a", [
        { plugin_code: "strata", id: "strata", kind: "inference", name: "Strata", status: "ready", architectures: ["qwen4exp"] },
        { plugin_code: "qdrant", id: "rag", kind: "rag", name: "Qdrant", status: "ready" },
      ]),
      peer("b", [{ plugin_code: "strata", id: "strata", kind: "inference", name: "Strata", status: "error" }]),
      peer("c", [{ plugin_code: "strata", id: "strata", kind: "inference", name: "Strata", status: "starting" }]),
      peer("d", []),
    ]);
    expect(services).toEqual([
      { code: "strata", name: "Strata", architectures: ["qwen4exp"], settings: [], pomSettings: [], nodeIds: ["a", "c"], stages: false, stageArchitectures: [], vision: false },
    ]);
    expect(serviceRunsModel(services[0], "qwen4exp")).toBe(true);
    expect(serviceRunsModel(services[0], "llama")).toBe(false);
    expect(serviceRunsModel(services[0], null)).toBe(false);
  });

  it("marks a service whose engine runs slices", () => {
    const services = inferenceServicesOf([
      peer("a", [{ plugin_code: "strata", id: "strata", kind: "inference", name: "Strata", status: "ready" }]),
      peer("b", [{ plugin_code: "strata", id: "strata", kind: "inference", name: "Strata", status: "ready", stages: true, pom_settings: ["hidden_state_dtype"] }]),
    ]);
    expect(services[0].stages).toBe(true);
    expect(services[0].pomSettings).toEqual(["hidden_state_dtype"]);
    expect(services[0].nodeIds).toEqual(["a", "b"]);
  });

  it("runs slices only of the architectures the service names for them", () => {
    const [strata] = inferenceServicesOf([
      peer("a", [{ plugin_code: "strata", id: "strata", kind: "inference", name: "Strata", status: "ready", stages: true,
        architectures: ["qwen4exp", "qwen35moe"], stage_architectures: ["qwen4exp"] }]),
    ]);
    expect(strata.stageArchitectures).toEqual(["qwen4exp"]);
    expect(serviceRunsSlices(strata, "qwen4exp")).toBe(true);
    // Qwen3.6 runs on Strata as the whole model only
    expect(serviceRunsModel(strata, "qwen35moe")).toBe(true);
    expect(serviceRunsSlices(strata, "qwen35moe")).toBe(false);
    // a service that names no stage architectures runs every one of its architectures in slices
    expect(serviceRunsSlices({ ...strata, stageArchitectures: [] }, "qwen35moe")).toBe(true);
  });

  it("does not offer a service no machine can run now", () => {
    expect(
      inferenceServicesOf([
        peer("a", [{ plugin_code: "strata", id: "strata", kind: "inference", name: "Strata", status: "error" }]),
      ]),
    ).toEqual([]);
  });

  it("is empty without peers", () => {
    expect(inferenceServicesOf(undefined)).toEqual([]);
  });
});
