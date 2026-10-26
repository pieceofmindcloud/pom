import { useState } from "react";
import { LogOut } from "lucide-react";
import { api } from "../lib/api";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { Confirm } from "./Confirm";

/**
 * "Leave network" as an icon at the foot of the Sidebar, for a visitor: it has
 * no Network screen worth walking to, and leaving is the one network act it
 * owns. Same act and confirmation as the Networks screen: the node goes back
 * to its own network (`POST /api/ui/network/leave`) and restarts there.
 */
export function LeaveNetworkButton() {
  const { t } = useI18n();
  const toast = useToast();
  const [asking, setAsking] = useState(false);
  const [leaving, setLeaving] = useState(false);

  async function leave() {
    setLeaving(true);
    try {
      await api.post("/api/ui/network/leave");
      toast.success(t("network.forgetNetworkStarted"), t("network.forgetNetworkStartedDetail"));
      window.setTimeout(() => window.location.assign("/admin-ui/"), 1500);
    } catch (reason) {
      toast.fromError(t("network.forgetNetworkFailed"), reason);
      setLeaving(false);
      setAsking(false);
    }
  }

  return (
    <>
      <button
        type="button"
        data-testid="sidebar-leave-network"
        onClick={() => setAsking(true)}
        aria-label={t("network.leave")}
        title={t("network.leave")}
        className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md border border-line text-muted hover:border-rose/50 hover:text-rose"
      >
        <LogOut className="h-3.5 w-3.5" />
      </button>
      <Confirm open={asking} title={t("network.forgetNetworkTitle")} description={t("network.forgetNetworkDescription")} confirmLabel={t("network.leave")} tone="danger" busy={leaving} onConfirm={() => void leave()} onCancel={() => !leaving && setAsking(false)} />
    </>
  );
}
