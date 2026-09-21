import { describe, expect, it } from "vitest";
import { PUBLIC_PLUGIN_STORE_URL, publicPluginPurchaseUrl } from "./pluginPurchase";

describe("publicPluginPurchaseUrl", () => {
  it("encodes arbitrary plugin codes as one query parameter on the public store", () => {
    const code = "analytics/plus & 100%";

    const url = new URL(publicPluginPurchaseUrl(code));

    expect(url.origin + url.pathname).toBe(PUBLIC_PLUGIN_STORE_URL);
    expect(url.searchParams.get("plugin")).toBe(code);
    expect(url.search).toBe("?plugin=analytics%2Fplus+%26+100%25");
  });
});
