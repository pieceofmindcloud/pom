/**
 * Catálogos de texto de plugin.
 *
 * Cada plugin instala os próprios textos junto com o backend e a interface: o
 * manifesto lista, por idioma, um asset JSON plano `{"<plugin_code>.chave":
 * "texto ICU"}`. Este módulo carrega os idiomas **selecionados** (o ativo e `en`
 * como piso, só os que a aplicação suporta), valida o formato de novo no
 * navegador e entrega ao mesmo `formatMessage` do POM. Catálogo ausente,
 * ilegível ou inválido é descartado com aviso: a tela cai no `en` ou na chave,
 * nunca quebra o shell.
 */
import { SUPPORTED_LOCALES, clearPluginMessages, setPluginMessages, type Locale } from "../i18n/locales";
import { pluginAssetUrl, type ActivePluginUi } from "./pluginUi";

export type CatalogFetcher = (url: string) => Promise<unknown>;

const jsonFetcher: CatalogFetcher = async (url) => {
  const response = await fetch(url, { credentials: "same-origin" });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
};

/** Textos aceitos: objeto plano, chaves em `<code>.`, valores string. */
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

/** Idiomas a carregar: o ativo e `en`, se o manifesto os declara e o POM os suporta. */
export function selectedLocales(plugin: ActivePluginUi, active: Locale): Locale[] {
  const declared = plugin.manifest.i18n ?? {};
  return [...new Set<Locale>(["en", active])].filter(
    (locale) => SUPPORTED_LOCALES.includes(locale) && locale in declared,
  );
}

const loadedFor = new Map<string, string>();

/**
 * Sincroniza os catálogos com o conjunto de plugins ativos e o idioma. Plugins
 * que saíram têm seus textos removidos; os que continuam com a mesma versão e
 * idioma não são baixados de novo.
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
    for (const locale of selectedLocales(plugin, active)) {
      const asset = plugin.manifest.i18n?.[locale];
      if (!asset) continue;
      try {
        const catalog = validateCatalog(plugin.code, await fetcher(pluginAssetUrl(plugin.code, asset)));
        if (catalog) catalogs[locale] = catalog;
        else console.warn(`Catálogo ${locale} do plugin ${plugin.code} é inválido e foi ignorado`);
      } catch (error) {
        console.warn(`Catálogo ${locale} do plugin ${plugin.code} indisponível`, error);
      }
    }
    setPluginMessages(plugin.code, catalogs);
    loadedFor.set(plugin.code, stamp);
  }
}

/** Só para testes: esquece o que já foi carregado. */
export function resetPluginCatalogs(): void {
  for (const code of loadedFor.keys()) clearPluginMessages(code);
  loadedFor.clear();
}
