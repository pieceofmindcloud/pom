// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { overridesBody } from "./NodeOverridesPanel";

const empty = { ctx: "", sessions: "", gpuLayers: "", offload: "inherit" as const };

describe("overridesBody", () => {
  it("sends only the fields the operator set", () => {
    expect(overridesBody(empty)).toEqual({});
    expect(overridesBody({ ...empty, ctx: "4096" })).toEqual({ n_ctx: 4096 });
    expect(overridesBody({ ...empty, sessions: "2", offload: "cpu" })).toEqual({ max_sessions: 2, offload_kqv: false });
    expect(overridesBody({ ...empty, gpuLayers: "20", offload: "gpu" })).toEqual({ n_gpu_layers: 20, offload_kqv: true });
  });
});
