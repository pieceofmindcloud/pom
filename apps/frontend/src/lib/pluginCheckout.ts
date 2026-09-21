import { PUBLIC_PLUGIN_CATALOG_URL, type PublicPluginCatalogEntry } from "./pluginCatalog";
import { api } from "./api";

const LICENSE_SERVER_ORIGIN = new URL(PUBLIC_PLUGIN_CATALOG_URL).origin;

type CheckoutLocale = "en" | "pt-BR";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function checkoutPlanCode(value: unknown, pluginCode: string): string | null {
  if (!isRecord(value) || !nonEmptyString(value.code) || value.checkout_available !== true) return null;
  if (!Array.isArray(value.plugin_codes) || !value.plugin_codes.includes(pluginCode)) return null;
  return value.code;
}

function checkoutUrl(value: unknown): string | null {
  if (!nonEmptyString(value)) return null;
  try {
    const url = new URL(value);
    const trustedHost = url.hostname === "checkout.stripe.com" || url.origin === LICENSE_SERVER_ORIGIN;
    if (url.protocol !== "https:" || !trustedHost || url.username || url.password) return null;
    return url.toString();
  } catch {
    return null;
  }
}

/** Resolve the public plan and open the same Stripe session flow used by the POM site. */
export async function createPluginCheckout(
  plugin: PublicPluginCatalogEntry,
  locale: CheckoutLocale,
): Promise<string> {
  const plansPayload = await api.get<unknown>("/api/ui/plugins/checkout/plans");
  const plans = isRecord(plansPayload) && Array.isArray(plansPayload.plans) ? plansPayload.plans : [];
  const planCode = plans
    .map((plan) => checkoutPlanCode(plan, plugin.code))
    .find((code): code is string => code !== null);
  if (!planCode) throw new Error("No public checkout plan is available for this plugin");

  const interval = plugin.pricing.monthly ? "month" : "year";
  const session = await api.post<unknown>("/api/ui/plugins/checkout/sessions", {
      plan_code: planCode,
      plugin_code: plugin.code,
      interval,
      locale,
  });
  const url = isRecord(session) ? checkoutUrl(session.url) : null;
  if (!url) throw new Error("Invalid checkout URL");
  return url;
}

export function navigateToCheckout(url: string): void {
  window.location.assign(url);
}
