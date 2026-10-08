/** The site's purchase wizard: account, billing details, Stripe and the license key, in steps. */
export const PUBLIC_PLUGIN_PURCHASE_URL = "https://pieceofmind.cloud/comprar/";

export function publicPluginPurchaseUrl(pluginCode: string): string {
  const url = new URL(PUBLIC_PLUGIN_PURCHASE_URL);
  url.searchParams.set("plugin", pluginCode);
  return url.toString();
}

export function navigateToPluginPurchase(pluginCode: string): void {
  window.location.assign(publicPluginPurchaseUrl(pluginCode));
}
