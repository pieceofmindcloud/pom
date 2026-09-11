import clsx from "clsx";
import { NavLink, useInRouterContext, useLocation } from "react-router-dom";
import { useI18n } from "../i18n/provider";

const CHAT_SURFACES = [
  { to: "/chat", labelKey: "chat.aiTab" },
  { to: "/network-chat", labelKey: "chat.networkTab" },
] as const;

function isSurfaceActive(pathname: string, to: string): boolean {
  return pathname === to || pathname === `/admin-ui${to}`;
}

/** Shared route tabs keep the two chat surfaces reachable in both directions. */
export function ChatSurfaceTabs({ className }: { className?: string }) {
  if (!useInRouterContext()) return null;
  return <ChatSurfaceTabsInRouter className={className} />;
}

function ChatSurfaceTabsInRouter({ className }: { className?: string }) {
  const { t } = useI18n();
  const { pathname } = useLocation();

  return (
    <nav
      className={clsx(
        "flex shrink-0 items-center gap-1 border-b border-line bg-surface/75 px-3 py-2 sm:px-4",
        className,
      )}
      role="tablist"
      aria-label={t("chat.surfaceLabel")}
    >
      {CHAT_SURFACES.map(({ to, labelKey }) => {
        const active = isSurfaceActive(pathname, to);
        return (
          <NavLink
            key={to}
            to={to}
            end
            role="tab"
            aria-selected={active}
            className={clsx(
              "rounded-md px-3 py-1.5 text-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris",
              active
                ? "bg-iris/15 text-fg ring-1 ring-inset ring-iris/30"
                : "text-muted hover:bg-line/40 hover:text-fg",
            )}
          >
            {t(labelKey)}
          </NavLink>
        );
      })}
    </nav>
  );
}
