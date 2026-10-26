import { Loader2, Network, Unplug } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { clusterOutOfSight, isConnecting } from "../lib/connection";
import { useConnection } from "../lib/queries";
import { Badge } from "./ui";

/**
 * Where a member stands in its network, on every screen: "Connecting…" while
 * it is still on its way in (right after a join or a boot it has no models to
 * show yet), then how many machines of the cluster it sees, or a warning when
 * it sees none. An admin or a node off the network shows nothing.
 */
export function ConnectionBadge({ className }: { className?: string }) {
  const { t } = useI18n();
  const connection = useConnection();
  const status = connection.data;
  if (!status || status.phase === "standalone") return null;
  if (isConnecting(status)) {
    return (
      <span title={t(`connection.phase.${status.phase}`)} className={className} data-phase={status.phase}>
        <Badge tone="amber" data-testid="connection-badge">
          <Loader2 className="h-3 w-3 shrink-0 animate-spin" aria-hidden="true" />
          <span className="truncate">{t("connection.badge")}</span>
        </Badge>
      </span>
    );
  }
  const outOfSight = clusterOutOfSight(status);
  const title = outOfSight
    ? t("connection.outOfSightDetail")
    : t(status.admin_in_sight ? "connection.inSightDetail" : "connection.noAdminDetail", { count: status.peers });
  const Icon = outOfSight ? Unplug : Network;
  return (
    <span title={title} className={className} data-phase={status.phase}>
      <Badge tone={outOfSight || !status.admin_in_sight ? "amber" : "lime"} data-testid="connection-badge">
        <Icon className="h-3 w-3 shrink-0" aria-hidden="true" />
        <span className="truncate">
          {outOfSight ? t("connection.outOfSight") : t("connection.inSight", { count: status.peers })}
        </span>
      </Badge>
    </span>
  );
}
