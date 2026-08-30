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
  KeyRound,
  Plus,
  RefreshCw,
  ShieldAlert,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { Confirm } from "../components/Confirm";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Field,
  Input,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { since } from "../lib/format";
import { useApiKeys, useCluster, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

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
          void navigator.clipboard.writeText(value);
          setCopied(true);
          window.setTimeout(() => setCopied(false), 1600);
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

// ── API tokens ───────────────────────────────────────────────────────────────

export function TokenKeys() {
  const { t } = useI18n();
  const keys = useApiKeys();
  const cluster = useCluster();
  const toast = useToast();

  const [label, setLabel] = useState("");
  const [nodeId, setNodeId] = useState("");
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<{ key: string; label: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const nameOf = (id: string) =>
    cluster.data?.peers.find((p) => p.node_id === id)?.node?.hostname ??
    (id ? `${id.slice(0, 8)}…` : t("keys.allNetwork"));

  async function create() {
    setCreating(true);
    try {
      await api.post("/admin/api-keys", {
        label: label.trim() || t("keys.unnamed"),
        node_id: nodeId || null,
      });
      toast.success(t("keys.created"), t("keys.createdDetail"));
      setLabel("");
      void keys.refetch();
    } catch (e) {
      toast.fromError(t("keys.createFailed"), e);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(key: string) {
    setBusy(true);
    try {
      await api.del(`/admin/api-keys/${encodeURIComponent(key)}`);
      toast.success(t("keys.revoked"));
      void keys.refetch();
    } catch (e) {
      toast.fromError(t("keys.revokeFailed"), e);
    } finally {
      setBusy(false);
      setRevoking(null);
    }
  }

  return (
    <>
      <PageHeader
        title={t("keys.title")}
        subtitle={t("keys.tokenSubtitle")}
      />

      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
        <div className="space-y-5">
          {keys.isError && (
            <ErrorState error={keys.error} onRetry={() => void keys.refetch()} />
          )}
          {keys.isLoading && <Skeleton className="h-48 w-full" />}

          {keys.data && (
            <Panel>
              <PanelHeader
                title={t("keys.tokenCount", { count: keys.data.length })}
                subtitle={t("keys.tokensCoexist")}
                actions={
                  <Button
                    size="sm"
                    icon={<RefreshCw className="h-3.5 w-3.5" />}
                    loading={keys.isFetching}
                    onClick={() => void keys.refetch()}
                  >
                    {t("keys.refresh")}
                  </Button>
                }
              />
              {keys.data.length === 0 ? (
                <Empty icon={<KeyRound className="h-5 w-5" />} title={t("keys.empty")}>
                  {t("keys.connectionOnly")}
                </Empty>
              ) : (
                <div className="divide-y divide-line">
                  {keys.data.map((k) => (
                    <div key={k.key} className="px-5 py-3.5">
                      <div className="flex flex-wrap items-center justify-between gap-3">
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <span className="font-medium text-fg">{k.label}</span>
                          <Badge tone={k.active ? "aqua" : "neutral"}>
                            {k.active ? t("keys.active") : t("keys.inactive")}
                          </Badge>
                          <Badge tone="neutral">{nameOf(k.node_id)}</Badge>
                          <Mono className="text-faint">{k.id}</Mono>
                        </div>
                        <div className="flex shrink-0 items-center gap-3">
                          <span className="text-[0.72rem] text-faint">
                            {t("keys.createdAt")} {since(k.created_at)}
                          </span>
                          <Button
                            size="sm"
                            variant="ghost"
                            aria-label={t("keys.revoke", { label: k.label })}
                            icon={<Trash2 className="h-3.5 w-3.5" />}
                            onClick={() => setRevoking({ key: k.key, label: k.label })}
                          />
                        </div>
                      </div>
                      <div className="mt-2.5">
                        <Secret value={k.key} label={t("keys.tokenLabel", { label: k.label })} />
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </Panel>
          )}
        </div>

        <Panel className="self-start">
          <PanelHeader title={t("keys.newToken")} />
          <div className="space-y-4 px-5 py-4">
            <Field label={t("keys.purpose")} hint={t("keys.purposeHint")}>
              <Input
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                placeholder="opencode, painel, CI…"
              />
            </Field>
            <Field
              label={t("keys.networkNode")}
              hint={t("keys.nodeHint")}
            >
              <Select value={nodeId} onChange={(e) => setNodeId(e.target.value)}>
                <option value="">{t("keys.allNetwork")}</option>
                {(cluster.data?.peers ?? []).map((p) => (
                  <option key={p.node_id} value={p.node_id}>
                    {p.node?.hostname || p.short_id}
                  </option>
                ))}
              </Select>
            </Field>
            <Button
              variant="primary"
              icon={<Plus className="h-3.5 w-3.5" />}
              loading={creating}
              onClick={() => void create()}
            >
              {t("keys.create")}
            </Button>
          </div>
        </Panel>
      </div>

      <Confirm
        open={revoking !== null}
        title={t("keys.revokeTitle", { label: revoking?.label ?? "" })}
        description={t("keys.revokeDescription")}
        confirmLabel={t("keys.revoke")}
        tone="danger"
        busy={busy}
        onConfirm={() => revoking && void revoke(revoking.key)}
        onCancel={() => setRevoking(null)}
      />
    </>
  );
}

// ── Connection key ───────────────────────────────────────────────────────────

export function ConnectionKeys() {
  const { t } = useI18n();
  const node = useNode();
  const toast = useToast();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);

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
                <Button
                  variant="danger"
                  icon={<RefreshCw className="h-3.5 w-3.5" />}
                  onClick={() => setConfirming(true)}
                >
                  {t("keys.generateAnother")}
                </Button>
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
