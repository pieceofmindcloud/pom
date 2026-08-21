import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { ChevronDown, Moon, PanelLeftClose, PanelLeftOpen, Sun } from "lucide-react";
import { useEffect, useState } from "react";
import { NavLink, useLocation } from "react-router-dom";
import { LogoFull, LogoMark } from "./Logo";
import { brand } from "../lib/brand";
import { navForProfile, type NavSection } from "./nav";
import { useI18n } from "../i18n/provider";
import { useProfile } from "../lib/profile";
import { useNode } from "../lib/queries";
import { useTheme } from "../lib/theme";

/**
 * O cabeçalho da barra: só a marca, ocupando a faixa inteira.
 *
 * A arte precisa de fundo claro para o slogan não sumir no tema escuro (ver
 * `LogoFull`), e um cartão arredondado dentro de uma coluna já delimitada era
 * caixa dentro de caixa. A faixa vai de borda a borda e a linha de baixo faz o
 * mesmo papel que o cartão fazia: separar a marca do menu.
 */
function SidebarBrand({
  collapsed,
  onToggle,
}: {
  collapsed: boolean;
  onToggle?: () => void;
}) {
  const { t } = useI18n();
  return (
    <div
      className={clsx(
        "relative flex shrink-0 items-center justify-center border-b border-line transition-[height] duration-200",
        collapsed ? "h-[68px]" : "h-[92px]",
      )}
    >
      {collapsed ? <LogoMark size={34} /> : <LogoFull width={104} variant="band" />}
      {onToggle && (
        <button
          type="button"
          onClick={onToggle}
          aria-label={collapsed ? t("sidebar.expand") : t("sidebar.collapse")}
          title={collapsed ? t("sidebar.expand") : t("sidebar.collapse")}
          className={clsx(
            "absolute flex h-7 w-7 items-center justify-center rounded-md border border-line bg-surface text-muted transition-colors hover:border-line-strong hover:text-fg",
            collapsed ? "bottom-1.5 right-1/2 translate-x-1/2" : "right-2 top-2",
          )}
        >
          {collapsed ? <PanelLeftOpen className="h-3.5 w-3.5" /> : <PanelLeftClose className="h-3.5 w-3.5" />}
        </button>
      )}
    </div>
  );
}

/**
 * Idioma e tema, no pé da barra.
 *
 * O idioma é uma bandeira, não "EN"/"PT-BR": o rótulo em texto só ajuda quem já
 * sabe o que a sigla quer dizer, e o alvo aqui é justamente quem abriu a tela
 * no idioma errado. O nome do idioma continua no `aria-label` e no `title`,
 * porque bandeira não é idioma para um leitor de tela.
 */
function SidebarUtilityControls({ collapsed }: { collapsed: boolean }) {
  const { locale, setLocale, t } = useI18n();
  const { theme, toggle } = useTheme();
  // O commit deste nó vive aqui, ao lado dos controles: num cluster de várias
  // máquinas "qual build é esse?" não se responde sem shell, e um peer rodando
  // binário velho parece sadio até a hora da inferência. Sem a palavra "build"
  // na frente - é um hash em fonte mono, e o que ele é fica no tooltip.
  const { data: node } = useNode();
  const languages = [
    { id: "en", label: "English" },
    { id: "pt-BR", label: "Português (Brasil)" },
  ] as const;
  return (
    <div className={clsx("border-t border-line py-2.5", collapsed ? "px-2" : "px-3")}>
      <div
        role="group"
        aria-label={locale === "pt-BR" ? "Idioma" : "Language"}
        className={clsx(
          "flex items-center gap-1.5",
          collapsed ? "flex-col" : "flex-row",
        )}
      >
        {languages.map((lang) => (
          <button
            key={lang.id}
            type="button"
            aria-pressed={locale === lang.id}
            aria-label={lang.label}
            title={lang.label}
            onClick={() => setLocale(lang.id)}
            className={clsx(
              "flex h-8 items-center justify-center rounded-md border transition-colors",
              collapsed ? "w-8" : "w-10",
              locale === lang.id
                ? "border-iris/50 bg-iris/12"
                : "border-line opacity-55 hover:opacity-100",
            )}
          >
            <img
              src={brand.flags[lang.id]}
              alt=""
              aria-hidden="true"
              className="h-3.5 w-5 rounded-[2px] object-cover ring-1 ring-inset ring-black/20"
            />
          </button>
        ))}
        {!collapsed && node?.build_commit && (
          <span
            title={t("sidebar.buildTitle")}
            className="min-w-0 flex-1 truncate px-1 text-center font-mono text-[0.68rem] text-muted"
          >
            {node.build_commit}
          </span>
        )}
        <button
          type="button"
          onClick={toggle}
          aria-label={theme === "dark" ? t("topbar.useLightTheme") : t("topbar.useDarkTheme")}
          className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md border border-line text-muted hover:text-fg"
        >
          {theme === "dark" ? <Moon className="h-3.5 w-3.5" /> : <Sun className="h-3.5 w-3.5" />}
        </button>
      </div>
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

function FlyoutLeaf({
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
            "block rounded-lg px-2.5 py-2 text-[0.83rem] transition-colors",
            isActive ? "bg-iris/12 text-fg ring-1 ring-inset ring-iris/25" : "text-muted hover:bg-line/40 hover:text-fg",
          )}
        >
          {t(labelKey)}
        </span>
      )}
    </NavLink>
  );
}

function CollapsedSection({
  section,
  onNavigate,
}: {
  section: NavSection;
  onNavigate?: () => void;
}) {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const holdsPath = sectionHoldsPath(section, pathname);
  const label = t(section.labelKey);
  const Icon = section.icon;

  if (section.to) {
    return (
      <NavLink to={section.to} end onClick={onNavigate} aria-label={label} title={label}>
        {({ isActive }) => (
          <span
            className={clsx(
              "relative flex h-10 items-center justify-center rounded-lg transition-colors duration-150",
              isActive ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
            )}
          >
            {isActive && (
              <motion.span
                layoutId="nav-collapsed-section"
                className="absolute inset-0 rounded-lg bg-iris/12 ring-1 ring-inset ring-iris/25"
                transition={{ type: "spring", stiffness: 520, damping: 42 }}
              />
            )}
            <Icon className={clsx("relative h-[1.05rem] w-[1.05rem]", isActive && "text-iris")} />
          </span>
        )}
      </NavLink>
    );
  }

  return (
    <div className="group relative">
      <button
        type="button"
        aria-label={label}
        title={label}
        aria-haspopup="menu"
        className={clsx(
          "flex h-10 w-full items-center justify-center rounded-lg transition-colors duration-150",
          holdsPath ? "bg-iris/12 text-iris ring-1 ring-inset ring-iris/25" : "text-muted hover:bg-line/40 hover:text-fg",
        )}
      >
        <Icon className="h-[1.05rem] w-[1.05rem]" />
      </button>
      <div className="pointer-events-none absolute left-full top-0 z-50 pl-2 opacity-0 transition-opacity duration-150 group-hover:pointer-events-auto group-hover:opacity-100 group-focus-within:pointer-events-auto group-focus-within:opacity-100">
        <div className="w-56 rounded-xl border border-line-strong bg-surface p-2 shadow-2xl shadow-black/30">
          <p className="px-2.5 py-2 font-mono text-[0.68rem] uppercase tracking-[0.16em] text-faint">
            {label}
          </p>
          <div className="space-y-0.5">
            {section.children?.map((child) => (
              <FlyoutLeaf key={child.to} {...child} onNavigate={onNavigate} />
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}

function Section({
  section,
  onNavigate,
  collapsed,
}: {
  section: NavSection;
  onNavigate?: () => void;
  collapsed: boolean;
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

  if (collapsed) return <CollapsedSection section={section} onNavigate={onNavigate} />;

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

export function SidebarContent({
  onNavigate,
  collapsed = false,
  onToggle,
}: {
  onNavigate?: () => void;
  collapsed?: boolean;
  onToggle?: () => void;
}) {
  // O menu segue o perfil: o que este perfil não abre também não é oferecido.
  // Ver `navForProfile` e a tabela em `lib/access.ts`.
  const { profile } = useProfile();
  const sections = navForProfile(profile);
  return (
    <>
      <SidebarBrand collapsed={collapsed} onToggle={onToggle} />
      {/* O respiro do topo é o que separa o primeiro item da linha divisória:
          sem ele a pílula ativa encosta na borda da faixa da marca. */}
      <nav
        className={clsx(
          "flex min-h-0 flex-1 flex-col gap-1 pb-4 pt-[5px]",
          collapsed ? "overflow-visible px-2" : "overflow-y-auto px-3",
        )}
      >
        {sections.map((s) => (
          <Section key={s.labelKey} section={s} onNavigate={onNavigate} collapsed={collapsed} />
        ))}
      </nav>
      <SidebarUtilityControls collapsed={collapsed} />
    </>
  );
}

export function Sidebar() {
  const [collapsed, setCollapsed] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.localStorage.getItem("pom.sidebar.collapsed") === "true";
  });

  useEffect(() => {
    window.localStorage.setItem("pom.sidebar.collapsed", String(collapsed));
  }, [collapsed]);

  return (
    <aside
      className={clsx(
        "relative hidden shrink-0 flex-col border-r border-line bg-surface transition-[width] duration-200 lg:flex",
        collapsed ? "w-[68px]" : "w-[248px]",
      )}
    >
      <SidebarContent collapsed={collapsed} onToggle={() => setCollapsed((value) => !value)} />
    </aside>
  );
}
