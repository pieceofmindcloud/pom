// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchPublicPlugin, fetchPublicPluginCatalog, PLUGIN_CATALOG_PATH, releaseForTarget } from "./pluginCatalog";

function release(overrides: Record<string, unknown> = {}) {
  return {
    os: "linux",
    arch: "x86_64",
    channel: "stable",
    version: "1.2.0",
    published_at: "2026-09-21T14:33:00.000Z",
    ...overrides,
  };
}

function publicPlugin(code: string, overrides: Record<string, unknown> = {}) {
  return {
    code,
    display_name: code,
    description: `${code} description`,
    requires_license: true,
    icon_url: null,
    screenshot_urls: [],
    pricing: { monthly: null, annual: null },
    releases: [release()],
    ...overrides,
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchPublicPluginCatalog", () => {
  it("reads multiple published plugins through this node, on the page's own origin", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      plugins: [
        publicPlugin("alpha"),
        publicPlugin("beta", {
          icon_url: "https://license.pieceofmind.cloud/v1/plugins/beta/images/icon",
          screenshot_urls: Array.from({ length: 5 }, (_, index) =>
            `https://license.pieceofmind.cloud/v1/plugins/beta/images/screenshot_${index + 1}`),
        }),
      ],
    }), { status: 200, headers: { "Content-Type": "application/json" } }));
    vi.stubGlobal("fetch", fetchMock);

    const catalog = await fetchPublicPluginCatalog();

    expect(catalog.plugins.map((plugin) => plugin.code)).toEqual(["alpha", "beta"]);
    expect(catalog.plugins[1].screenshot_urls).toHaveLength(4);
    expect(fetchMock.mock.calls[0][0]).toBe(PLUGIN_CATALOG_PATH);
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ credentials: "same-origin" });
  });

  it("passes the store query to the node", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ plugins: [] }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    await fetchPublicPluginCatalog(undefined, { q: " rag ", page: 2, perPage: 10 });

    expect(fetchMock.mock.calls[0][0]).toBe(`${PLUGIN_CATALOG_PATH}?q=rag&page=2&per_page=10`);
  });

  it("answers null for a plugin the store does not list", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ error: "plugin_not_listed" }), { status: 404 })));

    await expect(fetchPublicPlugin("missing")).resolves.toBeNull();
  });

  it("drops invalid releases and plugins that have no valid release", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      plugins: [
        publicPlugin("mixed", {
          releases: [
            release(),
            release({ version: "", arch: "" }),
            release({ published_at: "not-a-date" }),
          ],
        }),
        publicPlugin("unpublished", { releases: [release({ version: null })] }),
      ],
    }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    const catalog = await fetchPublicPluginCatalog();

    expect(catalog.plugins.map((plugin) => plugin.code)).toEqual(["mixed"]);
    expect(catalog.plugins[0].releases).toHaveLength(1);
    expect(catalog.plugins[0].icon_url).toBeNull();
    expect(catalog.plugins[0].screenshot_urls).toEqual([]);
  });

  it("does not let GitHub repository fields in the license catalog publish install sources", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({
      plugins: [
        publicPlugin("catalog-release", {
          requires_license: false,
          github_repository: "https://github.com/example/ignored-repository",
        }),
        publicPlugin("github-only", {
          requires_license: false,
          github_repository: "https://github.com/example/ignored-repository",
          releases: [],
        }),
      ],
    }), { status: 200 })));

    const catalog = await fetchPublicPluginCatalog();

    expect(catalog.plugins.map((plugin) => plugin.code)).toEqual(["catalog-release"]);
    expect("github_repository" in catalog.plugins[0]).toBe(false);
  });

  it("reads featured, pom_team and a GitHub install suggestion, accepting only plain github.com repositories", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({
      plugins: [
        publicPlugin("community", { requires_license: false, featured: true, pom_team: false, github_url: "https://github.com/dev/pom-plugin-x/" }),
        publicPlugin("official", { pom_team: true, github_url: "https://evil.example/dev/pom-plugin-x" }),
        publicPlugin("deep-path", { github_url: "https://github.com/dev/pom-plugin-x/releases/latest" }),
        publicPlugin("plain-http", { github_url: "http://github.com/dev/pom-plugin-x" }),
        publicPlugin("older-server", {})
      ]
    }), { status: 200 })));

    const catalog = await fetchPublicPluginCatalog();
    const byCode = Object.fromEntries(catalog.plugins.map((plugin) => [plugin.code, plugin]));

    expect(byCode.community).toMatchObject({ featured: true, pom_team: false, github_url: "https://github.com/dev/pom-plugin-x" });
    expect(byCode.official).toMatchObject({ featured: false, pom_team: true, github_url: null });
    expect(byCode["deep-path"]!.github_url).toBeNull();
    expect(byCode["plain-http"]!.github_url).toBeNull();
    expect(byCode["older-server"]).toMatchObject({ featured: false, pom_team: null, github_url: null });
  });

  it("does not load catalog images with embedded credentials", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      plugins: [publicPlugin("safe-image", {
        icon_url: "https://user:secret@license.pieceofmind.cloud/v1/plugins/safe-image/images/icon",
      })],
    }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    const catalog = await fetchPublicPluginCatalog();

    expect(catalog.plugins[0].icon_url).toBeNull();
  });

  it("preserves catalog metadata but ignores activation usage not provided by the public contract", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({
      plugins: [publicPlugin("capacity", {
        description_pt_br: "Descrição em português",
        activation_usage: { active_machines: 2, max_machines: 5 },
      })],
    }), { status: 200 })));

    const catalog = await fetchPublicPluginCatalog();

    expect(catalog.plugins[0].description).toBe("capacity description");
    expect(catalog.plugins[0].description_pt_br).toBe("Descrição em português");
    expect("activation_usage" in catalog.plugins[0]).toBe(false);
  });

  it("does not accept public activation aggregates as license usage", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({
      plugins: [publicPlugin("capacity", {
        description_pt_br: "   ",
        activation_usage: { active_machines: -1, max_machines: 0 },
      })],
    }), { status: 200 })));

    const catalog = await fetchPublicPluginCatalog();

    expect(catalog.plugins[0].description_pt_br).toBeNull();
    expect("activation_usage" in catalog.plugins[0]).toBe(false);
  });

  it("rejects a malformed public catalog response", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ items: [] }), { status: 200 })));

    await expect(fetchPublicPluginCatalog()).rejects.toThrow("Invalid public plugin catalog");
  });
});

describe("releaseForTarget", () => {
  it("selects the matching target instead of the newest release for another platform", () => {
    const releases = [
      release({ os: "macos", arch: "aarch64", version: "2.0.0" }),
      release({ os: "linux", arch: "x86_64", version: "1.5.0" }),
    ];

    expect(releaseForTarget(releases, "linux", "x86_64")?.version).toBe("1.5.0");
  });

  it("returns no release when the target is not published", () => {
    expect(releaseForTarget([release({ os: "macos", arch: "aarch64" })], "linux", "x86_64")).toBeNull();
  });
});

describe("how the site sells a plugin", () => {
  async function read(entry: Record<string, unknown>) {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ plugins: [publicPlugin("logs", entry)] }),
      { status: 200, headers: { "Content-Type": "application/json" } })));
    return (await fetchPublicPluginCatalog()).plugins[0]!;
  }

  it("keeps the sales mode and a running promotion, and defaults an older server to the checkout", async () => {
    expect((await read({})).sales_mode).toBe("checkout");
    expect((await read({})).promotion).toBeNull();
    expect((await read({ sales_mode: "contact" })).sales_mode).toBe("contact");
    const promotion = { percent: 50, intervals: ["month", "year"], applies_to: "payments", payments: 3, ends_at: "2099-01-01T00:00:00.000Z", discount_id: "dsc_x" };
    expect((await read({ promotion })).promotion).toEqual({ percent: 50, intervals: ["month", "year"], applies_to: "payments", payments: 3, ends_at: "2099-01-01T00:00:00.000Z" });
  });

  it("drops a promotion that is malformed or already over", async () => {
    const valid = { percent: 20, intervals: ["month"], applies_to: "first", payments: null, ends_at: "2099-01-01T00:00:00.000Z" };
    for (const promotion of [
      { ...valid, percent: 0 }, { ...valid, percent: 100 }, { ...valid, intervals: ["week"] }, { ...valid, applies_to: "payments" },
      { ...valid, ends_at: "2000-01-01T00:00:00.000Z" }, { ...valid, ends_at: "soon" }, "50%",
    ]) {
      expect((await read({ promotion })).promotion).toBeNull();
    }
  });
});
