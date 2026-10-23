import { Loader2 } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { isConnecting } from "../lib/connection";
import { useConnection } from "../lib/queries";
import { Badge } from "./ui";

/**
 * "Connecting…" while this node is still on its way into its network: right
 * after a join or a boot it has no models to show yet, and without this the
 * screens only said the network serves none. Nothing once it arrived.
 */
export function ConnectionBadge({ className }: { className?: string }) {
  const { t } = useI18n();
  const connection = useConnection();
  const status = connection.data;
  if (!isConnecting(status)) return null;
  return (
    <span title={t(`connection.phase.${status!.phase}`)} className={className}>
      <Badge tone="amber" data-testid="connection-badge">
        <Loader2 className="h-3 w-3 shrink-0 animate-spin" aria-hidden="true" />
        <span className="truncate">{t("connection.badge")}</span>
      </Badge>
    </span>
  );
}
