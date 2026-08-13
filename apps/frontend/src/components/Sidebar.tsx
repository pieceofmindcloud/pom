import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { ChevronDown, Moon, Sun } from "lucide-react";
import { useState } from "react";
import { NavLink, useLocation } from "react-router-dom";
import { Logo } from "./Logo";
import { NAV, type NavSection } from "./nav";
import { useI18n } from "../i18n/provider";
import { useNode } from "../lib/queries";
import { useTheme } from "../lib/theme";

function SidebarUtilityControls() {
  const { locale, setLocale, t } = useI18n();
  const { theme, toggle } = useTheme();
  // O commit deste nó, sempre visível: num cluster de várias máquinas, "qual
  // build é esse?" não se responde sem shell na máquina, e um peer rodando
  // binário velho parece sadio até a hora da inferência.
  const { data: node } = useNode();
  return (
    <div className="border-t border-line p-3">
      <div role="group" aria-label={locale === "pt-BR" ? "Idioma" : "Language"} className="flex gap-1">
        {(["en", "pt-BR"] as const).map((next) => (
          <button
            key={next}
            type="button"
            aria-pressed={locale === next}
            onClick={() => setLocale(next)}
            className={clsx("min-h-11 flex-1 rounded-lg border px-2 text-xs font-semibold", locale === next ? "border-iris/50 bg-iris/12 text-fg" : "border-line text-muted hover:text-fg")}
          >
            {next === "en" ? "EN" : "PT-BR"}
          </button>
        ))}
        <button type="button" onClick={toggle} aria-label={theme === "dark" ? t("topbar.useLightTheme") : t("topbar.useDarkTheme")} className="flex min-h-11 w-11 items-center justify-center rounded-lg border border-line text-muted hover:text-fg">
          {theme === "dark" ? <Moon className="h-4 w-4" /> : <Sun className="h-4 w-4" />}
        </button>
      </div>
      {node?.build_commit && (
        <p
          title={t("sidebar.buildTitle")}
          className="mt-2 truncate px-1 font-mono text-[0.62rem] text-faint"
        >
          {t("sidebar.build")} {node.build_commit}
        </p>
      )}
    </div>
  );
}

function sectionHoldsPath(section: NavSection, pathname: string) {
  return (section.children ?? []).some((c) =>
    c.end ? pathname === c.to : pathname.startsWith(c.to),
  );
}

function Leaf({
  to,
  labelKey,
  end,
  onNavigate,
}: {
  to: string;
  labelKey: string;
  end?: boolean;
  onNavigate?: () => void;
}) {
  const { t } = useI18n();
  return (
    <NavLink to={to} end={end} onClick={onNavigate}>
      {({ isActive }) => (
        <span
          className={clsx(
            "relative flex items-center rounded-lg py-1.5 pl-3 pr-2.5 text-[0.83rem]",
            "transition-colors duration-150",
            isActive ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
          )}
        >
          {isActive && (
            <motion.span
              layoutId="nav-leaf"
              className="absolute inset-0 rounded-lg bg-iris/12 ring-1 ring-inset ring-iris/25"
              transition={{ type: "spring", stiffness: 520, damping: 42 }}
            />
          )}
          <span className="relative truncate">{t(labelKey)}</span>
        </span>
      )}
    </NavLink>
  );
}

function Section({
  section,
  onNavigate,
}: {
  section: NavSection;
  onNavigate?: () => void;
}) {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const holdsPath = sectionHoldsPath(section, pathname);
  // A section the operator opened stays open; one holding the current page
  // opens itself. Collapsing the section you are inside of would hide where
  // you are.
  const [open, setOpen] = useState(holdsPath);
  const expanded = open || holdsPath;
  const Icon = section.icon;

  if (section.to) {
    return (
      <NavLink to={section.to} end onClick={onNavigate}>
        {({ isActive }) => (
          <span
            className={clsx(
              "relative flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm font-medium",
              "transition-colors duration-150",
              isActive ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
            )}
          >
            {isActive && (
              <motion.span
                layoutId="nav-section"
                className="absolute inset-0 rounded-lg bg-iris/12 ring-1 ring-inset ring-iris/25"
                transition={{ type: "spring", stiffness: 520, damping: 42 }}
              />
            )}
            <Icon
              className={clsx("relative h-[1.05rem] w-[1.05rem] shrink-0", isActive && "text-iris")}
            />
            <span className="relative truncate">{t(section.labelKey)}</span>
          </span>
        )}
      </NavLink>
    );
  }

  return (
    <div>
      <button
        onClick={() => setOpen((o) => !o)}
        aria-expanded={expanded}
        className={clsx(
          "flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm font-medium",
          "transition-colors duration-150",
          holdsPath ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
        )}
      >
        <Icon
          className={clsx("h-[1.05rem] w-[1.05rem] shrink-0", holdsPath && "text-iris")}
        />
        <span className="flex-1 truncate text-left">{t(section.labelKey)}</span>
        <ChevronDown
          className={clsx(
            "h-3.5 w-3.5 shrink-0 text-faint transition-transform duration-200 ease-swift",
            expanded && "rotate-180",
          )}
        />
      </button>
      <AnimatePresence initial={false}>
        {expanded && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
            className="overflow-hidden"
          >
            {/* The rail makes the children read as belonging to the section. */}
            <div className="ml-[1.32rem] mt-0.5 flex flex-col gap-0.5 border-l border-line pl-2">
              {section.children?.map((c) => (
                <Leaf key={c.to} {...c} onNavigate={onNavigate} />
              ))}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

export function SidebarContent({ onNavigate }: { onNavigate?: () => void }) {
  return (
    <>
      <div className="flex h-16 shrink-0 items-center px-5">
        <Logo size={30} />
      </div>
      <nav className="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto px-3 pb-4">
        {NAV.map((s) => (
          <Section key={s.labelKey} section={s} onNavigate={onNavigate} />
        ))}
      </nav>
      <SidebarUtilityControls />
    </>
  );
}

export function Sidebar() {
  return (
    <aside className="hidden w-[248px] shrink-0 flex-col border-r border-line bg-surface lg:flex">
      <SidebarContent />
    </aside>
  );
}
