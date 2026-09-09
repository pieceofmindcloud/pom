import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import {
  ChevronDown,
  LogOut,
  Menu,
  PowerOff,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { isServing } from "../lib/format";
import { useCluster, useInvalidate, useNode } from "../lib/queries";
import { useProfile } from "../lib/profile";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { Confirm } from "./Confirm";
import { Notifications } from "./Notifications";
import { SidebarContent } from "./Sidebar";
import { Badge, Dot, Skeleton } from "./ui";
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
      <Badge tone={serving ? "aqua" : "neutral"}>
        <Dot tone={serving ? "aqua" : "neutral"} pulse={serving} />
        <span className="truncate">{n.hostname}</span>
      </Badge>
      <Badge mono tone="neutral" className="hidden sm:inline-flex">
        {n.short_id}
      </Badge>
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

type PendingAction = "unload" | "leave" | null;

function NodeActions() {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState<PendingAction>(null);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const node = useNode();
  const toast = useToast();
  const invalidate = useInvalidate();

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, [open]);

  const loaded = node.data?.config != null || isServing(node.data?.status.state);
  const leaveLabel = t("topbar.leaveNetworkNamed", {
    name: node.data?.network_name?.trim() || t("sidebar.thisNetwork"),
  });

  async function run(action: Exclude<PendingAction, null>) {
    setBusy(true);
    try {
      if (action === "unload") {
        await api.post("/api/ui/unload");
        toast.success(t("topbar.unloaded"), t("topbar.unloadedDetail"));
      } else {
        await api.post("/api/ui/network/leave");
        toast.success(
          t("topbar.left"),
          t("topbar.leftDetail"),
        );
        window.setTimeout(() => window.location.assign("/setup"), 1500);
      }
      invalidate();
    } catch (e) {
      toast.fromError(
        action === "unload" ? t("topbar.unloadFailed") : t("topbar.leaveFailed"),
        e,
      );
    } finally {
      setBusy(false);
      setPending(null);
    }
  }

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={clsx(
          "flex h-9 items-center gap-1.5 rounded-lg border px-2.5 text-[0.82rem] transition-colors duration-150",
          open
            ? "border-iris/40 bg-iris/12 text-iris"
            : "border-line bg-raised text-muted hover:border-line-strong hover:text-fg",
        )}
      >
        <span className="hidden sm:inline">{t("topbar.thisNode")}</span>
        <ChevronDown
          className={clsx("h-3.5 w-3.5 transition-transform duration-200", open && "rotate-180")}
        />
      </button>

      <AnimatePresence>
        {open && (
          <motion.div
            initial={{ opacity: 0, y: -6, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: -6, scale: 0.98 }}
            transition={{ duration: 0.16, ease: [0.22, 1, 0.36, 1] }}
            className="absolute right-0 top-11 z-50 w-72 origin-top-right overflow-hidden rounded-xl border border-line bg-surface p-1.5 shadow-lift"
          >
            <button
              disabled={!loaded}
              onClick={() => {
                setOpen(false);
                setPending("unload");
              }}
              className="flex w-full items-start gap-3 rounded-lg px-2.5 py-2 text-left transition-colors hover:bg-line/50 disabled:pointer-events-none disabled:opacity-45"
            >
              <PowerOff className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
              <span className="min-w-0">
                <span className="block text-[0.84rem] font-medium text-fg">
                  {t("topbar.unloadModel")}
                </span>
                <span className="block text-xs leading-relaxed text-muted">
                  {loaded
                    ? t("topbar.unloadDetail")
                    : t("topbar.noModel")}
                </span>
              </span>
            </button>
            <button
              onClick={() => {
                setOpen(false);
                setPending("leave");
              }}
              className="flex w-full items-start gap-3 rounded-lg px-2.5 py-2 text-left transition-colors hover:bg-line/50"
            >
              <LogOut className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
              <span className="min-w-0">
                <span className="block text-[0.84rem] font-medium text-fg">
                  {leaveLabel}
                </span>
                <span className="block text-xs leading-relaxed text-muted">
                  {t("sidebar.leaveNetworkDescription")}
                </span>
              </span>
            </button>
          </motion.div>
        )}
      </AnimatePresence>

      <Confirm
        open={pending === "unload"}
        title={t("topbar.unloadTitle")}
        description={
          t("topbar.unloadRangeDescription", {
            range: node.data?.config
              ? `${node.data.config.start_layer}–${node.data.config.end_layer}`
              : t("topbar.thisNode"),
          })
        }
        confirmLabel={t("topbar.unload")}
        tone="danger"
        busy={busy}
        onConfirm={() => run("unload")}
        onCancel={() => setPending(null)}
      />
      <Confirm
        open={pending === "leave"}
        title={t("topbar.leaveTitle")}
        description={t("sidebar.leaveNetworkDescription")}
        confirmLabel={leaveLabel}
        tone="danger"
        busy={busy}
        onConfirm={() => run("leave")}
        onCancel={() => setPending(null)}
      />
    </div>
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
          <ProfileBadge />
          <Notifications />
          <NodeActions />
        </div>
      </header>

      {/* Menu on narrow screens. The sidebar is the same component. */}
      <AnimatePresence>
        {drawer && (
          <motion.div
            className="fixed inset-0 z-50 lg:hidden"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
          >
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
          </motion.div>
        )}
      </AnimatePresence>
    </>
  );
}
