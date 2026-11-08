import { describe, expect, it } from "vitest";
import { downloadCompanions } from "./modelProvider";

const file = (filename: string) => ({ filename, size_bytes: 1 });

describe("downloadCompanions", () => {
  it("takes the other parts of a split GGUF and offers projectors and MTP drafts", () => {
    const files = [
      file("M-Q4-00001-of-00002.gguf"),
      file("M-Q4-00002-of-00002.gguf"),
      file("M-Q8-00001-of-00003.gguf"),
      file("mmproj-M-f16.gguf"),
      file("M-MTP-Q8_0.gguf"),
      file("M-Q5.gguf"),
    ];
    const { parts, sidecars } = downloadCompanions("M-Q4-00001-of-00002.gguf", files);
    expect(parts.map((f) => f.filename)).toEqual(["M-Q4-00002-of-00002.gguf"]);
    expect(sidecars.map((f) => f.filename)).toEqual(["mmproj-M-f16.gguf", "M-MTP-Q8_0.gguf"]);
  });

  it("has no parts for a single file", () => {
    expect(downloadCompanions("M-Q5.gguf", [file("M-Q5.gguf"), file("smtp-notes.gguf")])).toEqual({ parts: [], sidecars: [] });
  });
});
