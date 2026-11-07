import { describe, expect, it } from "vitest";
import { accumulateToolCalls, imageCallOf, sizeFor } from "./chatImageTool";

describe("generate_image tool", () => {
  it("fragments reassemble into one call", () => {
    let calls = accumulateToolCalls([], [{ index: 0, id: "c1", function: { name: "generate_image", arguments: "{\"pro" } }]);
    calls = accumulateToolCalls(calls, [{ index: 0, function: { arguments: "mpt\":\"a red fox in snow\"," } }]);
    calls = accumulateToolCalls(calls, [{ index: 0, function: { arguments: "\"aspect\":\"landscape\",\"use_attached_image\":true}" } }]);
    expect(calls).toHaveLength(1);
    expect(imageCallOf(calls)).toEqual({ prompt: "a red fox in snow", negative: undefined, aspect: "landscape", useAttachedImage: true });
  });

  it("ignores other tools and incomplete calls", () => {
    expect(imageCallOf([{ name: "search", arguments: "{\"prompt\":\"x\"}" }])).toBeNull();
    expect(imageCallOf([{ name: "generate_image", arguments: "{\"prompt\":" }])).toBeNull();
    expect(imageCallOf([{ name: "generate_image", arguments: "{\"prompt\":\"  \"}" }])).toBeNull();
  });

  it("the menu size wins over the aspect", () => {
    expect(sizeFor("portrait", "")).toEqual({ width: 832, height: 1216 });
    expect(sizeFor("portrait", "1344x768")).toEqual({ width: 1344, height: 768 });
    expect(sizeFor(undefined, "")).toEqual({ width: 1024, height: 1024 });
  });
});

import { GENERATE_IMAGE, workflowCallOf, workflowToolName, workflowTools } from "./chatImageTool";
import type { ChatImageWorkflow } from "./types";

const UPSCALE: ChatImageWorkflow = {
  id: "pub:upscale-4x",
  name: "Upscale 4x",
  node_id: "n2",
  description: "Makes a photo four times larger",
  params: [
    { key: "prompt", label: "Prompt", type: "text" },
    { key: "steps", label: "Steps", type: "integer", min: 1, max: 50, default: 20 },
    { key: "sampler", label: "Sampler", type: "choice", options: ["euler", "dpmpp_2m"] },
    { key: "image", label: "Photo", type: "image" },
  ],
};

describe("workflow tools", () => {
  it("one tool per published workflow with params, never named generate_image", () => {
    const legacy: ChatImageWorkflow = { id: "pub:old", name: "Old", node_id: "n2", inputs: ["prompt"] };
    const builtin: ChatImageWorkflow = { id: "builtin:img-1", name: "Z", node_id: "n1" };
    const sneaky: ChatImageWorkflow = { ...UPSCALE, id: "pub:generate-image" };
    const tools = workflowTools([builtin, UPSCALE, legacy, sneaky]);
    const names = tools.map((tool) => tool.function.name);
    expect(names).toEqual(["wf_upscale_4x", "wf_generate_image"]);
    expect(names).not.toContain(GENERATE_IMAGE);
    const schema = tools[0].function.parameters;
    expect(tools[0].function.description).toBe("Makes a photo four times larger");
    expect(schema.properties.steps).toMatchObject({ type: "integer", minimum: 1, maximum: 50, default: 20 });
    expect(schema.properties.sampler).toMatchObject({ enum: ["euler", "dpmpp_2m"] });
    expect(schema.properties.use_attached_image).toMatchObject({ type: "boolean" });
    expect(schema.required).toEqual(["prompt", "sampler"]);
    expect(workflowToolName(UPSCALE)).toBe("wf_upscale_4x");
  });

  it("a wf_ call is dispatched to its workflow with its arguments", () => {
    const call = workflowCallOf([
      { name: "generate_image", arguments: "{\"prompt\":\"x\"}" },
      { name: "wf_upscale_4x", arguments: "{\"prompt\":\"sharper\",\"steps\":12,\"use_attached_image\":true,\"other\":1}" },
    ], [UPSCALE]);
    expect(call?.workflow.id).toBe("pub:upscale-4x");
    expect(call?.params).toEqual({ prompt: "sharper", steps: 12 });
    expect(call?.useAttachedImage).toBe(true);
    expect(workflowCallOf([{ name: "wf_nope", arguments: "{}" }], [UPSCALE])).toBeNull();
    expect(workflowCallOf([{ name: "wf_upscale_4x", arguments: "{\"pro" }], [UPSCALE])).toBeNull();
  });
});
