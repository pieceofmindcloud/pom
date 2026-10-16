import { useI18n } from "../i18n/provider";
import { brand } from "../lib/brand";
import { useLanguageMenu } from "./LanguageFlags";

export const BRAND_LANGUAGES = [
  { id: "pt-BR", short: "PT", label: "Português (Brasil)" },
  { id: "en", short: "EN", label: "English" },
] as const;

/**
 * The language selector of the site's home page, used in the gate and the wizard.
 * It shows only the flag and the code of the current language; a click opens the
 * submenu with the options.
 */
export function BrandLanguageSwitch() {
  const { locale, setLocale } = useI18n();
  const { open, setOpen, rootRef, triggerRef } = useLanguageMenu();
  const current = BRAND_LANGUAGES.find((language) => language.id === locale) ?? BRAND_LANGUAGES[0];
  const label = locale === "pt-BR" ? "Idioma" : "Language";

  const choose = (id: (typeof BRAND_LANGUAGES)[number]["id"]) => {
    setLocale(id);
    setOpen(false);
    triggerRef.current?.focus();
  };

  return (
    <div ref={rootRef} className="pom-login-langs">
      <button
        ref={triggerRef}
        type="button"
        className="pom-login-lang"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={`${label}: ${current.label}`}
        title={`${label}: ${current.label}`}
        onClick={() => setOpen((value) => !value)}
      >
        <img src={brand.flags[current.id]} alt="" width={17} height={12} />
        <span>{current.short}</span>
      </button>
      {open && (
        <div role="listbox" aria-label={label} className="pom-login-lang-menu">
          {BRAND_LANGUAGES.map((language) => (
            <button
              key={language.id}
              type="button"
              role="option"
              className="pom-login-lang-option"
              aria-selected={locale === language.id}
              onClick={() => choose(language.id)}
            >
              <img src={brand.flags[language.id]} alt="" width={17} height={12} />
              <span>{language.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
