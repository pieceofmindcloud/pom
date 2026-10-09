import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { ChevronDown, ChevronUp, Moon, Sun } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { NavLink, useLocation, useNavigate } from "react-router-dom";
import { LogoFull, LogoMark } from "./Logo";
import { ChatSessionList } from "./ChatSessionList";
import { navFooterForProfile, navForProfile, type NavSection } from "./nav";
import { useI18n } from "../i18n/provider";
import { useProfile } from "../lib/profile";
import { hasConversations, useOptionalChatSessionController } from "../lib/chatStore";
import { useLobbyUnread, useNetworkChatUnread } from "../lib/networkChatUnread";
import { useNode } from "../lib/queries";
import { useTheme } from "../lib/theme";
import { TlsChip } from "./TlsIndicator";
import { LanguageFlags } from "./LanguageFlags";

/**
 * Header of the bar. When it receives the desktop Sidebar's toggle, the logo
 * itself becomes the accessible control and the click toggles the state. In the mobile drawer
 * it stays a passive mark, since the Topbar controls the opening from outside.
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
        "h-16",
      )}
    >
      {onToggle && (
        <button
          type="button"
          aria-expanded={!collapsed}
          onClick={onToggle}
          aria-label={collapsed ? t("sidebar.expand") : t("sidebar.collapse")}
          title={collapsed ? t("sidebar.expand") : t("sidebar.collapse")}
          className={clsx(
            "flex shrink-0 items-center justify-center rounded-md focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris focus-visible:ring-offset-2 focus-visible:ring-offset-surface",
            collapsed ? "h-12 w-12" : "h-20 w-36",
          )}
        >
          {collapsed ? <LogoMark size={34} /> : <LogoFull width={104} variant="card" />}
        </button>
      )}
      {!onToggle && (collapsed ? <LogoMark size={34} /> : <LogoFull width={104} variant="band" />)}
    </div>
  );
}

function NavigationIcon({
  icon: Icon,
  image,
  className,
}: {
  icon: NavSection["icon"];
  image?: string;
  className: string;
}) {
  const [imageFailed, setImageFailed] = useState(false);
  if (!image || imageFailed) {
    return <Icon className={className} aria-hidden="true" />;
  }
  return (
    <img
      src={image}
      alt=""
      aria-hidden="true"
      onError={() => setImageFailed(true)}
      className={clsx(className, "object-contain")}
    />
  );
}

/** Quanto cada clique na seta rola, em pixels. */
const SCROLL_STEP = 120;

function ScrollEdgeButton({ direction, onClick }: { direction: "up" | "down"; onClick: () => void }) {
  const { t } = useI18n();
  const label = t(direction === "up" ? "sidebar.scrollUp" : "sidebar.scrollDown");
  const Chevron = direction === "up" ? ChevronUp : ChevronDown;
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      className="flex h-5 w-full shrink-0 items-center justify-center text-faint transition-colors hover:text-fg"
    >
      <Chevron className="h-3.5 w-3.5" aria-hidden="true" />
    </button>
  );
}

/**
 * Menu rolável: quando os itens não cabem na altura da janela, aparecem setas
 * no topo e no pé para rolar. Só são desenhadas as setas do lado em que ainda
 * há itens escondidos.
 */
export function ScrollableNav({ className, children }: { className?: string; children: ReactNode }) {
  const scrollerRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const [edges, setEdges] = useState({ up: false, down: false });

  const measure = useCallback(() => {
    const scroller = scrollerRef.current;
    if (!scroller) return;
    const up = scroller.scrollTop > 1;
    const down = scroller.scrollTop + scroller.clientHeight < scroller.scrollHeight - 1;
    setEdges((previous) => (previous.up === up && previous.down === down ? previous : { up, down }));
  }, []);

  useEffect(() => {
    const scroller = scrollerRef.current;
    const content = contentRef.current;
    if (!scroller || !content) return;
    measure();
    // A altura da janela muda o que cabe; a do conteúdo muda quando uma seção abre.
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(measure);
    observer?.observe(scroller);
    observer?.observe(content);
    window.addEventListener("resize", measure);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, [measure]);

  const scrollBy = (delta: number) => {
    scrollerRef.current?.scrollBy({ top: delta, behavior: "smooth" });
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {edges.up && <ScrollEdgeButton direction="up" onClick={() => scrollBy(-SCROLL_STEP)} />}
      <div
        ref={scrollerRef}
        onScroll={measure}
        className={clsx("min-h-0 flex-1 overflow-y-auto overflow-x-hidden overscroll-y-contain", className)}
      >
        <div ref={contentRef} className="flex flex-col gap-1">
          {children}
        </div>
      </div>
      {edges.down && <ScrollEdgeButton direction="down" onClick={() => scrollBy(SCROLL_STEP)} />}
    </div>
  );
}

/**
 * Language and theme, at the foot of the bar.
 *
 * The language is a flag, not "EN"/"PT-BR": the text label only helps whoever already
 * knows what the abbreviation means, and the target here is precisely whoever opened the screen
 * in the wrong language. The language name stays in the `aria-label` and in the `title`,
 * because a flag is not a language for a screen reader.
 */
function SidebarUtilityControls({ collapsed }: { collapsed: boolean }) {
  const { t } = useI18n();
  const { profile } = useProfile();
  const { theme, toggle } = useTheme();
  // This node's commit lives here, beside the controls: in a cluster of several
  // machines "which build is this?" cannot be answered without a shell, and a peer running
  // an old binary looks healthy until inference time. Without the word "build"
  // in front - it is a hash in mono font, and what it is stays in the tooltip.
  const { data: node } = useNode();
  const footerLinks = navFooterForProfile(profile);
  return (
    <div className={clsx("border-t border-line py-2.5", collapsed ? "px-2" : "px-3")}>
      {footerLinks.length > 0 && (
        <div className={clsx("mb-2 grid gap-1.5", collapsed ? "grid-cols-1" : "grid-cols-2")}>
          {footerLinks.map((item) => {
            const Icon = item.icon;
            return (
              <NavLink
                key={item.to}
                to={item.to}
                aria-label={t(item.labelKey)}
                title={t(item.labelKey)}
                className={({ isActive }) => clsx(
                  "flex h-8 min-w-0 items-center justify-center gap-1.5 rounded-md border px-2 text-[0.72rem] transition-colors",
                  isActive
                    ? "border-iris/50 bg-iris/12 text-fg"
                    : "border-line text-muted hover:border-line-strong hover:text-fg",
                )}
              >
                <Icon className="h-3.5 w-3.5 shrink-0" />
                {!collapsed && <span className="truncate">{t(item.labelKey)}</span>}
              </NavLink>
            );
          })}
        </div>
      )}
      <div className={clsx("flex items-center gap-1.5", collapsed ? "flex-col" : "flex-row")}>
        <LanguageFlags className={collapsed ? "flex-col" : "flex-row"}>
          <TlsChip />
        </LanguageFlags>
        {!collapsed && profile !== "infer" && node?.build_commit && (
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

function leafHoldsPath(leaf: NonNullable<NavSection["children"]>[number], pathname: string): boolean {
  return (leaf.end ? pathname === leaf.to : pathname === leaf.to || pathname.startsWith(`${leaf.to}/`))
    || (leaf.children ?? []).some((child) => leafHoldsPath(child, pathname));
}

/**
 * New messages in the lobby, where machines that are not members talk and ask
 * to join. Amber, beside the members' room count, so an administrator tells
 * the two apart at a glance.
 */
function LobbyBadge({ count, compact = false }: { count: number; compact?: boolean }) {
  const { t } = useI18n();
  if (count < 1) return null;
  const label = t("networkChat.lobbyUnread", { count });
  return (
    <span
      data-testid="network-chat-lobby-unread"
      aria-label={label}
      title={label}
      className={clsx(
        "shrink-0 rounded-full bg-amber px-1.5 text-center text-[0.65rem] font-semibold text-ink",
        compact ? "absolute -bottom-1 -right-1 min-w-4 leading-4" : "min-w-5 leading-5",
      )}
    >
      {count > 99 ? "99+" : count}
    </span>
  );
}

function UnreadBadge({ count, compact = false, inline = false }: { count: number; compact?: boolean; inline?: boolean }) {
  const { t } = useI18n();
  if (count < 1) return null;
  return (
    <span
      data-testid="network-chat-unread"
      aria-label={t("networkChat.unread", { count })}
      className={clsx(
        "shrink-0 rounded-full bg-iris px-1.5 text-center text-[0.65rem] font-semibold leading-5 text-ink",
        compact ? "absolute -right-1 -top-1 min-w-4 leading-4" : inline ? "min-w-5" : "ml-auto min-w-5",
      )}
    >
      {count > 99 ? "99+" : count}
    </span>
  );
}

function sectionHoldsPath(section: NavSection, pathname: string) {
  return (section.to ? pathname === section.to || pathname.startsWith(`${section.to}/`) : false)
    || (section.children ?? []).some((child) => leafHoldsPath(child, pathname));
}

function Leaf({
  to,
  labelKey,
  end,
  children,
  onNavigate,
}: {
  to: string;
  labelKey: string;
  end?: boolean;
  children?: NonNullable<NavSection["children"]>[number]["children"];
  onNavigate?: () => void;
}) {
  const { t } = useI18n();
  const linkEnd = end ?? Boolean(children?.length);
  return (
    <div>
      <NavLink to={to} end={linkEnd} onClick={onNavigate} data-guide-nav={to}>
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
      {children && children.length > 0 && (
        <div className="ml-3 border-l border-line pl-2">
          {children.map((child) => <Leaf key={child.to} {...child} onNavigate={onNavigate} />)}
        </div>
      )}
    </div>
  );
}

function FlyoutLeaf({
  to,
  labelKey,
  end,
  children,
  onNavigate,
}: {
  to: string;
  labelKey: string;
  end?: boolean;
  children?: NonNullable<NavSection["children"]>[number]["children"];
  onNavigate?: () => void;
}) {
  const { t } = useI18n();
  const linkEnd = end ?? Boolean(children?.length);
  return (
    <div>
      <NavLink to={to} end={linkEnd} onClick={onNavigate} data-guide-nav={to}>
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
      {children && children.length > 0 && <div className="ml-3 border-l border-line pl-2">{children.map((child) => <FlyoutLeaf key={child.to} {...child} onNavigate={onNavigate} />)}</div>}
    </div>
  );
}

function CollapsedSection({
  section,
  onNavigate,
  badge,
  lobbyBadge,
}: {
  section: NavSection;
  onNavigate?: () => void;
  badge?: number;
  lobbyBadge?: number;
}) {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const holdsPath = sectionHoldsPath(section, pathname);
  const label = t(section.labelKey);
  const Icon = section.icon;
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const flyoutRef = useRef<HTMLDivElement>(null);
  const [anchor, setAnchor] = useState<{ top: number; left: number } | null>(null);

  // O submenu é `fixed` e ancorado ao ícone, para não ser cortado pela área
  // rolável do menu (um `overflow` faria o mesmo com um `absolute`).
  const anchorFlyout = () => {
    const rect = triggerRef.current?.getBoundingClientRect();
    if (rect) setAnchor({ top: rect.top, left: rect.right });
  };

  useLayoutEffect(() => {
    const flyout = flyoutRef.current;
    if (!flyout || !anchor) return;
    // Desce a partir do ícone, mas não passa do pé da janela.
    const overflow = anchor.top + flyout.offsetHeight - (window.innerHeight - 8);
    flyout.style.top = `${Math.max(8, anchor.top - Math.max(0, overflow))}px`;
  }, [anchor]);

  if (section.to) {
    return (
      <NavLink to={section.to} end onClick={onNavigate} data-guide-nav={section.to} aria-label={label} title={label}>
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
            <NavigationIcon
              icon={Icon}
              image={section.iconImage}
              className={clsx("relative h-[1.05rem] w-[1.05rem]", isActive && "text-iris")}
            />
            <UnreadBadge count={badge ?? 0} compact />
            {/* Both on a collapsed icon: the lobby takes the other corner. */}
            <LobbyBadge count={lobbyBadge ?? 0} compact />
          </span>
        )}
      </NavLink>
    );
  }

  return (
    <div className="group relative" onMouseEnter={anchorFlyout} onFocus={anchorFlyout}>
      <button
        ref={triggerRef}
        type="button"
        aria-label={label}
        title={label}
        aria-haspopup="menu"
        aria-expanded={open}
        data-guide-section={section.labelKey}
        onClick={() => {
          anchorFlyout();
          setOpen((value) => !value);
        }}
        className={clsx(
          "flex h-10 w-full items-center justify-center rounded-lg transition-colors duration-150",
          holdsPath ? "bg-iris/12 text-iris ring-1 ring-inset ring-iris/25" : "text-muted hover:bg-line/40 hover:text-fg",
        )}
      >
        <NavigationIcon
          icon={Icon}
          image={section.iconImage}
          className="h-[1.05rem] w-[1.05rem]"
        />
      </button>
      <div
        ref={flyoutRef}
        style={anchor ? { top: anchor.top, left: anchor.left } : undefined}
        className={clsx(
          "pointer-events-none fixed z-50 pl-2 transition-opacity duration-150 group-hover:pointer-events-auto group-hover:opacity-100 group-focus-within:pointer-events-auto group-focus-within:opacity-100",
          open ? "pointer-events-auto opacity-100" : "opacity-0",
        )}
      >
        <div className="max-h-[calc(100vh-16px)] w-56 overflow-y-auto rounded-xl border border-line-strong bg-surface p-2 shadow-2xl shadow-black/30">
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
  badge,
  lobbyBadge,
}: {
  section: NavSection;
  onNavigate?: () => void;
  collapsed: boolean;
  badge?: number;
  /** New lobby messages, for an administrator: a second, amber badge. */
  lobbyBadge?: number;
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

  if (collapsed) return <CollapsedSection section={section} onNavigate={onNavigate} badge={badge} lobbyBadge={lobbyBadge} />;

  if (section.to) {
    return (
      <NavLink to={section.to} end onClick={onNavigate} data-guide-nav={section.to}>
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
            <NavigationIcon
              icon={Icon}
              image={section.iconImage}
              className={clsx("relative h-[1.05rem] w-[1.05rem] shrink-0", isActive && "text-iris")}
            />
            <span className="relative truncate">{t(section.labelKey)}</span>
            <span className="ml-auto flex shrink-0 items-center gap-1">
              <LobbyBadge count={lobbyBadge ?? 0} />
              <UnreadBadge count={badge ?? 0} inline />
            </span>
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
        data-guide-section={section.labelKey}
        className={clsx(
          "flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm font-medium",
          "transition-colors duration-150",
          holdsPath ? "text-fg" : "text-muted hover:bg-line/40 hover:text-fg",
        )}
      >
        <NavigationIcon
          icon={Icon}
          image={section.iconImage}
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

function InferenceMainMenu({
  onNavigate,
  collapsed,
  unreadCount,
}: {
  onNavigate?: () => void;
  collapsed: boolean;
  unreadCount: number;
}) {
  const { session, sessions, selectSession, startNew, deleteSession } =
    useOptionalChatSessionController() ?? {
      session: null,
      sessions: [],
      selectSession: () => {},
      startNew: () => {},
      deleteSession: () => {},
    };
  const { profile } = useProfile();
  const sections = navForProfile(profile);
  const navigate = useNavigate();
  const { pathname } = useLocation();

  /*
   * This list is the `infer` profile's main menu, and it is seen from every route -
   * from the documentation, from the settings. Changing the active conversation is not
   * navigating: outside `/chat` the click changed the shared state and the screen
   * stayed the same, as if the menu had stopped responding. Whoever is already
   * in the chat is not sent again, so as not to stack identical entries in the
   * browser history.
   */
  const openChat = () => {
    if (pathname !== "/chat") navigate("/chat");
  };

  return (
    <div
      className={clsx(
        "flex min-h-0 flex-1 flex-col pb-4 pt-[5px]",
        collapsed ? "overflow-visible px-2" : "px-3",
      )}
    >
      {/* No conversation yet: the menu is only the network and the rest. */}
      {hasConversations(sessions) && <ChatSessionList
        sessions={sessions}
        activeId={session?.id ?? ""}
        compact
        collapsed={collapsed}
        onPick={(next) => {
          selectSession(next);
          openChat();
          onNavigate?.();
        }}
        onNew={() => {
          startNew(session?.model ?? null);
          openChat();
          onNavigate?.();
        }}
        onDelete={(id) => deleteSession(id, session?.model ?? null)}
      />}
      <div className="mt-2 shrink-0 space-y-1">
        {sections.map((section) => (
          <Section
            key={section.labelKey}
            section={section}
            onNavigate={onNavigate}
            collapsed={collapsed}
            badge={section.to === "/network-chat" ? unreadCount : undefined}
          />
        ))}
      </div>
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
  // The menu follows the profile: what this profile does not open is not offered either.
  // See `navForProfile` and the table in `lib/access.ts`.
  const { profile } = useProfile();
  const { unreadCount, snapshot } = useNetworkChatUnread(false);
  const { data: node } = useNode();
  // Only an administrator answers the lobby, and only while the network has it on.
  const lobbyUnread = useLobbyUnread(Boolean(node?.authority_level) && snapshot?.lobbyEnabled === true);
  return (
    <>
      <SidebarBrand collapsed={collapsed} onToggle={onToggle} />
      {profile === "infer" ? (
        <InferenceMainMenu onNavigate={onNavigate} collapsed={collapsed} unreadCount={unreadCount} />
      ) : (
        <nav className="flex min-h-0 flex-1 flex-col">
          <ScrollableNav className={clsx("pb-4 pt-[5px]", collapsed ? "px-2" : "px-3")}>
            {navForProfile(profile).map((s) => (
              <Section
                key={s.labelKey}
                section={s}
                onNavigate={onNavigate}
                collapsed={collapsed}
                badge={s.to === "/network-chat" ? unreadCount : undefined}
                lobbyBadge={s.to === "/network-chat" ? lobbyUnread : undefined}
              />
            ))}
          </ScrollableNav>
        </nav>
      )}
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
