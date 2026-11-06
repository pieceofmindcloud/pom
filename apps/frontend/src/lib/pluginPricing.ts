import type { PublicPluginCatalogEntry, PublicPluginPrice, PublicPluginPromotion } from "./pluginCatalog";

/**
 * What a plugin's price looks like in POM, from the public catalog: nothing
 * announced for a "talk to sales" plugin, the base price otherwise, and while a
 * promotion runs, the base price beside the discounted one. Prices are the
 * license server's base amounts (before the buyer's taxes, which the checkout
 * adds); the discount is the same percentage Paddle applies to that base.
 */

export type PlanInterval = "month" | "year";

export function formatPluginPrice(price: PublicPluginPrice | null | undefined, locale: string): string | null {
  if (!price || price.amount === null || price.amount === undefined || !price.currency) return null;
  try {
    return new Intl.NumberFormat(locale, { style: "currency", currency: price.currency.toUpperCase() }).format(price.amount / 100);
  } catch {
    return null;
  }
}

/** Whether a "talk to sales" plugin: no price to show, the team makes each customer an offer. */
export function salesByContact(entry: Pick<PublicPluginCatalogEntry, "sales_mode" | "requires_license"> | null | undefined): boolean {
  return entry?.sales_mode === "contact" && entry.requires_license !== false;
}

/** The promotion when it discounts this term, and only until it ends (a page may stay open past that). */
export function promotionFor(promotion: PublicPluginPromotion | null | undefined, interval: PlanInterval, now = Date.now()): PublicPluginPromotion | null {
  return promotion && promotion.intervals.includes(interval) && Date.parse(promotion.ends_at) > now ? promotion : null;
}

/** The term's price after the promotion, rounded to the currency's smallest unit as Paddle rounds a percentage. */
export function discountedPrice(price: PublicPluginPrice | null | undefined, promotion: PublicPluginPromotion | null): PublicPluginPrice | null {
  if (!price || price.amount === null || !promotion) return null;
  return { ...price, amount: Math.round(price.amount * (100 - promotion.percent) / 100) };
}

export interface TermPrice {
  /** The base price, formatted; null when the catalog has none. */
  full: string | null;
  /** The discounted price while a promotion covers this term. */
  sale: string | null;
}

export function termPrice(entry: Pick<PublicPluginCatalogEntry, "pricing" | "promotion"> | null | undefined, interval: PlanInterval, locale: string): TermPrice {
  const price = interval === "year" ? entry?.pricing.annual : entry?.pricing.monthly;
  const full = formatPluginPrice(price, locale);
  const sale = full ? formatPluginPrice(discountedPrice(price, promotionFor(entry?.promotion, interval)), locale) : null;
  return { full, sale: sale && sale !== full ? sale : null };
}

/** The promotion's end, as a date in the interface's language. */
export function promotionEnd(promotion: PublicPluginPromotion, locale: string): string {
  return new Intl.DateTimeFormat(locale, { day: "numeric", month: "long" }).format(new Date(promotion.ends_at));
}
