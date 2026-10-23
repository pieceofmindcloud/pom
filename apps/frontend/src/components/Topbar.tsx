import { motion } from "framer-motion";
import {
  Globe2,
  Menu,
  X,
} from "lucide-react";
import { useState } from "react";
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

/** One line saying what this node is and whether the pipeline is closed. */
function NodeChip() {
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

  return (
    <div className="flex min-w-0 items-center gap-2">
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
      <Badge tone={serving ? "aqua" : "neutral"}>
        <Dot tone={serving ? "aqua" : "neutral"} pulse={serving} />
        <span className="truncate">{n.hostname}</span>
      </Badge>
      <NatBadge />
      {c && c.peers.length > 0 && (
        <Badge tone={complete ? "aqua" : "amber"} className="hidden md:inline-flex">
          {t("topbar.nodeCount", { count: c.peers.length })}
          {c.total_layers > 0 && (
            <span className="font-mono">
              · {t("topbar.layers", { covered: c.covered_layers, total: c.total_layers })}
            </span>
          )}
        </Badge>
      )}
    </div>
  );
}

function ProfileBadge() {
  const { profile } = useProfile();
  const { t } = useI18n();

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

        <div className="min-w-0 flex-1 overflow-hidden">
          <NodeChip />
        </div>

        <div className="flex shrink-0 items-center gap-2">
          <OpenNativeChatWindowButton />
          <ProfileBadge />
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
