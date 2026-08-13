import { describe, expect, it } from "vitest";
import { edgeStyle } from "./ModelNetworkCanvas";

describe("edgeStyle", () => {
  it("pinta a rota principal de verde, sólida e com seta", () => {
    const s = edgeStyle("principal");
    expect(s.className).toContain("stroke-aqua");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeTruthy();
  });

  it("pinta rota paralela de azul, sólida e sem seta", () => {
    const s = edgeStyle("paralela");
    expect(s.className).toContain("stroke-iris");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeUndefined();
  });

  it("pinta rota inativa de cinza tracejado", () => {
    const s = edgeStyle("inativa");
    expect(s.className).toContain("stroke-faint");
    expect(s.strokeDasharray).toBe("7 7");
  });
});
