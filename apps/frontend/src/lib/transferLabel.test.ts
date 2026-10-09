import { describe, expect, it } from "vitest";
import { transferHash, transferTitle } from "./transferLabel";

/**
 * The notification showed the file's sha256 - `df48d094c3d1023c0dab9e…` - and the
 * operator had no way of knowing which model it was or which layers it carried. The
 * `.pom` travels with the offer and arrives before the first weight byte; the
 * readable name comes from it.
 */
describe("transferTitle", () => {
  const hashName =
    "df48d094c3d1023c0dab9e9d3b1bf982226db9490d1f42150aab016836248158.gguf";

  it("presents the slice by the model and the layers, not by the hash", () => {
    expect(
      transferTitle({
        name: hashName,
        model_label: "qwen3:8b",
        start_layer: 0,
        end_layer: 15,
      }),
    ).toBe("qwen3:8b · L0–15");
  });

  it("a single layer does not become a range", () => {
    expect(
      transferTitle({ name: hashName, model_label: "qwen3:8b", start_layer: 7, end_layer: 7 }),
    ).toBe("qwen3:8b · L7");
  });

  /**
   * Without a manifest neither a range nor a model is invented: a whole GGUF does not
   * belong to any `.pom` and keeps being called by its own name.
   */
  it("a whole model keeps being named by the file name", () => {
    expect(transferTitle({ name: "llama-3.1-8b-Q4_K_M.gguf" })).toBe(
      "llama-3.1-8b-Q4_K_M.gguf",
    );
  });

  it("without a model label, the range still helps more than the hash alone", () => {
    expect(transferTitle({ name: hashName, start_layer: 16, end_layer: 31 })).toBe(
      `L16–31 · ${hashName}`,
    );
  });

  it("a node of a previous version, which sends none of this, does not break the screen", () => {
    expect(transferTitle({ name: hashName })).toBe(hashName);
    expect(
      transferTitle({ name: hashName, model_label: null, start_layer: null, end_layer: null }),
    ).toBe(hashName);
  });
});

/**
 * The hash does not vanish: it is what identifies the part between machines and what
 * names the file on disk. It just stops being the title.
 */
describe("transferHash", () => {
  it("shortens the sha256 to fit beside the name", () => {
    expect(transferHash("df48d094c3d1023c0dab9e9d3b1bf982226db9490d1f42150aab016836248158.gguf"))
      .toBe("df48d094c3d1");
  });

  it("does not repeat a name that is already readable", () => {
    expect(transferHash("llama-3.1-8b-Q4_K_M.gguf")).toBeNull();
    expect(transferHash("fatia_0_15.gguf")).toBeNull();
  });
});
