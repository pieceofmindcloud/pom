import { Check, Copy, KeyRound, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useMemo, useState } from "react";
import { Confirm, Modal } from "./Confirm";
import { Badge, Button, Empty, ErrorState, Field, Input, Mono, PageHeader, Select, Skeleton } from "./ui";
import { api } from "../lib/api";
import { copyText } from "../lib/clipboard";
import { since } from "../lib/format";
import { useApiKeys, useCluster, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { useProfile } from "../lib/profile";
import type { ApiKey, IssuedApiKey } from "../lib/types";

/**
 * O token recém-criado, na única vez em que ele existe fora do cliente.
 *
 * A rede guarda só o hash e o trecho visível: fechar esta janela sem copiar é
 * perder o token, e por isso ela não fecha clicando fora nem tem "cancelar" -
 * só o botão que diz que a cópia foi feita.
 */
function IssuedSecret({ issued, onDone }: { issued: IssuedApiKey | null; onDone: () => void }) {
  const { t } = useI18n();
  const [copied, setCopied] = useState(false);
  const copy = () => {
    if (!issued) return;
    void copyText(issued.key).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    });
  };
  return <Modal open={issued !== null} title={t("keys.issuedTitle")} onClose={() => undefined} footer={<Button variant="primary" onClick={() => { setCopied(false); onDone(); }}>{t("keys.issuedDone")}</Button>}>
    {issued && <div className="space-y-3">
      <p>{t("keys.issuedDetail")}</p>
      <div className="flex items-center gap-2">
        <code data-testid="issued-secret" className="min-w-0 flex-1 break-all rounded-md border border-line bg-ink px-2.5 py-2 font-mono text-[0.72rem] text-fg">{issued.key}</code>
        <Button size="sm" variant="outline" aria-label={t("keys.copy", { label: issued.label })} onClick={copy} icon={copied ? <Check className="h-3.5 w-3.5 text-aqua" /> : <Copy className="h-3.5 w-3.5" />}>{copied ? t("keys.issuedCopied") : null}</Button>
      </div>
    </div>}
  </Modal>;
}

export function ApiKeysTable() {
  const { t } = useI18n();
  const { profile, secondary } = useProfile();
  const node = useNode();
  const cluster = useCluster();
  const keys = useApiKeys();
  const toast = useToast();
  // Other machines' keys are the principal's to see and issue.
  const isAdmin = profile === "admin" && !secondary;
  const inference = profile === "infer";
  const ownId = node.data?.node_id;
  const [createOpen, setCreateOpen] = useState(false);
  const [label, setLabel] = useState("");
  const [nodeId, setNodeId] = useState("");
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<{ id: string; label: string } | null>(null);
  const [issued, setIssued] = useState<IssuedApiKey | null>(null);
  const [busy, setBusy] = useState(false);
  const ownKey = (key: ApiKey) => (key.origin_node_id ?? key.node_id) === ownId;
  const visibleKeys = useMemo(() => {
    const filtered = (keys.data ?? []).filter((key) => isAdmin ? true : ownKey(key) && key.active);
    return isAdmin ? [...filtered].sort((left, right) => Number(ownKey(right)) - Number(ownKey(left))) : filtered;
  }, [isAdmin, keys.data, ownId]);
  const previousKeys = inference ? visibleKeys : [];
  const nameOf = (id: string) => cluster.data?.peers.find((peer) => peer.node_id === id)?.node?.hostname || (id ? `${id.slice(0, 8)}…` : t("keys.allNetwork"));

  async function create() {
    setCreating(true);
    try {
      const created = await api.post<IssuedApiKey>("/admin/api-keys", { label: label.trim() || (inference ? t("keys.inferenceLabel") : t("keys.unnamed")), node_id: isAdmin ? nodeId || null : ownId ?? null });
      if (inference && previousKeys.length > 0) {
        try {
          await Promise.all(previousKeys.map((key) => api.del(`/admin/api-keys/${encodeURIComponent(key.id)}`)));
        } catch (error) {
          try { await api.del(`/admin/api-keys/${encodeURIComponent(created.id)}`); } catch { /* A próxima leitura expõe o estado. */ }
          throw error;
        }
      }
      toast.success(t("keys.created"));
      setLabel("");
      setNodeId("");
      setCreateOpen(false);
      setIssued(created);
      void keys.refetch();
    } catch (error) {
      toast.fromError(t("keys.createFailed"), error);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(id: string) {
    setBusy(true);
    try {
      await api.del(`/admin/api-keys/${encodeURIComponent(id)}`);
      toast.success(t("keys.revoked"));
      void keys.refetch();
    } catch (error) {
      toast.fromError(t("keys.revokeFailed"), error);
    } finally {
      setBusy(false);
      setRevoking(null);
    }
  }

  return <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink"><PageHeader className="!mb-0 border-b border-line bg-surface/75 px-3 py-3 sm:px-4" title={t("keys.title")} subtitle={t("keys.tokenSubtitle")} actions={<><Button size="sm" icon={<RefreshCw className="h-3.5 w-3.5" />} loading={keys.isFetching} onClick={() => void keys.refetch()}>{t("keys.refresh")}</Button><Button size="sm" variant="primary" icon={<Plus className="h-3.5 w-3.5" />} onClick={() => setCreateOpen(true)}>{t("keys.create")}</Button></>} />{keys.isError ? <div className="px-5 py-5"><ErrorState error={keys.error} onRetry={() => void keys.refetch()} /></div> : keys.isLoading ? <div className="space-y-3 px-5 py-5"><Skeleton className="h-10 w-full" /><Skeleton className="h-10 w-full" /></div> : <div className="min-h-0 flex-1 overflow-auto border-y border-line"><table className="min-w-[900px] w-full border-collapse text-left text-xs"><thead className="sticky top-0 z-10 bg-raised text-[0.66rem] uppercase tracking-[0.12em] text-faint"><tr><th className="border-b border-line px-4 py-3 font-medium">{t("keys.createdAt")}</th><th className="border-b border-line px-4 py-3 font-medium">{t("keys.networkNode")}</th><th className="border-b border-line px-4 py-3 font-medium">{t("keys.purpose")}</th><th className="border-b border-line px-4 py-3 font-medium">{t("keys.tokenValue")}</th><th className="border-b border-line px-4 py-3 font-medium" title={t("keys.trackingIdHint")}>{t("keys.trackingId")}</th><th className="border-b border-line px-4 py-3 font-medium">{t("keys.status")}</th><th className="border-b border-line px-4 py-3 font-medium">{t("keys.actions")}</th></tr></thead><tbody className="divide-y divide-line">{visibleKeys.map((key) => <tr key={key.id} className="align-middle hover:bg-raised/45"><td className="whitespace-nowrap px-4 py-3.5 text-muted">{since(key.created_at)}</td><td className="px-4 py-3.5"><span className="text-fg">{nameOf(key.node_id)}</span><Mono className="mt-1 block text-[0.64rem] text-faint">{key.node_id}</Mono></td><td className="px-4 py-3.5 text-fg">{key.label}</td><td className="px-4 py-3.5"><code data-testid="key-hint" aria-label={t("keys.tokenLabel", { label: key.label })} className="inline-block rounded-md border border-line bg-ink px-2.5 py-1.5 font-mono text-[0.72rem] text-fg">{key.key_hint || "—"}</code></td><td className="px-4 py-3.5"><Mono className="text-[0.66rem] text-muted" title={t("keys.trackingIdHint")}>{key.id}</Mono></td><td className="px-4 py-3.5"><Badge tone={key.active ? "aqua" : "neutral"}>{key.active ? t("keys.active") : t("keys.inactive")}</Badge></td><td className="px-4 py-3.5">{(isAdmin || ownKey(key)) && <Button size="sm" variant="ghost" aria-label={t("keys.revoke", { label: key.label })} icon={<Trash2 className="h-3.5 w-3.5" />} onClick={() => setRevoking({ id: key.id, label: key.label })} />}</td></tr>)}</tbody></table>{visibleKeys.length === 0 && <Empty icon={<KeyRound className="h-5 w-5" />} title={t("keys.empty")}>{t("keys.connectionOnly")}</Empty>}</div>}<Modal open={createOpen} title={t("keys.newToken")} onClose={() => !creating && setCreateOpen(false)} footer={<><Button variant="ghost" onClick={() => setCreateOpen(false)}>{t("common.close")}</Button><Button variant="primary" loading={creating} onClick={() => void create()}>{t("keys.create")}</Button></>}><div className="space-y-4"><Field label={t("keys.purpose")} hint={t("keys.purposeHint")}><Input value={label} onChange={(event) => setLabel(event.target.value)} placeholder={t("keys.purposePlaceholder")} /></Field>{isAdmin && <Field label={t("keys.networkNode")} hint={t("keys.nodeHint")}><Select value={nodeId} onChange={(event) => setNodeId(event.target.value)}><option value="">{t("keys.allNetwork")}</option>{(cluster.data?.peers ?? []).map((peer) => <option key={peer.node_id} value={peer.node_id}>{peer.node?.hostname || peer.short_id}</option>)}</Select></Field>}</div></Modal><Confirm open={revoking !== null} title={t("keys.revokeTitle", { label: revoking?.label ?? "" })} description={t("keys.revokeDescription")} confirmLabel={t("keys.revokeConfirm")} tone="danger" busy={busy} onConfirm={() => revoking && void revoke(revoking.id)} onCancel={() => setRevoking(null)} /><IssuedSecret issued={issued} onDone={() => setIssued(null)} /></div>;
}
