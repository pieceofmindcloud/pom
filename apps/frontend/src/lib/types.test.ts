import { describe, expect, it } from "vitest";
import { tpPartsOf } from "./types";

describe("tpPartsOf", () => {
  it("treats a catalog response without tp_parts as pipeline-only", () => {
    expect(tpPartsOf({})).toEqual([]);
  });

  it("keeps tensor parts when the catalog provides them", () => {
    const part = { id: "tp-0" } as never;
    expect(tpPartsOf({ tp_parts: [part] })).toEqual([part]);
  });
});
