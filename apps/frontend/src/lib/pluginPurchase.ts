export const PUBLIC_PLUGIN_STORE_URL = "https://pieceofmind.cloud/plugins/index.html";

export function publicPluginPurchaseUrl(pluginCode: string): string {
  const url = new URL(PUBLIC_PLUGIN_STORE_URL);
  url.searchParams.set("plugin", pluginCode);
  return url.toString();
}

export function navigateToPluginPurchase(pluginCode: string): void {
  window.location.assign(publicPluginPurchaseUrl(pluginCode));
}
