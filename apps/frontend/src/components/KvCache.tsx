/**
 * O cache de KV de **uma** máquina, no card dela.
 *
 * Dois lugares guardam KV fora do contexto do modelo: o diretório de snapshots
 * em disco (`kv-slots`), que o próprio nó mede e publica em `node.kv_cache`, e
 * o cache em RAM do servidor de inferência (`cache_ram` no `/health`), que só
 * existe com um modelo carregado. A linha mostra os dois e oferece esvaziá-los.
 *
 * O disco aparece mesmo sem modelo carregado: os snapshots são de tudo o que a
 * máquina já serviu, e é justamente depois de trocar de modelo que eles sobram.
 * Como em `LeaveNode`, o pedido vai ao nó local com o `node_id` do alvo
 * (`/api/ui/kv-cache/clear`), porque só o dono de um disco apaga dele.
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
  /** No próprio nó o pedido é local, sem repasse e sem `node_id`. */
  isSelf: boolean;
  /** Ausente num nó antigo, que ainda não publica o campo. */
  disk: KvCacheDisk | undefined;
  /** Ausente sem modelo carregado, ou num nó antigo. */
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
