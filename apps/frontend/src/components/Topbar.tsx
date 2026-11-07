import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import {
  Globe2,
  Info,
  Menu,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { isServing } from "../lib/format";
import { useCluster, useNode } from "../lib/queries";
import { useProfile } from "../lib/profile";
import { useI18n } from "../i18n/provider";
import { ConnectionBadge } from "./ConnectionStatus";
import { NatBadge } from "./NatBadge";
import { Notifications } from "./Notifications";
import { UsageButton } from "./UsageButton";
import { SidebarContent } from "./Sidebar";
import { Badge, Dot, Skeleton } from "./ui";
import { OpenNativeChatWindowButton, QuitNativeAppButton } from "./OpenNativeChatWindowButton";
export { TlsChip } from "./TlsIndicator";

/** What this node is and whether the pipeline is closed: a line of badges. */
function NodeChipItems() {
  const { t } = useI18n();
  const node = useNode();
  const cluster = useCluster();

  if (node.isLoading) return <Skeleton className="h-8 w-56" />;
  if (node.isError)
    return (
      <Badge tone="rose">
        <Dot tone="rose" /> {t("topbar.nodeUnreachable")}
      </Badge>
    );

  const n = node.data!;
  const c = cluster.data;
  const serving = isServing(n.status.state);
  const complete = c?.complete ?? false;
  // A machine alone in the network named after it: the network badge already
  // says it all, so neither its name again nor "1 node" is drawn.
  const alone =
    Boolean(n.network_name) &&
    n.network_name.trim() === (n.hostname ?? "").trim() &&
    (c?.peers.length ?? 0) <= 1;

  return (
    <>
      {/* Which network this machine is in, on every screen: a node starts in
          a network of its own and may later join someone else's, and nothing
          else on the page says which one it is looking at. */}
      {n.network_name && (
        <Link
          to="/config/network"
          data-testid="topbar-network"
          title={t("topbar.network", { name: n.network_name })}
          aria-label={t("topbar.network", { name: n.network_name })}
          className="min-w-0 shrink rounded-full focus-visible:outline-offset-2"
        >
          <Badge tone="iris" className="max-w-[14rem]">
            <Globe2 className="h-3 w-3 shrink-0" aria-hidden="true" />
            <span className="truncate">{n.network_name}</span>
          </Badge>
        </Link>
      )}
      {/* Still on its way into that network: why there is nothing to show yet. */}
      <ConnectionBadge className="min-w-0 shrink" />
      {!alone && (
        <Badge tone={serving ? "aqua" : "neutral"} data-testid="topbar-machine">
          <Dot tone={serving ? "aqua" : "neutral"} pulse={serving} />
          <span className="truncate">{n.hostname}</span>
        </Badge>
      )}
      <NatBadge />
      {!alone && c && c.peers.length > 0 && (
        <Badge tone={complete ? "aqua" : "amber"}>
          {t("topbar.nodeCount", { count: c.peers.length })}
          {c.total_layers > 0 && (
            <span className="font-mono">
              · {t("topbar.layers", { covered: c.covered_layers, total: c.total_layers })}
            </span>
          )}
        </Badge>
      )}
    </>
  );
}

/** The badges in a line, from `md` up. */
function NodeChip() {
  return (
    <div className="hidden min-w-0 items-center gap-2 md:flex">
      <NodeChipItems />
    </div>
  );
}

/**
 * Below `md` (a phone, the macOS menu-bar panel) the same badges fold into one
 * icon that opens them in a popover like the bell's, so the bar keeps room
 * for its actions.
 */
function NodeChipButton() {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="relative md:hidden" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        data-testid="topbar-details"
        title={t("topbar.details")}
        aria-label={t("topbar.details")}
        aria-expanded={open}
        className={clsx(
          "flex h-9 w-9 items-center justify-center rounded-lg border transition-colors duration-150",
          open
            ? "border-iris/40 bg-iris/12 text-iris"
            : "border-line bg-raised text-muted hover:border-line-strong hover:text-fg",
        )}
      >
        <Info className="h-4 w-4" />
      </button>
      <AnimatePresence>
        {open && (
          <motion.div
            initial={{ opacity: 0, y: -6, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: -6, scale: 0.98 }}
            transition={{ duration: 0.16, ease: [0.22, 1, 0.36, 1] }}
            data-testid="topbar-details-popover"
            className="absolute left-0 top-11 z-50 w-[min(18rem,calc(100vw-2rem))] origin-top-left overflow-hidden rounded-xl border border-line bg-surface shadow-lift"
          >
            <div className="border-b border-line px-4 py-3 font-display text-sm font-semibold text-fg">{t("topbar.details")}</div>
            <div className="flex flex-col items-start gap-2 px-4 py-3 [&>*]:max-w-full">
              <NodeChipItems />
              <ProfileBadge />
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

/** The persisted profile, for an admin or a worker; a visitor's needs no label. */
function ProfileBadge() {
  const { profile } = useProfile();
  const { t } = useI18n();
  if (profile === "visitor") return null;

  return (
    <Badge
      data-testid="node-profile"
      tone="iris"
      className="max-w-[9rem] truncate"
    >
      {t(`profile.${profile}`)}
    </Badge>
  );
}


export function Topbar() {
  const { t } = useI18n();
  const [drawer, setDrawer] = useState(false);

  return (
    <>
      <header className="sticky top-0 z-40 flex h-16 shrink-0 items-center gap-3 border-b border-line bg-surface/85 px-4 backdrop-blur-md sm:px-6">
        <button
          onClick={() => setDrawer(true)}
          aria-label={t("topbar.openMenu")}
          className="flex h-9 w-9 items-center justify-center rounded-lg border border-line bg-raised text-muted transition-colors hover:text-fg lg:hidden"
        >
          <Menu className="h-4 w-4" />
        </button>

        <NodeChipButton />
        <div className="min-w-0 flex-1 overflow-hidden">
          <NodeChip />
        </div>

        <div className="flex shrink-0 items-center gap-2">
          <OpenNativeChatWindowButton />
          <span className="hidden md:contents">
            <ProfileBadge />
          </span>
          <UsageButton />
          <Notifications />
          <QuitNativeAppButton />
        </div>
      </header>

      {/* Menu on narrow screens. The sidebar is the same component. */}
      {drawer && (
        <div className="fixed inset-0 z-50 lg:hidden">
            <div
              className="absolute inset-0 bg-black/55 backdrop-blur-[2px]"
              onClick={() => setDrawer(false)}
            />
            <motion.div
              initial={{ x: -280 }}
              animate={{ x: 0 }}
              exit={{ x: -280 }}
              transition={{ duration: 0.24, ease: [0.22, 1, 0.36, 1] }}
              className="relative flex h-full w-[268px] flex-col border-r border-line bg-surface"
            >
              <button
                onClick={() => setDrawer(false)}
                aria-label={t("topbar.closeMenu")}
                className="absolute right-3 top-5 flex h-8 w-8 items-center justify-center rounded-lg text-faint transition-colors hover:text-fg"
              >
                <X className="h-4 w-4" />
              </button>
              <SidebarContent onNavigate={() => setDrawer(false)} />
            </motion.div>
        </div>
      )}
    </>
  );
}
