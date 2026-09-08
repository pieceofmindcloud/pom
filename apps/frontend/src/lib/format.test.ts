import { describe, expect, it } from "vitest";
import { deployModelDisplayName } from "./format";

describe("deployModelDisplayName", () => {
  it("remove a extensão GGUF final", () => {
    expect(deployModelDisplayName("Qwen3.8-27B-UD-IQ3_XXS.gguf")).toBe("Qwen3.8-27B-UD-IQ3_XXS");
  });

  it("remove apenas o hash hexadecimal final de oito caracteres", () => {
    expect(deployModelDisplayName("Ornith-1.5-35B-Q4_K_M-42739874")).toBe("Ornith-1.5-35B-Q4_K_M");
    expect(deployModelDisplayName("Ornith-1.5-35B-Q4_K_M-b3bdbf23.gguf")).toBe("Ornith-1.5-35B-Q4_K_M");
  });

  it("preserva hifens e dígitos legítimos que não são um hash", () => {
    expect(deployModelDisplayName("Qwen3.8-27B-UD-IQ3_XXS-v2-2024.gguf")).toBe("Qwen3.8-27B-UD-IQ3_XXS-v2-2024");
  });
});
