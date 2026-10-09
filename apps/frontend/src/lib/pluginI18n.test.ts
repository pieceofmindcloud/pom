import { afterEach, describe, expect, it, vi } from "vitest";
import { formatMessage } from "../i18n/locales";
import { resetPluginCatalogs, selectedLocales, syncPluginCatalogs, validateCatalog } from "./pluginI18n";
import type { ActivePluginUi } from "./pluginUi";

const plugin = (i18n: Record<string, string> | undefined, version = "1.0.0"): ActivePluginUi => ({
  code: "acme",
  version,
  manifest: {
    schema: "pom-plugin-ui/v1",
    plugin_code: "acme",
    menu: [],
    routes: [],
    screens: {},
    i18n,
    assets: Object.values(i18n ?? {}),
  },
});

const CATALOGS: Record<string, unknown> = {
  "/api/ui/plugins/acme/assets/i18n/en.json": { "acme.title": "Audit", "acme.rows": "{n, plural, one {# row} other {# rows}}" },
  "/api/ui/plugins/acme/assets/i18n/pt-BR.json": { "acme.title": "Auditoria", "acme.rows": "{n, plural, one {# linha} other {# linhas}}" },
};
const fetcher = vi.fn(async (url: string) => {
  if (!(url in CATALOGS)) throw new Error("HTTP 404");
  return CATALOGS[url];
});
const both = { en: "i18n/en.json", "pt-BR": "i18n/pt-BR.json" };

describe("plugin i18n catalogs", () => {
  afterEach(() => {
    resetPluginCatalogs();
    fetcher.mockClear();
    vi.restoreAllMocks();
  });

  it("installs the plugin's own texts for the active locale, without the POM owning them", async () => {
    await syncPluginCatalogs([plugin(both)], "pt-BR", fetcher);
    expect(formatMessage("pt-BR", "acme.title")).toBe("Auditoria");
    expect(formatMessage("pt-BR", "acme.rows", { n: 2 })).toBe("2 linhas");
    expect(formatMessage("en", "acme.title")).toBe("Audit");
  });

  it("loads only the selected languages: the active one plus en as the floor", () => {
    expect(selectedLocales(plugin(both), "pt-BR")).toEqual(["en", "pt-BR"]);
    expect(selectedLocales(plugin(both), "en")).toEqual(["en"]);
    // A language POM does not support or the plugin does not declare does not enter.
    expect(selectedLocales(plugin({ en: "i18n/en.json", fr: "i18n/fr.json" }), "pt-BR")).toEqual(["en"]);
  });

  it("falls back to en when the plugin has no catalog for the active locale", async () => {
    await syncPluginCatalogs([plugin({ en: "i18n/en.json" })], "pt-BR", fetcher);
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(formatMessage("pt-BR", "acme.title")).toBe("Audit");
  });

  it("tolerates a plugin without catalogs and a missing catalog file", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    await syncPluginCatalogs([plugin(undefined)], "en", fetcher);
    expect(fetcher).not.toHaveBeenCalled();
    expect(formatMessage("en", "acme.title")).toBe("acme.title");
    await syncPluginCatalogs([plugin({ en: "i18n/absent.json" }, "2.0.0")], "en", fetcher);
    expect(warn).toHaveBeenCalled();
    expect(formatMessage("en", "acme.title")).toBe("acme.title");
  });

  it("rejects invalid catalogs: wrong namespace, non-string, non-object", async () => {
    expect(validateCatalog("acme", { "acme.a": "x" })).toEqual({ "acme.a": "x" });
    expect(validateCatalog("acme", { "nav.logs": "hijack" })).toBeNull();
    expect(validateCatalog("acme", { "acme.a": 1 })).toBeNull();
    expect(validateCatalog("acme", ["acme.a"])).toBeNull();
    expect(validateCatalog("acme", null)).toBeNull();
    vi.spyOn(console, "warn").mockImplementation(() => {});
    CATALOGS["/api/ui/plugins/acme/assets/i18n/bad.json"] = { "nav.logs": "hijack" };
    await syncPluginCatalogs([plugin({ en: "i18n/bad.json" })], "en", fetcher);
    expect(formatMessage("en", "nav.logs")).not.toBe("hijack");
  });

  it("never overrides a POM message and drops the texts when the plugin goes away", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    await syncPluginCatalogs([plugin(both)], "en", fetcher);
    expect(formatMessage("en", "acme.title")).toBe("Audit");
    await syncPluginCatalogs([], "en", fetcher);
    expect(formatMessage("en", "acme.title")).toBe("acme.title");
  });

  it("reloads when the locale or the plugin version changes, not otherwise", async () => {
    await syncPluginCatalogs([plugin(both)], "en", fetcher);
    await syncPluginCatalogs([plugin(both)], "en", fetcher);
    expect(fetcher).toHaveBeenCalledTimes(1);
    await syncPluginCatalogs([plugin(both)], "pt-BR", fetcher);
    expect(formatMessage("pt-BR", "acme.title")).toBe("Auditoria");
    await syncPluginCatalogs([plugin(both, "1.1.0")], "pt-BR", fetcher);
    expect(fetcher.mock.calls.length).toBeGreaterThan(2);
  });
});
