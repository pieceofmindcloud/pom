/**
 * The two credentials this product has, kept on separate screens because they
 * do very different jobs.
 *
 * The **connection key** is the cluster's operator credential: it is the same
 * string on every machine, it is what a second machine is given at setup, and
 * it also opens the `/v1` routes. The **API tokens** are what you hand to a
 * harness — one per node, so the Logs screen can say which one paid for what.
 */
import {
  Check,
  Copy,
  Eye,
  EyeOff,
  RefreshCw,
  ShieldAlert,
} from "lucide-react";
import { useState } from "react";
import { Confirm } from "../components/Confirm";
import { copyText } from "../lib/clipboard";
import {
  Button,
  ErrorState,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { ApiKeysTable } from "../components/ApiKeysTable";

function Secret({ value, label }: { value: string; label: string }) {
  const { t } = useI18n();
  const [shown, setShown] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex min-w-0 items-center gap-2">
      <code className="min-w-0 flex-1 truncate rounded-lg border border-line bg-ink px-3 py-2 font-mono text-[0.78rem] text-fg">
        {shown ? value : "•".repeat(Math.min(48, value.length))}
      </code>
      <Button
        size="sm"
        variant="ghost"
        aria-label={shown ? t("keys.hide", { label }) : t("keys.show", { label })}
        onClick={() => setShown((s) => !s)}
        icon={shown ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
      />
      <Button
        size="sm"
        variant="ghost"
        aria-label={t("keys.copy", { label })}
        onClick={() => {
          void copyText(value).then((ok) => {
            if (!ok) return;
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1600);
          });
        }}
        icon={
          copied ? (
            <Check className="h-3.5 w-3.5 text-aqua" />
          ) : (
            <Copy className="h-3.5 w-3.5" />
          )
        }
      />
    </div>
  );
}

// ── Connection key ───────────────────────────────────────────────────────────

export function TokenKeys() {
  return <ApiKeysTable />;
}

export function ConnectionKeys() {
  const { t } = useI18n();
  const node = useNode();
  const toast = useToast();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  /*
   * Seeing the key belongs to whoever operates this machine - it is already in
   * its `pom.json`, and this screen is only served behind the administrative
   * boundary. **Changing it** is another matter: the key is the same across the
   * whole cluster, and rotating it cuts off every other node. It is still
   * `/admin/regenerate-key`, and the effective authority is what decides -
   * `profile: admin` without a grant does not have it.
   */
  const canRotate = node.data?.authority_level === "primary";

  async function regenerate() {
    setBusy(true);
    try {
      await api.post("/admin/regenerate-key");
      toast.success(t("keys.rotated"), t("keys.rotatedDetail"));
      void node.refetch();
    } catch (e) {
      toast.fromError(t("keys.rotateFailed"), e);
    } finally {
      setBusy(false);
      setConfirming(false);
    }
  }

  return (
    <>
      <PageHeader
        title={t("keys.connectionTitle")}
        subtitle={t("keys.connectionSubtitle")}
      />

      {node.isLoading && <Skeleton className="h-48 w-full" />}
      {node.isError && <ErrorState error={node.error} onRetry={() => void node.refetch()} />}

      {node.data && (
        <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
          <Panel>
            <PanelHeader
              eyebrow={node.data.hostname}
              title={t("keys.clusterKey")}
              subtitle={t("keys.clusterKeySubtitle")}
              actions={
                canRotate ? (
                  <Button
                    variant="danger"
                    icon={<RefreshCw className="h-3.5 w-3.5" />}
                    onClick={() => setConfirming(true)}
                  >
                    {t("keys.generateAnother")}
                  </Button>
                ) : undefined
              }
            />
            <div className="space-y-4 px-5 py-4">
              <Secret value={node.data.connection_key} label={t("keys.connectionKey")} />
              <dl className="grid gap-4 sm:grid-cols-2">
                <div>
                  <dt className="eyebrow mb-1">{t("keys.bootstrapAddress")}</dt>
                  <dd>
                    <Mono className="text-fg">
                      {node.data.system.hostname}:{node.data.p2p_port}
                    </Mono>
                  </dd>
                </div>
                <div>
                  <dt className="eyebrow mb-1">{t("keys.apiBaseUrl")}</dt>
                  <dd>
                    <Mono className="text-fg">
                      http://{node.data.system.hostname}:{node.data.gateway_port}/v1
                    </Mono>
                  </dd>
                </div>
              </dl>
            </div>
          </Panel>

          <Panel className="self-start">
            <PanelHeader title={t("keys.keyAccess")} />
            <ul className="space-y-3 px-5 py-4 text-[0.83rem] leading-relaxed text-muted">
              <li>
                {t("keys.adminAccess")}
              </li>
              <li>
                {t("keys.openaiAccess")}
              </li>
              <li className="flex gap-2.5 pt-1">
                <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
                {t("keys.rotationWarning")}
              </li>
            </ul>
          </Panel>
        </div>
      )}

      <Confirm
        open={confirming}
        title={t("keys.rotateTitle")}
        description={t("keys.rotateDescription")}
        confirmLabel={t("keys.generateAnother")}
        tone="danger"
        busy={busy}
        onConfirm={() => void regenerate()}
        onCancel={() => setConfirming(false)}
      />
    </>
  );
}
