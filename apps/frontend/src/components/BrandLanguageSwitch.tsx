import { useI18n } from "../i18n/provider";
import { brand } from "../lib/brand";

export const BRAND_LANGUAGES = [
  { id: "pt-BR", short: "PT", label: "Português (Brasil)" },
  { id: "en", short: "EN", label: "English" },
] as const;

/** O seletor de idioma em pílula da página inicial do site, usado no portão e no wizard. */
export function BrandLanguageSwitch() {
  const { locale, setLocale } = useI18n();
  return (
    <div className="pom-login-langs" role="group" aria-label={locale === "pt-BR" ? "Idioma" : "Language"}>
      {BRAND_LANGUAGES.map((language) => (
        <button
          key={language.id}
          type="button"
          className="pom-login-lang"
          aria-pressed={locale === language.id}
          aria-label={language.label}
          title={language.label}
          onClick={() => setLocale(language.id)}
        >
          <img src={brand.flags[language.id]} alt="" width={17} height={12} />
          <span>{language.short}</span>
        </button>
      ))}
    </div>
  );
}
