/**
 * The KV cache of **one** machine, on its card.
 *
 * Two places hold KV outside the model's context: the snapshot directory
 * on disk (`kv-slots`), which the node itself measures and publishes in `node.kv_cache`, and
 * the RAM cache of the inference server (`cache_ram` in `/health`), which only
 * exists with a loaded model. The row shows both and offers to empty them.
 *
 * The disk shows up even without a loaded model: the snapshots are of everything the
 * machine has already served, and it is precisely after switching models that they are left over.
 * As in `LeaveNode`, the request goes to the local node with the target's `node_id`
 * (`/api/ui/kv-cache/clear`), because only the owner of a disk deletes from it.
 */
import { useState } from "react";
import { Trash2 } from "lucide-react";
import { clearKvCache } from "../lib/api";
import { bytes } from "../lib/format";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { KvCacheDisk, KvCacheRam } from "../lib/types";
import { Confirm } from "./Confirm";
import { Button, InfoTip, KeyValue } from "./ui";
import { useI18n } from "../i18n/provider";

export function KvCache({
  nodeId,
  hostname,
  isSelf,
  disk,
  ram,
  className,
}: {
  nodeId: string;
  hostname: string;
  /** On the node itself the request is local, with no forwarding and no `node_id`. */
  isSelf: boolean;
  /** Absent on an old node, which does not publish the field yet. */
  disk: KvCacheDisk | undefined;
  /** Absent without a loaded model, or on an old node. */
  ram: KvCacheRam | undefined;
  className?: string;
}) {
  const { t } = useI18n();
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  if (!disk) return null;

  const held = disk.bytes + (ram?.bytes ?? 0);

  async function clear() {
    setBusy(true);
    try {
      const done = await clearKvCache(isSelf ? undefined : nodeId);
      toast.success(
        t("dashboard.kvCacheCleared", { hostname }),
        t("dashboard.kvCacheClearedDetail", {
          disk: bytes(done.disk_bytes),
          ram: bytes(done.ram_bytes),
        }),
      );
      invalidate();
    } catch (e) {
      toast.fromError(t("dashboard.kvCacheClearFailed", { hostname }), e);
    } finally {
      setBusy(false);
      setAsking(false);
    }
  }

  return (
    <div data-testid="kv-cache" className={className}>
      <div className="mb-1.5 flex items-center gap-1">
        <span className="eyebrow">{t("dashboard.kvCache")}</span>
        <InfoTip
          label={t("dashboard.kvCache")}
          message={t("dashboard.kvCacheTip", { path: disk.path ?? "-" })}
        />
      </div>
      <div className="flex flex-wrap items-end justify-between gap-x-4 gap-y-2">
        <dl className="grid min-w-0 flex-1 gap-x-3 gap-y-2 sm:grid-cols-2">
          <KeyValue compact label={t("dashboard.kvCacheDisk")}>
            {disk.enabled ? (
              <span className="font-mono" data-testid="kv-cache-disk">
                {bytes(disk.bytes)}
                <span className="text-muted">
                  {" · "}
                  {t("dashboard.kvCacheSnapshots", { count: disk.snapshots })}
                </span>
              </span>
            ) : (
              <span className="text-muted">{t("dashboard.kvCacheOff")}</span>
            )}
          </KeyValue>
          {ram && (
            <KeyValue compact label={t("dashboard.kvCacheRam")}>
              {!ram.enabled ? (
                <span className="text-muted">{t("dashboard.kvCacheOff")}</span>
              ) : ram.limit_bytes == null ? (
                <span className="flex items-center gap-1 text-muted">
                  {t("dashboard.kvCacheRamUnavailable")}
                  <InfoTip
                    label={t("dashboard.kvCacheRam")}
                    message={t("dashboard.kvCacheRamUnavailableTip")}
                  />
                </span>
              ) : (
                <span className="font-mono" data-testid="kv-cache-ram">
                  {t("dashboard.kvCacheRamOf", {
                    used: bytes(ram.bytes),
                    limit: bytes(ram.limit_bytes),
                  })}
                </span>
              )}
            </KeyValue>
          )}
        </dl>
        <Button
          size="sm"
          variant="outline"
          disabled={held === 0}
          onClick={() => setAsking(true)}
          className="shrink-0"
        >
          <Trash2 className="h-3.5 w-3.5" />
          {t("dashboard.kvCacheClear")}
        </Button>
      </div>

      <Confirm
        open={asking}
        busy={busy}
        tone="danger"
        title={t("dashboard.kvCacheClearTitle", { hostname })}
        confirmLabel={t("dashboard.kvCacheClearConfirm")}
        onCancel={() => setAsking(false)}
        onConfirm={() => void clear()}
        description={t("dashboard.kvCacheClearDescription", {
          hostname,
          size: bytes(disk.bytes),
        })}
      />
    </div>
  );
}
