import { describe, expect, it } from "vitest";
import { NAV, navFooterForProfile, navForProfile } from "./nav";

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

  it("groups request and deployment logs under Logs", () => {
    const logs = NAV.find((section) => section.labelKey === "nav.logs");

    expect(logs?.to).toBeUndefined();
    expect(logs?.children?.map((child) => [child.labelKey, child.to])).toEqual([
      ["nav.requestLogs", "/logs"],
      ["nav.deploymentLogs", "/implantacoes/logs"],
    ]);
  });

  it("gives statistics its own top-level destination after logs", () => {
    const labels = NAV.map((section) => section.labelKey);
    expect(labels.indexOf("nav.statistics")).toBeGreaterThan(labels.indexOf("nav.logs"));
    expect(NAV.find((section) => section.labelKey === "nav.statistics")).toMatchObject({
      to: "/estatisticas",
    });
  });

  it("places Preferences under Settings", () => {
    const settings = NAV.find((section) => section.labelKey === "nav.settings");

    expect(settings?.children?.map((child) => [child.labelKey, child.to])).toContainEqual([
      "nav.preferences",
      "/config/preferencias",
    ]);
  });

  it("keeps network chat separate and removes wizard and connection keys from the menu", () => {
    expect(NAV.map((section) => section.labelKey)).toContain("nav.networkChat");
    expect(NAV.flatMap((section) => section.children ?? []).map((child) => child.labelKey)).not.toContain("nav.setupWizard");
    expect(NAV.flatMap((section) => section.children ?? []).map((child) => child.labelKey)).not.toContain("nav.connectionKeys");
  });
});

describe("navForProfile", () => {
  it("mostra ao administrador o menu inteiro", () => {
    const labels = navForProfile("admin").map((s) => s.labelKey);
    expect(labels).toEqual(NAV.map((s) => s.labelKey));
    expect(navFooterForProfile("admin")).toEqual([]);
    expect(navFooterForProfile("node")).toEqual([]);
  });

  it("tira do nó implantar, implantações e logs", () => {
    const labels = navForProfile("node").map((s) => s.labelKey);
    expect(labels).not.toContain("nav.deploy");
    expect(labels).not.toContain("nav.deployments");
    // Logs é uma tela restrita ao administrador.
    expect(labels).not.toContain("nav.logs");
    expect(labels).not.toContain("nav.statistics");
  });

  it("preserva no nó tudo que ninguém restringiu", () => {
    const sections = navForProfile("node");
    const labels = sections.map((s) => s.labelKey);
    expect(labels).toContain("nav.dashboard");
    expect(labels).toContain("nav.chat");
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

  it("mantém Rede e Configurações como menus principais", () => {
    const settings = navForProfile("node").find((section) => section.labelKey === "nav.settings");
    const network = navForProfile("node").find((section) => section.labelKey === "nav.network");
    expect(network?.children?.map((child) => child.to)).toEqual([
      "/config/rede",
      "/config/rede/participantes",
      "/config/administracao",
      "/config/compartilhamento",
      "/config/keys/api",
    ]);
    expect(network?.children?.map((child) => child.labelKey)).toEqual([
      "nav.networkInfo",
      "nav.networkParticipants",
      "nav.administrators",
      "nav.sharing",
      "nav.apiKeys",
    ]);
    expect(settings?.children?.map((child) => child.to)).toEqual([
      "/config/preferencias",
      "/config/armazenamento",
      "/config/preferencias/admin",
    ]);

    const inferSettings = navForProfile("infer");
    expect(inferSettings.some((section) => section.labelKey === "nav.network")).toBe(true);
    expect(inferSettings.flatMap((section) => section.children ?? []).some((child) => child.to === "/config/rede/participantes")).toBe(false);
  });

  it("deixa a rede no menu principal do infer e move docs/config para o rodapé", () => {
    expect(navForProfile("infer").map((section) => section.labelKey)).toEqual([
      "nav.publicNetworks",
      "nav.networkChat",
      "nav.network",
    ]);
    expect(navFooterForProfile("infer").map((item) => [item.labelKey, item.to])).toEqual([
      ["nav.documentation", "/documentacao"],
      ["nav.settings", "/config/preferencias"],
    ]);
  });

  it("oferece o compartilhamento aos três perfis", () => {
    for (const profile of ["admin", "node", "infer"] as const) {
      const destinos = navForProfile(profile).flatMap((section) => [
        section.to,
        ...(section.children ?? []).map((child) => child.to),
      ]);
      expect(destinos).toContain("/config/compartilhamento");
    }
  });

  /** Filtrar não pode reescrever `NAV`: o menu do outro perfil viria podado. */
  it("não modifica a tabela de origem", () => {
    const before = JSON.stringify(NAV.map((s) => (s.children ?? []).map((c) => c.to)));
    navForProfile("node");
    expect(JSON.stringify(NAV.map((s) => (s.children ?? []).map((c) => c.to)))).toBe(before);
  });
});
