import { describe, expect, it } from "vitest";
import { NAV, navForProfile } from "./nav";

describe("navigation structure", () => {
  it("puts Hugging Face model search first under Models", () => {
    const models = NAV.find((section) => section.labelKey === "nav.models");

    expect(models?.children?.map((child) => child.to)).toEqual([
      "/modelos/remotos",
      "/modelos",
      "/modelos/fatiar",
      "/fatias",
    ]);
    expect(models?.children?.[0]?.labelKey).toBe("nav.remoteModels");
    expect(NAV.some((section) => section.labelKey === "nav.shardedModels")).toBe(false);
    expect(NAV.flatMap((section) => section.children ?? []).some(
      (child) => child.to === "/fatias/andamento",
    )).toBe(false);
  });

  it("nests network deployment and history under Deployments", () => {
    const deployments = NAV.find((section) => section.labelKey === "nav.deployments");

    expect(deployments?.to).toBeUndefined();
    expect(deployments?.children?.map((child) => [child.labelKey, child.to])).toEqual([
      ["nav.deploy", "/implantar"],
      ["nav.history", "/implantacoes"],
    ]);
    expect(NAV.some((section) => section.labelKey === "nav.deploy")).toBe(false);
  });

  it("opens Logs directly without a submenu", () => {
    const logs = NAV.find((section) => section.labelKey === "nav.logs");

    expect(logs?.to).toBe("/logs");
    expect(logs?.children).toBeUndefined();
  });

  it("places Preferences under Settings", () => {
    const settings = NAV.find((section) => section.labelKey === "nav.settings");

    expect(settings?.children?.map((child) => [child.labelKey, child.to])).toContainEqual([
      "nav.preferences",
      "/config/preferencias",
    ]);
  });
});

describe("navForProfile", () => {
  it("mostra ao administrador o menu inteiro", () => {
    const labels = navForProfile("admin").map((s) => s.labelKey);
    expect(labels).toEqual(NAV.map((s) => s.labelKey));
  });

  it("tira do nó implantar, implantações e logs", () => {
    const labels = navForProfile("node").map((s) => s.labelKey);
    expect(labels).not.toContain("nav.deploy");
    expect(labels).not.toContain("nav.deployments");
    // Logs é uma tela restrita ao administrador.
    expect(labels).not.toContain("nav.logs");
  });

  it("preserva no nó tudo que ninguém restringiu", () => {
    const sections = navForProfile("node");
    const labels = sections.map((s) => s.labelKey);
    expect(labels).toContain("nav.dashboard");
    expect(labels).toContain("nav.testChat");
    expect(labels).toContain("nav.models");
    expect(labels).toContain("nav.settings");
    const models = sections.find((s) => s.labelKey === "nav.models");
    expect(models?.children?.map((c) => c.to)).toEqual([
      "/modelos/remotos",
      "/modelos",
      "/modelos/fatiar",
      "/fatias",
    ]);
  });

  /** Filtrar não pode reescrever `NAV`: o menu do outro perfil viria podado. */
  it("não modifica a tabela de origem", () => {
    const before = JSON.stringify(NAV.map((s) => (s.children ?? []).map((c) => c.to)));
    navForProfile("node");
    expect(JSON.stringify(NAV.map((s) => (s.children ?? []).map((c) => c.to)))).toBe(before);
  });
});
