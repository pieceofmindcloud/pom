import { describe, expect, it } from "vitest";
import { deployModelDisplayName } from "./format";

describe("deployModelDisplayName", () => {
  it("removes the trailing GGUF extension", () => {
    expect(deployModelDisplayName("Qwen3.8-27B-UD-IQ3_XXS.gguf")).toBe("Qwen3.8-27B-UD-IQ3_XXS");
  });

  it("removes only the trailing eight-character hexadecimal hash", () => {
    expect(deployModelDisplayName("Ornith-1.5-35B-Q4_K_M-42739874")).toBe("Ornith-1.5-35B-Q4_K_M");
    expect(deployModelDisplayName("Ornith-1.5-35B-Q4_K_M-b3bdbf23.gguf")).toBe("Ornith-1.5-35B-Q4_K_M");
  });

  it("preserves legitimate hyphens and digits that are not a hash", () => {
    expect(deployModelDisplayName("Qwen3.8-27B-UD-IQ3_XXS-v2-2024.gguf")).toBe("Qwen3.8-27B-UD-IQ3_XXS-v2-2024");
  });
});
