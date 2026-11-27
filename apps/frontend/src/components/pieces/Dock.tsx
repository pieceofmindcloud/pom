/**
 * The POM's dock: the open apps with a screen of their own, floating over the
 * bottom of every screen (absolute in the shell's content column, so it
 * centres on the screens and not on the window with the menu), like the macOS Dock. The apps that cannot be closed
 * come first, then a divider and the closable ones. A click goes to the app's
 * screen, a hover gear opens its settings, a hover X closes it, a right click
 * opens the system menu (`PiecesProvider`). On Pieces it always shows; on the
 * other screens it hides and comes up when the pointer reaches the bottom
 * edge, so it never sits on a screen's own content.
 */
import { Settings2, X } from "lucide-react";
import clsx from "clsx";
import { useState } from "react";
import { useLocation } from "react-router-dom";
import { useI18n } from "../../i18n/provider";
import type { ActivePluginUi } from "../../lib/pluginUi";
import { AppIcon } from "./shared";
import { usePieces } from "./PiecesProvider";

export const PIECES_ROUTE = "/pieces";

export function Dock() {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const pieces = usePieces();
  const { plugins, isOpen, routeOf, launching, busy, nameOf, entryOf, engines } = pieces;
  const [shown, setShown] = useState(false);
  const pinnedOpen = pathname === PIECES_ROUTE;
  // Apps with a screen go on the dock; the ones without (an inference engine)
  // are notifications on Pieces.
  const docked = plugins.filter((plugin) => isOpen(plugin) && (routeOf(plugin.code) || launching === plugin.code));
  if (docked.length === 0 && !pinnedOpen) return null;
  const pinned = docked.filter((plugin) => !plugin.on_demand);
  const closable = docked.filter((plugin) => plugin.on_demand);
  const visible = pinnedOpen || shown;

  const tile = (plugin: ActivePluginUi) => {
    const name = nameOf(plugin);
    const route = routeOf(plugin.code);
    return (
      <li
        key={plugin.code}
        data-testid={`background-service-${plugin.code}`}
        onContextMenu={pieces.openMenu(plugin)}
        className="group relative flex flex-col items-center gap-0.5"
      >
        <button
          type="button"
          disabled={busy === plugin.code}
          onClick={() => (route ? pieces.goTo(route) : pieces.openSettings(plugin.code))}
          aria-label={route ? t("backgroundServices.goTo", { name }) : t("backgroundServices.settingsNamed", { name })}
          title={name}
          className={clsx(
            "relative rounded-[0.7rem] transition-transform duration-200 ease-out hover:-translate-y-1 hover:scale-125 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70",
            busy === plugin.code && "animate-pulse",
            launching === plugin.code && "animate-bounce",
          )}
        >
          <AppIcon plugin={plugin} engine={engines.has(plugin.code)} dim={false} small storeIcon={entryOf(plugin.code)?.icon_url} />
        </button>
        {route && (
          <button
            type="button"
            onClick={() => pieces.openSettings(plugin.code)}
            aria-label={t("backgroundServices.settingsNamed", { name })}
            title={t("backgroundServices.settings")}
            className="absolute -right-1.5 -top-1.5 flex h-5 w-5 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white/80 opacity-0 shadow transition-opacity hover:text-white focus-visible:opacity-100 group-hover:opacity-100"
          >
            <Settings2 size={11} />
          </button>
        )}
        {plugin.on_demand && (
          <button
            type="button"
            disabled={busy === plugin.code}
            onClick={() => pieces.askClose(plugin)}
            aria-label={t("backgroundServices.closeNamed", { name })}
            title={t("backgroundServices.close")}
            className="absolute -left-1.5 -top-1.5 flex h-5 w-5 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white/80 opacity-0 shadow transition-opacity hover:text-rose focus-visible:opacity-100 group-hover:opacity-100"
          >
            <X size={12} />
          </button>
        )}
        <span className="sr-only">{name}</span>
        <span className="h-1 w-1 rounded-full bg-white/80" aria-hidden="true" />
      </li>
    );
  };

  return (
    <>
      {/* The bottom edge that brings a hidden dock up. */}
      {!pinnedOpen && (
        <div
          aria-hidden="true"
          data-testid="dock-hot-edge"
          onPointerEnter={() => setShown(true)}
          className="absolute inset-x-0 bottom-0 z-40 h-2"
        />
      )}
      <section
        aria-label={t("backgroundServices.running", { count: docked.length })}
        onPointerEnter={() => setShown(true)}
        onPointerLeave={() => setShown(false)}
        onFocus={() => setShown(true)}
        onBlur={() => setShown(false)}
        className={clsx(
          "pointer-events-none absolute inset-x-0 bottom-3 z-40 flex justify-center px-4 transition-transform duration-300 ease-out",
          visible ? "translate-y-0" : "translate-y-[calc(100%+1rem)]",
        )}
      >
        <ul
          data-testid="services-dock"
          className="pointer-events-auto flex max-w-full flex-wrap items-end justify-center gap-x-2 gap-y-2 rounded-2xl border border-white/15 bg-[#0d0a1a]/70 px-2.5 pb-1 pt-2 shadow-[0_18px_50px_rgb(0_0_0/0.5)] backdrop-blur-2xl"
        >
          {docked.length === 0 && <li className="px-4 py-2 text-xs text-white/60">{t("backgroundServices.noneOpen")}</li>}
          {pinned.map(tile)}
          {pinned.length > 0 && closable.length > 0 && (
            <li aria-hidden="true" data-testid="dock-divider" className="mx-0.5 mb-1.5 h-9 w-px self-end bg-white/20" />
          )}
          {closable.map(tile)}
        </ul>
      </section>
    </>
  );
}
