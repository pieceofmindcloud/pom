// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { deployIdOfImageModel, dropOnStage, emptyChoices, imageModelId, machinesOf, planRequest, removeFromStage } from "./imagePipeline";
import type { ImageDeployment } from "./types";

describe("image pipeline", () => {
  it("drops machines as replicas and keeps a machine once per stage", () => {
    let choices = emptyChoices();
    choices = dropOnStage(choices, "diffusion", "b");
    choices = dropOnStage(choices, "diffusion", "c");
    choices = dropOnStage(choices, "diffusion", "b");
    choices = dropOnStage(choices, "vae", "b");
    expect(choices.diffusion.nodeIds).toEqual(["b", "c"]);
    expect(choices.vae.nodeIds).toEqual(["b"]);
    expect(removeFromStage(choices, "diffusion", "b").diffusion.nodeIds).toEqual(["c"]);
  });

  it("builds the plan request in flow order with only stages that have a file", () => {
    const choices = emptyChoices();
    choices.vae = { file: { name: "ae", size_bytes: 1 }, nodeIds: ["a"] };
    choices.text_encoder = { file: { name: "q", size_bytes: 2 }, nodeIds: ["a"] };
    const request = planRequest("z", choices, true);
    expect(request.stages.map((s) => s.component)).toEqual(["text_encoder", "vae"]);
    expect(request.allow_transfers).toBe(true);
    expect(planRequest("z", choices).allow_transfers).toBeUndefined();
  });

  it("round trips the dashboard id", () => {
    expect(deployIdOfImageModel(imageModelId("img-1"))).toBe("img-1");
    expect(deployIdOfImageModel("qwen3")).toBeNull();
  });

  it("lists each machine once in stage order", () => {
    const deployment = {
      stages: [
        { component: "text_encoder", file: { name: "q", size_bytes: 0 }, replicas: [{ node_id: "a", hostname: "A", status: "ready", in_flight: 0 }] },
        { component: "diffusion", file: { name: "z", size_bytes: 0 }, replicas: [{ node_id: "b", hostname: "B", status: "ready", in_flight: 0 }] },
        { component: "vae", file: { name: "ae", size_bytes: 0 }, replicas: [{ node_id: "a", hostname: "A", status: "ready", in_flight: 0 }] },
      ],
    } as unknown as ImageDeployment;
    expect(machinesOf(deployment)).toEqual([{ node_id: "a", hostname: "A" }, { node_id: "b", hostname: "B" }]);
  });
});
