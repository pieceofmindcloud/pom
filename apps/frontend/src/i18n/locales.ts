import IntlMessageFormat from "intl-messageformat";
import messagesCsv from "./messages.csv?raw";

export const SUPPORTED_LOCALES = ["en", "pt-BR"] as const;

export type Locale = (typeof SUPPORTED_LOCALES)[number];

export type MessageValues = Record<string, string | number>;

let activeLocale: Locale = "en";

export function getActiveLocale(): Locale {
  return activeLocale;
}

export function setActiveLocale(locale: Locale) {
  activeLocale = locale;
}

type ParsedCsv = string[][];
type LocaleMessages = Map<string, string>;

function parseCsv(csv: string): ParsedCsv {
  const rows: ParsedCsv = [];
  let row: string[] = [];
  let field = "";
  let quoted = false;

  for (let index = 0; index < csv.length; index += 1) {
    const character = csv[index];
    if (quoted) {
      if (character === '"') {
        if (csv[index + 1] === '"') {
          field += '"';
          index += 1;
        } else {
          quoted = false;
        }
      } else {
        field += character;
      }
      continue;
    }

    if (character === '"' && field.length === 0) {
      quoted = true;
    } else if (character === ",") {
      row.push(field);
      field = "";
    } else if (character === "\n") {
      row.push(field);
      rows.push(row);
      row = [];
      field = "";
    } else if (character !== "\r") {
      field += character;
    }
  }

  if (quoted) throw new Error("Unclosed quoted field in locale CSV");
  if (field.length > 0 || row.length > 0) {
    row.push(field);
    rows.push(row);
  }
  return rows;
}

function loadMessages(csv: string): Record<Locale, LocaleMessages> {
  const rows = parseCsv(csv);
  const header = rows.shift();
  if (!header || header.join(",") !== "key,en,pt-BR") {
    throw new Error("Locale CSV must have the header key,en,pt-BR");
  }

  const messages: Record<Locale, LocaleMessages> = {
    en: new Map(),
    "pt-BR": new Map(),
  };
  for (const row of rows) {
    if (row.length !== header.length || !row[0]) throw new Error("Malformed locale CSV row");
    messages.en.set(row[0], row[1]);
    messages["pt-BR"].set(row[0], row[2]);
  }
  return messages;
}

const messages = loadMessages(messagesCsv);
const messageFormatters = new Map<string, IntlMessageFormat>();
const truthySources = new Set<string>();
const nonZeroSources = new Set<string>();

for (const locale of SUPPORTED_LOCALES) {
  for (const message of messages[locale].values()) {
    for (const match of message.matchAll(/\{(__truthy_([A-Za-z_$][\w$]*)), select,/g)) {
      truthySources.add(match[2]);
    }
    for (const match of message.matchAll(/\{(__notZero_([A-Za-z_$][\w$]*)), select,/g)) {
      nonZeroSources.add(match[2]);
    }
  }
}

function formatter(locale: Locale, key: string, message: string): IntlMessageFormat {
  const cacheKey = `${locale}:${key}`;
  const cached = messageFormatters.get(cacheKey);
  if (cached) return cached;
  const created = new IntlMessageFormat(message, locale);
  messageFormatters.set(cacheKey, created);
  return created;
}

function valuesForIcu(values: MessageValues): MessageValues {
  const result: MessageValues = { ...values };
  for (const name of truthySources) {
    result[`__truthy_${name}`] = values[name] ? "true" : "false";
  }
  for (const name of nonZeroSources) {
    result[`__notZero_${name}`] = String(values[name]) !== "0" ? "true" : "false";
  }
  return result;
}

export function isLocale(value: string | null | undefined): value is Locale {
  return value === "en" || value === "pt-BR";
}

export function resolveLocale(
  savedLocale: string | null | undefined,
  browserLocales: readonly string[] | null | undefined,
): Locale {
  if (isLocale(savedLocale)) return savedLocale;

  for (const browserLocale of browserLocales ?? []) {
    const normalized = browserLocale.toLowerCase();
    if (normalized === "pt-br" || normalized === "pt" || normalized.startsWith("pt-")) {
      return "pt-BR";
    }
    if (normalized === "en" || normalized.startsWith("en-")) return "en";
  }

  return "en";
}

/** Every key in the CSV, kept sorted for stable callers and diffs. */
/*
 * Texts that belong to plugins. Each plugin carries its own catalog
 * (`lib/pluginI18n.ts` loads and validates it); here they only enter the same map that
 * `formatMessage` already consults, under the key `<plugin_code>.*`, and leave when the
 * plugin leaves. A key that already exists in the POM catalog is never overwritten.
 */
const pluginKeys = new Map<string, Set<string>>();
const pluginListeners = new Set<() => void>();
let pluginVersion = 0;

function notifyPluginMessages() {
  pluginVersion += 1;
  for (const listener of pluginListeners) listener();
}

export function subscribePluginMessages(listener: () => void): () => void {
  pluginListeners.add(listener);
  return () => pluginListeners.delete(listener);
}

export const pluginMessagesVersion = () => pluginVersion;

/** Replaces all the texts of this plugin in the given languages. */
export function setPluginMessages(code: string, catalogs: Partial<Record<Locale, Record<string, string>>>): void {
  clearPluginMessages(code, false);
  const owned = new Set<string>();
  for (const locale of SUPPORTED_LOCALES) {
    for (const [key, text] of Object.entries(catalogs[locale] ?? {})) {
      const reserved = messages[locale].has(key) && !owned.has(`${locale}:${key}`);
      if (reserved) continue;
      messages[locale].set(key, text);
      owned.add(`${locale}:${key}`);
      messageFormatters.delete(`${locale}:${key}`);
      for (const match of text.matchAll(/\{(__truthy_([A-Za-z_$][\w$]*)), select,/g)) truthySources.add(match[2]);
      for (const match of text.matchAll(/\{(__notZero_([A-Za-z_$][\w$]*)), select,/g)) nonZeroSources.add(match[2]);
    }
  }
  pluginKeys.set(code, owned);
  notifyPluginMessages();
}

export function clearPluginMessages(code: string, notify = true): void {
  for (const entry of pluginKeys.get(code) ?? []) {
    const [locale, ...rest] = entry.split(":");
    const key = rest.join(":");
    messages[locale as Locale].delete(key);
    messageFormatters.delete(`${locale}:${key}`);
  }
  pluginKeys.delete(code);
  if (notify) notifyPluginMessages();
}

export function messageKeys(locale: Locale): string[] {
  return [...messages[locale].keys()].sort();
}

export function formatMessage(locale: Locale, key: string, values: MessageValues = {}): string {
  const sourceLocale = messages[locale].has(key) ? locale : "en";
  const message = messages[sourceLocale].get(key);
  if (!message) return key;
  try {
    return String(formatter(sourceLocale, key, message).format(valuesForIcu(values)));
  } catch (error) {
    console.warn(`Could not format locale message "${key}"`, error);
    return message;
  }
}
