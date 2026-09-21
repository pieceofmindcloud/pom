import { afterEach, describe, expect, it, vi } from "vitest";
import type { PublicPluginCatalogEntry } from "./pluginCatalog";
import { createPluginCheckout } from "./pluginCheckout";

const mocks = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
}));

vi.mock("./api", () => ({ api: mocks }));

function plugin(overrides: Partial<PublicPluginCatalogEntry> = {}): PublicPluginCatalogEntry {
  return {
    code: "analytics_plus",
    display_name: "Analytics Plus",
    description: "English description",
    description_pt_br: null,
    requires_license: true,
    icon_url: null,
    screenshot_urls: [],
    pricing: {
      monthly: null,
      annual: { amount: 12000, currency: "BRL", interval: "year" },
    },
    activation_usage: null,
    releases: [{
      os: "linux",
      arch: "x86_64",
      channel: "stable",
      version: "1.0.0",
      published_at: "2026-09-21T14:33:00.000Z",
    }],
    ...overrides,
  };
}

afterEach(() => {
  mocks.get.mockReset();
  mocks.post.mockReset();
});

describe("createPluginCheckout", () => {
  it("opens a matching public Stripe plan through the authenticated same-origin POM API", async () => {
    mocks.get.mockResolvedValue({
        plans: [
          { code: "enterprise", plugin_codes: ["enterprise"], checkout_available: true },
          { code: "analytics-plus", plugin_codes: ["analytics_plus"], checkout_available: true },
        ],
      });
    mocks.post.mockResolvedValue({
        id: "cs_test_checkout_123",
        url: "https://checkout.stripe.com/c/pay/cs_test_checkout_123",
      });

    const checkoutUrl = await createPluginCheckout(plugin(), "pt-BR");

    expect(checkoutUrl).toBe("https://checkout.stripe.com/c/pay/cs_test_checkout_123");
    expect(mocks.get).toHaveBeenCalledWith("/api/ui/plugins/checkout/plans");
    expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/checkout/sessions", {
        plan_code: "analytics-plus",
        plugin_code: "analytics_plus",
        interval: "year",
        locale: "pt-BR",
    });
    expect(mocks.post.mock.calls[0][1]).not.toHaveProperty("license_key");
    expect(mocks.post.mock.calls[0][1]).not.toHaveProperty("lease_token");
  });

  it("does not create a session if the catalog has no available plan for this plugin", async () => {
    mocks.get.mockResolvedValue({
      plans: [{ code: "other", plugin_codes: ["other"], checkout_available: true }],
    });

    await expect(createPluginCheckout(plugin(), "en")).rejects.toThrow("No public checkout plan is available");
    expect(mocks.get).toHaveBeenCalledTimes(1);
    expect(mocks.post).not.toHaveBeenCalled();
  });

  it("refuses an unsafe checkout URL returned by the public API", async () => {
    mocks.get.mockResolvedValue({
      plans: [{ code: "analytics-plus", plugin_codes: ["analytics_plus"], checkout_available: true }],
    });
    mocks.post.mockResolvedValue({ id: "cs_test_checkout_123", url: "javascript:alert(1)" });

    await expect(createPluginCheckout(plugin(), "en")).rejects.toThrow("Invalid checkout URL");
  });
});
