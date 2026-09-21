import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchPublicPluginCatalog, PUBLIC_PLUGIN_CATALOG_URL } from "./pluginCatalog";

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
  it("reads multiple published plugins without sending credentials", async () => {
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
    expect(fetchMock).toHaveBeenCalledWith(PUBLIC_PLUGIN_CATALOG_URL, {
      credentials: "omit",
      signal: undefined,
    });
    expect(fetchMock.mock.calls[0][1]).not.toHaveProperty("headers");
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

  it("rejects a malformed public catalog response", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ items: [] }), { status: 200 })));

    await expect(fetchPublicPluginCatalog()).rejects.toThrow("Invalid public plugin catalog");
  });
});
