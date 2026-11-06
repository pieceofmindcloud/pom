import { describe, expect, it } from "vitest";
import { discountedPrice, promotionFor, salesByContact, termPrice } from "./pluginPricing";

const promotion = { percent: 50, intervals: ["month" as const], applies_to: "forever" as const, payments: null, ends_at: "2099-01-01T00:00:00.000Z" };
const entry = {
  pricing: { monthly: { amount: 1998, currency: "USD", interval: "month" }, annual: { amount: 9998, currency: "USD", interval: "year" } },
  promotion,
};

describe("plugin pricing", () => {
  it("discounts only the terms the promotion covers, as Paddle rounds a percentage", () => {
    expect(termPrice(entry, "month", "en")).toEqual({ full: "$19.98", sale: "$9.99" });
    expect(termPrice(entry, "year", "en")).toEqual({ full: "$99.98", sale: null });
    expect(discountedPrice({ amount: 999, currency: "USD", interval: "month" }, { ...promotion, percent: 15 })?.amount).toBe(849);
  });

  it("stops discounting when the promotion ends, even on a page left open", () => {
    expect(promotionFor(promotion, "month", Date.parse("2098-12-31T23:59:59Z"))).toBe(promotion);
    expect(promotionFor(promotion, "month", Date.parse("2099-01-01T00:00:01Z"))).toBeNull();
  });

  it("sells by contact only a paid plugin the console set so", () => {
    expect(salesByContact({ sales_mode: "contact", requires_license: true })).toBe(true);
    expect(salesByContact({ sales_mode: "contact", requires_license: false })).toBe(false);
    expect(salesByContact({ sales_mode: "checkout", requires_license: true })).toBe(false);
    expect(salesByContact(null)).toBe(false);
  });
});
