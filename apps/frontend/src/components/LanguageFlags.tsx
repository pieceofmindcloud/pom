import clsx from "clsx";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { useI18n } from "../i18n/provider";
import { brand } from "../lib/brand";

const LANGUAGES = [
  { id: "en", label: "English" },
  { id: "pt-BR", label: "Português (Brasil)" },
] as const;

/**
 * Estado de um menu de idioma que abre no clique: fecha num clique fora ou no
 * Escape, devolvendo o foco ao gatilho.
 */
export function useLanguageMenu() {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      triggerRef.current?.focus();
    };
    document.addEventListener("pointerdown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", close);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);

  return { open, setOpen, rootRef, triggerRef };
}

/**
 * Mostra apenas a bandeira do idioma atual; um clique abre o submenu com as
 * opções, e escolher uma troca o idioma e fecha o submenu.
 */
export function LanguageFlags({ className, children }: { className?: string; children?: ReactNode }) {
  const { locale, setLocale } = useI18n();
  const { open, setOpen, rootRef, triggerRef } = useLanguageMenu();
  const current = LANGUAGES.find((language) => language.id === locale) ?? LANGUAGES[0];
  const label = locale === "pt-BR" ? "Idioma" : "Language";

  const choose = (id: (typeof LANGUAGES)[number]["id"]) => {
    setLocale(id);
    setOpen(false);
    triggerRef.current?.focus();
  };

  return (
    <div ref={rootRef} className={clsx("relative inline-flex items-center gap-1.5", className)}>
      <button
        ref={triggerRef}
        type="button"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={`${label}: ${current.label}`}
        title={`${label}: ${current.label}`}
        onClick={() => setOpen((value) => !value)}
        className="flex h-8 w-10 items-center justify-center rounded-md border border-line transition-colors hover:border-line-strong"
      >
        <img
          src={brand.flags[current.id]}
          alt=""
          aria-hidden="true"
          className="h-3.5 w-5 rounded-[2px] object-cover ring-1 ring-inset ring-black/20"
        />
      </button>
      {open && (
        <div
          role="listbox"
          aria-label={label}
          className="absolute bottom-full left-0 z-50 mb-2 flex min-w-[11rem] flex-col gap-0.5 rounded-md border border-line bg-surface p-1 shadow-panel"
        >
          {LANGUAGES.map((language) => (
            <button
              key={language.id}
              type="button"
              role="option"
              aria-selected={locale === language.id}
              onClick={() => choose(language.id)}
              className={clsx(
                "flex items-center gap-2 rounded px-2 py-1.5 text-left text-xs transition-colors",
                locale === language.id ? "bg-iris/12 text-fg" : "text-muted hover:bg-line hover:text-fg",
              )}
            >
              <img
                src={brand.flags[language.id]}
                alt=""
                aria-hidden="true"
                className="h-3.5 w-5 shrink-0 rounded-[2px] object-cover ring-1 ring-inset ring-black/20"
              />
              <span>{language.label}</span>
            </button>
          ))}
        </div>
      )}
      {children}
    </div>
  );
}
