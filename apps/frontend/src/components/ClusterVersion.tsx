import { RefreshCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useI18n } from "../i18n/provider";
import { clusterVersionApi, formatClusterVersion } from "../lib/clusterVersion";
import { useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Badge, Button, Mono, Panel, PanelHeader } from "./ui";

/**
 * The version of the cluster this node sees: its copy of the trust chain,
 * which says who the admins are. When a peer announces a newer one the node
 * catches up by itself; this panel asks it to do so at once and says where
 * the newer copy was seen.
 */
export function ClusterVersion() {
  const { t } = useI18n();
  const node = useNode();
  const invalidate = useInvalidate();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const status = node.data?.cluster_version;
  // One automatic attempt per version seen outdated: the node keeps trying
  // on its own, and a source that never answers must not spin this screen.
  const attempted = useRef<string | null>(null);

  async function sync(manual: boolean) {
    setBusy(true);
    try {
      const answer = await clusterVersionApi.sync();
      invalidate();
      if (manual) {
        toast.success(
          answer.updated ? t("network.clusterVersionSynced") : t("network.clusterVersionNoChange"),
        );
      }
    } catch (e) {
      if (manual) toast.fromError(t("network.clusterVersionSyncFailed"), e);
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    if (!status?.outdated || attempted.current === status.version) return;
    attempted.current = status.version;
    void sync(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status?.outdated, status?.version]);

  if (!status?.version) return null;
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("network.clusterVersionEyebrow")}
        title={t("network.clusterVersionTitle")}
        subtitle={t("network.clusterVersionSubtitle")}
        actions={
          <Button
            size="sm"
            variant="outline"
            icon={<RefreshCw className="h-3.5 w-3.5" />}
            disabled={busy}
            onClick={() => void sync(true)}
          >
            {t("network.clusterVersionSync")}
          </Button>
        }
      />
      <div className="flex flex-wrap items-center gap-3 px-5 py-4">
        <span data-testid="cluster-version">
          <Mono className="text-fg">{formatClusterVersion(status.version)}</Mono>
        </span>
        {status.outdated ? (
          <Badge tone="amber">{t("network.clusterVersionOutdated")}</Badge>
        ) : (
          <Badge tone="aqua">{t("network.clusterVersionCurrent")}</Badge>
        )}
        {status.outdated && status.newer_on.length > 0 && (
          <span className="text-xs text-muted">
            {t("network.clusterVersionNewerOn", { hosts: status.newer_on.join(", ") })}
          </span>
        )}
      </div>
    </Panel>
  );
}
