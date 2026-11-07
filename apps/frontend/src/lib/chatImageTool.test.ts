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
