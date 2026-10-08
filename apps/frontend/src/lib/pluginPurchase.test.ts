import { describe, expect, it } from "vitest";
import { PUBLIC_PLUGIN_PURCHASE_URL, publicPluginPurchaseUrl } from "./pluginPurchase";

describe("publicPluginPurchaseUrl", () => {
  it("encodes arbitrary plugin codes as one query parameter on the site's purchase wizard", () => {
    const code = "analytics/plus & 100%";

    const url = new URL(publicPluginPurchaseUrl(code));

    expect(url.origin + url.pathname).toBe(PUBLIC_PLUGIN_PURCHASE_URL);
    expect(url.searchParams.get("plugin")).toBe(code);
    expect(url.search).toBe("?plugin=analytics%2Fplus+%26+100%25");
  });
});
