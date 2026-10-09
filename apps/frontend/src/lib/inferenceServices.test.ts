import { describe, expect, it } from "vitest";
import { inferenceServicesOf, serviceRunsModel } from "./inferenceServices";
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
      { code: "strata", name: "Strata", architectures: ["qwen4exp"], settings: [], nodeIds: ["a", "c"] },
    ]);
    expect(serviceRunsModel(services[0], "qwen4exp")).toBe(true);
    expect(serviceRunsModel(services[0], "llama")).toBe(false);
    expect(serviceRunsModel(services[0], null)).toBe(false);
  });

  it("is empty without peers", () => {
    expect(inferenceServicesOf(undefined)).toEqual([]);
  });
});
