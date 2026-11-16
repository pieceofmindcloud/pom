import { describe, expect, it } from "vitest";
import { chatModelOptions } from "./chatModels";

describe("chatModelOptions", () => {
  it("lists a deployment once, without its per-head aliases or deploy selectors", () => {
    expect(
      chatModelOptions(["Qwen_FlashNext", "Qwen_FlashNext@node-a", "Qwen_FlashNext@node-b", "other@d-1234abcd", "llama"]),
    ).toEqual(["Qwen_FlashNext", "llama"]);
  });
});
