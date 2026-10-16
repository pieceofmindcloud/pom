/**
 * Plugin text catalogs.
 *
 * Each plugin installs its own texts together with the backend and the interface:
 * the manifest lists, per language, a flat JSON asset `{"<plugin_code>.key":
 * "ICU text"}`. This module loads the **selected** languages (the active one and
 * `en` as a floor, only those the application supports), validates the format
 * again in the browser and hands it to POM's same `formatMessage`. A missing,
 * unreadable or invalid catalog is discarded with a warning: the screen falls
 * back to `en` or to the key, and never breaks the shell.
 */
import { SUPPORTED_LOCALES, clearPluginMessages, setPluginMessages, type Locale } from "../i18n/locales";
import { pluginAssetUrl, type ActivePluginUi } from "./pluginUi";

export type CatalogFetcher = (url: string) => Promise<unknown>;

const jsonFetcher: CatalogFetcher = async (url) => {
  const response = await fetch(url, { credentials: "same-origin" });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
};

/** Accepted texts: flat object, keys under `<code>.`, string values. */
export function validateCatalog(code: string, value: unknown): Record<string, string> | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const entries = Object.entries(value as Record<string, unknown>);
  if (entries.length > 5000) return null;
  const out: Record<string, string> = {};
  for (const [key, text] of entries) {
    if (!key.startsWith(`${code}.`) || typeof text !== "string" || text.length > 8192) return null;
    out[key] = text;
  }
  return out;
}

/** Languages to load: the active one and `en`, if the manifest declares them and
 * POM supports them. */
export function selectedLocales(plugin: ActivePluginUi, active: Locale): Locale[] {
  const declared = plugin.manifest.i18n ?? {};
  return [...new Set<Locale>(["en", active])].filter(
    (locale) => SUPPORTED_LOCALES.includes(locale) && locale in declared,
  );
}

const loadedFor = new Map<string, string>();

/**
 * Synchronizes the catalogs with the set of active plugins and the language. Plugins
 * that left have their texts removed; those that remain with the same version and
 * language are not downloaded again.
 */
export async function syncPluginCatalogs(
  plugins: readonly ActivePluginUi[],
  active: Locale,
  fetcher: CatalogFetcher = jsonFetcher,
): Promise<void> {
  const present = new Set(plugins.map((plugin) => plugin.code));
  for (const code of [...loadedFor.keys()]) {
    if (!present.has(code)) {
      clearPluginMessages(code);
      loadedFor.delete(code);
    }
  }
  for (const plugin of plugins) {
    const stamp = `${plugin.version ?? ""}|${active}`;
    if (loadedFor.get(plugin.code) === stamp) continue;
    const catalogs: Partial<Record<Locale, Record<string, string>>> = {};
    // An on-demand plugin serves its texts only while its process runs: a catalog that failed is retried on the next
    // sync (the plugin turning `running`), so the stamp is kept only when every selected catalog arrived.
    let complete = true;
    for (const locale of selectedLocales(plugin, active)) {
      const asset = plugin.manifest.i18n?.[locale];
      if (!asset) continue;
      try {
        const catalog = validateCatalog(plugin.code, await fetcher(pluginAssetUrl(plugin.code, asset)));
        if (catalog) catalogs[locale] = catalog;
        else {
          complete = false;
          console.warn(`The ${locale} catalog of plugin ${plugin.code} is invalid and was ignored`);
        }
      } catch (error) {
        complete = false;
        console.warn(`The ${locale} catalog of plugin ${plugin.code} is unavailable`, error);
      }
    }
    setPluginMessages(plugin.code, catalogs);
    if (complete) loadedFor.set(plugin.code, stamp);
    else loadedFor.delete(plugin.code);
  }
}

/** For tests only: forgets what was already loaded. */
export function resetPluginCatalogs(): void {
  for (const code of loadedFor.keys()) clearPluginMessages(code);
  loadedFor.clear();
}
