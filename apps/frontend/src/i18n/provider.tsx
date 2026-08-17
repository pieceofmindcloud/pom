import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { type Locale, formatMessage, resolveLocale, setActiveLocale } from "./locales";
import { storageKey } from "../lib/brand";

const STORAGE_KEY = storageKey("locale");

type I18nContextValue = {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  t: (key: string, values?: Record<string, string | number>) => string;
};

const I18nContext = createContext<I18nContextValue>({
  locale: "en",
  setLocale: () => {},
  t: (key) => key,
});

function browserLocale(): Locale {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY);
    return resolveLocale(saved, navigator.languages ?? [navigator.language]);
  } catch {
    return "en";
  }
}

export function LocaleProvider({
  children,
  initialLocale,
}: {
  children: ReactNode;
  initialLocale?: Locale;
}) {
  const [locale, setLocaleState] = useState<Locale>(() => initialLocale ?? browserLocale());
  setActiveLocale(locale);

  useEffect(() => {
    document.documentElement.lang = locale;
  }, [locale]);

  const setLocale = useCallback((nextLocale: Locale) => {
    setLocaleState(nextLocale);
    try {
      window.localStorage.setItem(STORAGE_KEY, nextLocale);
    } catch {
      // The chosen language still applies when storage is blocked.
    }
  }, []);

  const value = useMemo(
    () => ({
      locale,
      setLocale,
      t: (key: string, values: Record<string, string | number> = {}) =>
        formatMessage(locale, key, values),
    }),
    [locale, setLocale],
  );

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export const useI18n = () => useContext(I18nContext);

export function LanguageSelect({ className }: { className?: string }) {
  const { locale, setLocale } = useI18n();
  const label = locale === "pt-BR" ? "Idioma" : "Language";

  return (
    <label className={className}>
      <span className="sr-only">{label}</span>
      <select
        aria-label={label}
        value={locale}
        onChange={(event) => setLocale(event.target.value as Locale)}
        className="h-9 rounded-lg border border-line bg-raised px-2 text-[0.82rem] text-muted transition-colors hover:border-line-strong hover:text-fg"
      >
        <option value="en">English</option>
        <option value="pt-BR">Português (Brasil)</option>
      </select>
    </label>
  );
}
