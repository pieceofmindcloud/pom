import clsx from "clsx";
import type { ReactNode } from "react";
import { useI18n } from "../i18n/provider";
import { brand } from "../lib/brand";

const LANGUAGES = [
  { id: "en", label: "English" },
  { id: "pt-BR", label: "Português (Brasil)" },
] as const;

export function LanguageFlags({ className, children }: { className?: string; children?: ReactNode }) {
  const { locale, setLocale } = useI18n();
  const label = locale === "pt-BR" ? "Idioma" : "Language";

  return (
    <div
      role="group"
      aria-label={label}
      className={clsx("inline-flex items-center gap-1.5", className)}
    >
      {LANGUAGES.map((language) => (
        <button
          key={language.id}
          type="button"
          aria-pressed={locale === language.id}
          aria-label={language.label}
          title={language.label}
          onClick={() => setLocale(language.id)}
          className={clsx(
            "flex h-8 w-10 items-center justify-center rounded-md border transition-colors",
            locale === language.id
              ? "border-iris/50 bg-iris/12"
              : "border-line opacity-55 hover:opacity-100",
          )}
        >
          <img
            src={brand.flags[language.id]}
            alt=""
            aria-hidden="true"
            className="h-3.5 w-5 rounded-[2px] object-cover ring-1 ring-inset ring-black/20"
          />
        </button>
      ))}
      {children}
    </div>
  );
}
