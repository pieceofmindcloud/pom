/**
 * Take **one** machine out of the network, from its card on the Dashboard.
 *
 * It is the same act as the "This node > Leave the network" menu, which has always applied
 * only to the machine where the panel is open. But a node's panel shows the whole
 * cluster, and going to each machine's interface to shut it down is a detour the
 * operator should not have to make - so the request goes to the local node with the
 * target's `node_id` and it forwards it (`/api/ui/leave`, `webui::forward_leave`).
 * Whoever shuts down is still the owner of that process: this node does not detach
 * anyone from outside.
 *
 * Explicit confirmation because the way back is not this one: the machine that leaves
 * disappears from the panel, and rejoining is done through *its own* Network tab.
 */
import { useState } from "react";
import { LogOut } from "lucide-react";
import { leaveNetwork } from "../lib/api";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Confirm } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";

export function LeaveNode({
  nodeId,
  hostname,
  isSelf,
}: {
  nodeId: string;
  hostname: string;
  /** On the node itself the request is local - with no forwarding and no `node_id`. */
  isSelf: boolean;
}) {
  const { t } = useI18n();
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  async function leave() {
    setBusy(true);
    try {
      await leaveNetwork(isSelf ? undefined : nodeId);
      toast.success(t("dashboard.leftNode", { hostname }), t("dashboard.leftNodeDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("dashboard.leaveNodeFailed", { hostname }), e);
    } finally {
      setBusy(false);
      setAsking(false);
    }
  }

  return (
    <>
      <Button
        size="sm"
        variant="outline"
        onClick={() => setAsking(true)}
        className="shrink-0"
      >
        <LogOut className="h-3.5 w-3.5" />
        {t("dashboard.leaveNode")}
      </Button>

      <Confirm
        open={asking}
        busy={busy}
        tone="danger"
        title={t("dashboard.leaveNodeTitle", { hostname })}
        confirmLabel={t("dashboard.leaveNodeConfirm")}
        onCancel={() => setAsking(false)}
        onConfirm={() => void leave()}
        description={t("dashboard.leaveNodeDescription", { hostname })}
      />
    </>
  );
}
