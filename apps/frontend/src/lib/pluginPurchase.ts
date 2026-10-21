/** The site's purchase wizard: account, billing details, Paddle and the license key, in steps. */
export const PUBLIC_PLUGIN_PURCHASE_URL = "https://pieceofmind.cloud/comprar/";

export interface PurchaseOptions {
  /** The billing period preselected in the wizard. */
  interval?: "month" | "year";
  /** The interface language, so the wizard and its prices open in the same one. */
  locale?: string;
}

export function publicPluginPurchaseUrl(pluginCode: string, options: PurchaseOptions = {}): string {
  const url = new URL(PUBLIC_PLUGIN_PURCHASE_URL);
  url.searchParams.set("plugin", pluginCode);
  if (options.interval) url.searchParams.set("interval", options.interval);
  if (options.locale) url.searchParams.set("lang", options.locale.toLowerCase().startsWith("pt") ? "pt" : "en");
  return url.toString();
}

/**
 * Opens the purchase in a new browser tab: POM may be running inside a desktop
 * webview (WebView2, WebKitGTK, the macOS panel), which hands a new window to
 * the system browser, so the purchase happens outside and POM keeps running.
 */
export function navigateToPluginPurchase(pluginCode: string, options: PurchaseOptions = {}): void {
  window.open(publicPluginPurchaseUrl(pluginCode, options), "_blank", "noopener,noreferrer");
}
