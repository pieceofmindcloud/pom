import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { THIRD_PARTY_LICENSES } from "./licenses.generated";

// Generated together by scripts/build-licenses.mjs; the release packages carry the notices file.
const notices = readFileSync(join(__dirname, "..", "..", "..", "..", "THIRD-PARTY-NOTICES.txt"), "utf8");

describe("THIRD-PARTY-NOTICES.txt", () => {
  it("lists every package of the About catalog, so neither was regenerated without the other", () => {
    const listed = new Set(notices.split("\n"));
    const absent = THIRD_PARTY_LICENSES.filter(
      (entry) => !listed.has(`${entry.name} ${entry.version} - ${entry.license} - ${entry.repository}`),
    );
    expect(absent.map((entry) => `${entry.name} ${entry.version}`)).toEqual([]);
    expect(notices).toContain(`PACKAGES (${THIRD_PARTY_LICENSES.length})`);
  });

  it("carries POM's MIT license and NVIDIA's terms", () => {
    expect(notices).toContain("POM is licensed under the MIT License");
    expect(notices).toContain("https://docs.nvidia.com/cuda/eula/");
    expect(notices).toContain("Copyright (c) 2023-2026 The ggml authors");
  });
});
