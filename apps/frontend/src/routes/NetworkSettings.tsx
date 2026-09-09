/**
 * The network as an administrator sees it: who is connected, who is blocked,
 * and whether this machine is even taking part.
 */
import {
  ArrowDownToLine,
  ArrowUp,
  Ban,
  Check,
  Copy,
  Download,
  LogIn,
  LogOut,
  Network,
  Radio,
  Save,
  ShieldOff,
  Undo2,
} from "lucide-react";
import { useEffect, useState } from "react";
import { Confirm } from "../components/Confirm";
import { MachineIcon } from "../components/MachineIcon";
import {
  Badge,
  Button,
  Dot,
  Empty,
  ErrorState,
  Field,
  Footnote,
  Input,
  KeyValue,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { adminAuthorityApi } from "../lib/adminAuthority";
import { layerRange } from "../lib/format";
import { useAdminNodes, useCluster, useInvalidate, useNetworkInfo, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { NetworkInfoView, NetworkSettings as NetworkSettingsData, NodeInfo, TrustShareView } from "../lib/types";
import { useI18n } from "../i18n/provider";

export function NetworkSettings() {
  const { t } = useI18n();
  const node = useNode();
  const cluster = useCluster();
  const networkInfo = useNetworkInfo(Boolean(node.data));
  const isInfer = node.data?.profile === "infer";
  /*
   * Ler o retrato da rede é de `node` e de `admin`; bloquear e desbloquear
   * uma máquina continua sendo ato de administrador, e só com a concessão -
   * `profile: admin` sozinho não concede capacidade. Oferecer o botão a quem
   * vai levar 403 é o defeito de menu que esta base já nomeia.
   */
  const isAdmin = node.data?.authority_level != null;
  const networkEditable = networkInfo.data?.can_edit ?? isAdmin;
  const nodes = useAdminNodes(node.data !== undefined && !isInfer);
  const toast = useToast();
  const invalidate = useInvalidate();

  const [pending, setPending] = useState<{ id: string; hostname: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [nodeRequestBusy, setNodeRequestBusy] = useState(false);
  const [nodeRequestState, setNodeRequestState] = useState<"none" | "pending" | "rejected">("none");
  const [nodeRequestLoaded, setNodeRequestLoaded] = useState(false);
  const [relinquishConfirm, setRelinquishConfirm] = useState(false);
  const [relinquishing, setRelinquishing] = useState(false);

  const [advertiseAddr, setAdvertiseAddr] = useState("");
  const [addrSaving, setAddrSaving] = useState(false);
  const [addrLoaded, setAddrLoaded] = useState(false);
  const [networkDraft, setNetworkDraft] = useState<NetworkSettingsData | null>(null);
  const [networkSaving, setNetworkSaving] = useState(false);
  const [trustShare, setTrustShare] = useState<TrustShareView | null>(null);
  const [trustShareLoaded, setTrustShareLoaded] = useState(false);
  const [trustCopied, setTrustCopied] = useState<"document" | "key" | null>(null);

  useEffect(() => {
    if (!networkInfo.data || (networkDraft && networkEditable)) return;
    setNetworkDraft(networkInfo.data.network);
  }, [networkDraft, networkEditable, networkInfo.data]);

  useEffect(() => {
    if (!isAdmin || trustShareLoaded) return;
    setTrustShareLoaded(true);
    void api.get<TrustShareView>("/api/ui/network/trust-share")
      .then(setTrustShare)
      .catch(() => undefined);
  }, [isAdmin, trustShareLoaded]);

  // Load once: re-syncing on every poll would fight the operator's typing.
  useEffect(() => {
    if (addrLoaded || !node.data) return;
    setAdvertiseAddr(node.data.advertise_addr ?? "");
    setAddrLoaded(true);
  }, [node.data, addrLoaded]);

  useEffect(() => {
    if (nodeRequestLoaded || !node.data) return;
    setNodeRequestState(node.data.node_promotion_request ?? "none");
    setNodeRequestLoaded(true);
  }, [node.data, nodeRequestLoaded]);

  async function saveAdvertiseAddr() {
    setAddrSaving(true);
    try {
      const saved = await api.post<Pick<NodeInfo, "advertise_addr" | "effective_address">>(
        "/api/ui/network/advertise-addr",
        { advertise_addr: advertiseAddr.trim() },
      );
      setAdvertiseAddr(saved.advertise_addr ?? "");
      toast.success(t("network.addrSaved"), t("network.addrSavedDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.addrSaveFailed"), e);
    } finally {
      setAddrSaving(false);
    }
  }

  async function saveNetworkInfo() {
    if (!networkDraft) return;
    setNetworkSaving(true);
    try {
      const saved = await api.post<NetworkInfoView>("/api/ui/network/settings", networkDraft);
      setNetworkDraft(saved.network);
      toast.success(t("network.infoSaved"), t("network.infoSavedDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.infoSaveFailed"), e);
    } finally {
      setNetworkSaving(false);
    }
  }

  async function copySharedValue(kind: "document" | "key", value: string) {
    try {
      await navigator.clipboard.writeText(value);
      setTrustCopied(kind);
      window.setTimeout(() => setTrustCopied(null), 1800);
    } catch {
      // The document and key remain visibly selectable if the clipboard is unavailable.
    }
  }

  function downloadSharedValue(value: string, filename: string) {
    const url = URL.createObjectURL(new Blob([value], { type: "application/json;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = filename;
    link.click();
    URL.revokeObjectURL(url);
  }

  const detached = node.data?.detached ?? false;

  /** Hostnames come from the cluster view; the admin list carries only ids. */
  const systemOf = (id: string) =>
    cluster.data?.peers.find((p) => p.node_id === id)?.node?.system ?? null;

  async function setBlocked(id: string, blocked: boolean) {
    setBusy(true);
    try {
      await api.post(`/admin/nodes/${id}/${blocked ? "block" : "unblock"}`);
      toast.success(blocked ? t("network.blocked") : t("network.unblocked"));
      void nodes.refetch();
    } catch (e) {
      toast.fromError(blocked ? t("network.blockFailed") : t("network.unblockFailed"), e);
    } finally {
      setBusy(false);
      setPending(null);
    }
  }

  async function toggleMembership() {
    setBusy(true);
    try {
      await api.post(detached ? "/api/ui/rejoin" : "/api/ui/leave");
      toast.success(detached ? t("network.back") : t("network.detached"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.operationFailed"), e);
    } finally {
      setBusy(false);
    }
  }

  async function requestNodePromotion() {
    setNodeRequestBusy(true);
    try {
      await adminAuthorityApi.requestNodePromotion();
      setNodeRequestState("pending");
      toast.success(t("network.requestNodeSent"), t("network.requestNodePending"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.requestNodeFailed"), e);
    } finally {
      setNodeRequestBusy(false);
    }
  }

  async function relinquishNode() {
    setRelinquishing(true);
    try {
      await adminAuthorityApi.relinquishNode();
      toast.success(t("network.relinquishNodeStarted"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.relinquishNodeFailed"), e);
    } finally {
      setRelinquishing(false);
      setRelinquishConfirm(false);
    }
  }

  return (
    <>
      <PageHeader
        title={t("network.title")}
        subtitle={t("network.subtitle")}
      />

      <div className="space-y-5">
        <Panel>
          <PanelHeader
            eyebrow={t("network.thisMachine")}
            title={node.data?.hostname ?? "—"}
            subtitle={
              detached
                ? t("network.offlineDetail")
                : t("network.onlineDetail")
            }
            actions={
              <Button
                variant={detached ? "primary" : "danger"}
                loading={busy}
                icon={
                  detached ? <LogIn className="h-3.5 w-3.5" /> : <LogOut className="h-3.5 w-3.5" />
                }
                onClick={() => void toggleMembership()}
              >
                {detached ? t("network.rejoin") : t("network.leave")}
              </Button>
            }
          />
          {node.data && (
            <dl className="grid gap-x-6 gap-y-4 px-5 py-4 sm:grid-cols-3">
              <KeyValue label="node_id">
                <Mono>{node.data.node_id}</Mono>
              </KeyValue>
              <KeyValue label={t("network.ports")}>
                <Mono>
                  {t("network.portValues", {
                    p2p: node.data.p2p_port,
                    gateway: node.data.gateway_port,
                    inference: node.data.inference_port ?? "—",
                  })}
                </Mono>
              </KeyValue>
              <KeyValue label={t("network.profile")}>
                {t(`profile.${node.data.profile}`)}
              </KeyValue>
              {/* O antigo "Papel" (principal/nó) saiu do `/api/ui/node` com o
                  modelo de confiança e deixava a célula em branco. O que existe
                  hoje é a autoridade efetiva, e ela responde à mesma pergunta:
                  esta máquina manda em quê. `profile: admin` sem concessão
                  aparece como sem autoridade, que é o que ele é. */}
              <KeyValue label={t("network.authority")}>
                {node.data.authority_level
                  ? t(`network.authority.${node.data.authority_level}`)
                  : t("network.authorityNone")}
              </KeyValue>
            </dl>
          )}
        </Panel>

        {networkDraft && (
          <NetworkInfoPanel
            info={networkInfo.data}
            draft={networkDraft}
            editable={networkEditable}
            saving={networkSaving}
            onChange={setNetworkDraft}
            onSave={() => void saveNetworkInfo()}
            t={t}
          />
        )}

        {isAdmin && (
          <TrustSharePanel
            share={trustShare}
            copied={trustCopied}
            onCopy={copySharedValue}
            onDownload={downloadSharedValue}
            t={t}
          />
        )}

        {isInfer && (
          <Panel>
            <PanelHeader
              eyebrow={t("network.profile")}
              title={t("profile.infer")}
              subtitle={t("network.inferRequestDetail")}
              actions={
                nodeRequestState === "pending" ? (
                  <Badge tone="amber">{t("network.requestNodePending")}</Badge>
                ) : (
                  <Button
                    variant="primary"
                    loading={nodeRequestBusy}
                    icon={<ArrowUp className="h-3.5 w-3.5" />}
                    onClick={() => void requestNodePromotion()}
                  >
                    {t("network.requestNode")}
                  </Button>
                )
              }
            />
            {nodeRequestState === "rejected" && (
              <p className="border-t border-line px-5 py-3 text-sm text-rose">
                {t("network.requestNodeRejected")}
              </p>
            )}
          </Panel>
        )}

        {node.data?.profile === "node" && (
          <Panel>
            <PanelHeader
              eyebrow={t("network.profile")}
              title={t("network.relinquishNode")}
              subtitle={t("network.relinquishNodeDescription")}
              actions={
                <Button
                  variant="danger"
                  loading={relinquishing}
                  icon={<ArrowDownToLine className="h-3.5 w-3.5" />}
                  onClick={() => setRelinquishConfirm(true)}
                >
                  {t("network.relinquishNode")}
                </Button>
              }
            />
          </Panel>
        )}

        <Panel>
          <PanelHeader
            title={t("network.advertiseTitle")}
            subtitle={t("network.advertiseSubtitle")}
            actions={
              <Button
                variant="primary"
                icon={<Save className="h-3.5 w-3.5" />}
                loading={addrSaving}
                onClick={() => void saveAdvertiseAddr()}
                disabled={!addrLoaded}
              >
                {t("network.addrSave")}
              </Button>
            }
          />
          <div className="space-y-3 px-5 py-4">
            <Field label={t("network.addrLabel")} hint={t("network.addrHint")}>
              <Input
                value={advertiseAddr}
                spellCheck={false}
                placeholder={t("network.addrPlaceholder")}
                onChange={(e) => setAdvertiseAddr(e.target.value)}
              />
            </Field>
            <KeyValue label={t("network.addrEffective")}>
              <Mono>{node.data?.effective_address ?? "—"}</Mono>
            </KeyValue>
            <Footnote icon={<Radio className="h-3.5 w-3.5" />}>
              {t("network.addrPrecedence")}
            </Footnote>
          </div>
        </Panel>

        {nodes.isError && (
          <ErrorState
            title={t("network.listFailed")}
            error={nodes.error}
            onRetry={() => void nodes.refetch()}
          />
        )}
        {nodes.isLoading && <Skeleton className="h-48 w-full" />}

        {nodes.data && (
          <Panel>
            <PanelHeader
              title={t("network.knownCount", { count: nodes.data.length })}
              subtitle={t("network.knownDetail")}
            />
            {nodes.data.length === 0 ? (
              <Empty icon={<Network className="h-5 w-5" />} title={t("network.noNodes")}>
                {t("network.noNodesDetail")}
              </Empty>
            ) : (
              <div className="divide-y divide-line">
                {nodes.data.map((n) => (
                  <div
                    key={n.node_id}
                    className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3.5"
                  >
                    <MachineIcon system={systemOf(n.node_id)} size="sm" />
                    <div className="min-w-[12rem] flex-1">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-medium text-fg">
                          {n.hostname || n.short_id}
                        </span>
                        {n.is_self && (
                          <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">
                            {t("network.here")}
                          </Badge>
                        )}
                        {n.blocked && <Badge tone="rose">{t("network.blockedLabel")}</Badge>}
                        {n.verified && <Badge tone="aqua">{t("network.verified")}</Badge>}
                      </div>
                      <p className="mt-0.5 font-mono text-[0.7rem] text-faint">
                        {n.short_id} · {n.addr || t("network.noAddress")}
                      </p>
                    </div>
                    {n.total_layers ? (
                      <Mono className="shrink-0 text-iris">
                        {layerRange(n.start_layer ?? 0, n.end_layer ?? 0)}
                      </Mono>
                    ) : null}
                    <span className="flex shrink-0 items-center gap-1.5 text-[0.78rem] text-muted">
                      <Dot tone={n.connected ? "aqua" : "neutral"} />
                      {n.connected ? t("network.connected") : t("network.away")}
                    </span>
                    {isAdmin && !n.is_self && (
                      <>
                        <Button
                          size="sm"
                          variant={n.blocked ? "outline" : "danger"}
                          icon={
                            n.blocked ? (
                              <Undo2 className="h-3.5 w-3.5" />
                            ) : (
                              <Ban className="h-3.5 w-3.5" />
                            )
                          }
                          onClick={() =>
                            n.blocked
                              ? void setBlocked(n.node_id, false)
                              : setPending({ id: n.node_id, hostname: n.hostname || n.short_id })
                          }
                        >
                          {n.blocked ? t("network.unblock") : t("network.block")}
                        </Button>
                      </>
                    )}
                  </div>
                ))}
              </div>
            )}
            <Footnote
              className="border-t border-line px-5 py-3"
              icon={<ShieldOff className="h-3.5 w-3.5" />}
            >
              {t("network.identityNote")}
            </Footnote>
          </Panel>
        )}
      </div>

      <Confirm
        open={pending !== null}
        title={t("network.blockTitle", { hostname: pending?.hostname ?? "" })}
        description={t("network.blockDescription")}
        confirmLabel={t("network.block")}
        tone="danger"
        busy={busy}
        onConfirm={() => pending && void setBlocked(pending.id, true)}
        onCancel={() => setPending(null)}
      />
      <Confirm
        open={relinquishConfirm}
        title={t("network.relinquishNodeTitle")}
        description={t("network.relinquishNodeDescription")}
        confirmLabel={t("network.relinquishNode")}
        tone="danger"
        busy={relinquishing}
        onConfirm={() => void relinquishNode()}
        onCancel={() => setRelinquishConfirm(false)}
      />
    </>
  );
}

function NetworkInfoPanel({
  info,
  draft,
  editable,
  saving,
  onChange,
  onSave,
  t,
}: {
  info: NetworkInfoView | undefined;
  draft: NetworkSettingsData;
  editable: boolean;
  saving: boolean;
  onChange: (next: NetworkSettingsData) => void;
  onSave: () => void;
  t: (key: string, values?: Record<string, string | number>) => string;
}) {
  const update = (patch: Partial<NetworkSettingsData>) => onChange({ ...draft, ...patch });
  const publicNetwork = draft.visibility === "public";
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("network.networkInfoEyebrow")}
        title={t("network.networkInfoTitle")}
        subtitle={editable ? t("network.networkInfoEditSubtitle") : t("network.networkInfoReadOnlySubtitle")}
        actions={editable ? <Button variant="primary" icon={<Save className="h-3.5 w-3.5" />} loading={saving} onClick={onSave}>{t("network.infoSave")}</Button> : <Badge tone="neutral">{t("network.readOnly")}</Badge>}
      />
      {editable ? (
        <div className="grid min-w-0 gap-4 px-5 py-5 md:grid-cols-2">
          <Field label={t("network.publicName")} hint={publicNetwork ? t("network.publicNameRequired") : undefined}>
            <Input required={publicNetwork} value={draft.public_name} onChange={(event) => update({ public_name: event.target.value })} />
          </Field>
          <Field label={t("network.organization")}>
            <Input value={draft.organization} onChange={(event) => update({ organization: event.target.value })} />
          </Field>
          <Field label={t("network.description")}>
            <textarea value={draft.description} onChange={(event) => update({ description: event.target.value })} className="min-h-20 w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 text-sm text-fg outline-none focus:border-iris focus:ring-2 focus:ring-iris/25" />
          </Field>
          <Field label={t("network.rules")}>
            <textarea value={draft.rules} onChange={(event) => update({ rules: event.target.value })} className="min-h-20 w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 text-sm text-fg outline-none focus:border-iris focus:ring-2 focus:ring-iris/25" />
          </Field>
          <Field label={t("network.channels")}>
            <Input value={draft.channels} onChange={(event) => update({ channels: event.target.value })} />
          </Field>
          <Field label={t("network.visibility")}>
            <Select value={draft.visibility} onChange={(event) => update({ visibility: event.target.value as NetworkSettingsData["visibility"] })}>
              <option value="local">{t("network.visibilityLocal")}</option>
              <option value="private">{t("network.visibilityPrivate")}</option>
              <option value="public">{t("network.visibilityPublic")}</option>
            </Select>
          </Field>
          <Field label={t("network.accessPolicy")}>
            <Select value={draft.access_policy} onChange={(event) => update({ access_policy: event.target.value as NetworkSettingsData["access_policy"] })}>
              <option value="authorized">{t("network.accessAuthorized")}</option>
              <option value="anyone_with_anchor">{t("network.accessAnyoneWithAnchor")}</option>
              <option value="local_only">{t("network.accessLocalOnly")}</option>
            </Select>
          </Field>
          <Field label={t("network.country")} hint={publicNetwork ? t("network.countryRequired") : undefined}>
            <Input required={publicNetwork} value={draft.country_name} placeholder={draft.country_code} onChange={(event) => update({ country_name: event.target.value })} />
          </Field>
          <Field label={t("network.countryCode")} hint={t("network.countryCodeHint")}>
            <Input required={publicNetwork} maxLength={2} value={draft.country_code} placeholder="BR" onChange={(event) => update({ country_code: event.target.value.toUpperCase() })} />
          </Field>
          <Field label={t("network.city")} hint={publicNetwork ? t("network.cityRequired") : undefined}>
            <Input required={publicNetwork} value={draft.city} onChange={(event) => update({ city: event.target.value })} />
          </Field>
          <div className="min-w-0 md:col-span-2">
            <KeyValue label={t("network.geoLabel")}><Mono className="break-all">{draft.geo_label || t("network.notPublished")}</Mono></KeyValue>
            {draft.location && <p className="mt-3 text-xs text-faint">{t("network.locationCoordinates", { latitude: draft.location.latitude, longitude: draft.location.longitude })}</p>}
          </div>
        </div>
      ) : (
        <dl className="grid min-w-0 gap-x-6 gap-y-4 px-5 py-5 sm:grid-cols-2">
          <KeyValue label={t("network.publicName")}><span className="break-words">{draft.public_name || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.organization")}><span className="break-words">{draft.organization || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.description")}><span className="break-words whitespace-pre-wrap">{draft.description || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.rules")}><span className="break-words whitespace-pre-wrap">{draft.rules || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.channels")}><span className="break-words">{draft.channels || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.visibility")}><span>{visibilityLabel(draft.visibility, t)}</span></KeyValue>
          <KeyValue label={t("network.accessPolicy")}><span>{accessLabel(draft.access_policy, t)}</span></KeyValue>
          <KeyValue label={t("network.country")}><span>{draft.country_name || draft.country_code || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.city")}><span>{draft.city || t("network.notProvided")}</span></KeyValue>
          {draft.location && <KeyValue label={t("network.location")}><span>{t("network.locationCoordinates", { latitude: draft.location.latitude, longitude: draft.location.longitude })}</span></KeyValue>}
        </dl>
      )}
      {info?.network.visibility === "public" && <Footnote className="border-t border-line px-5 py-3" icon={<Radio className="h-3.5 w-3.5" />}>{t("network.publicInfoPropagation")}</Footnote>}
    </Panel>
  );
}

function TrustSharePanel({
  share,
  copied,
  onCopy,
  onDownload,
  t,
}: {
  share: TrustShareView | null;
  copied: "document" | "key" | null;
  onCopy: (kind: "document" | "key", value: string) => Promise<void>;
  onDownload: (value: string, filename: string) => void;
  t: (key: string, values?: Record<string, string | number>) => string;
}) {
  return (
    <Panel>
      <PanelHeader eyebrow={t("network.shareEyebrow")} title={t("network.shareTitle")} subtitle={t("network.shareSubtitle")} />
      {!share ? (
        <p className="px-5 py-5 text-sm text-muted">{t("network.shareLoading")}</p>
      ) : (
        <div className="space-y-4 px-5 py-5">
          <ShareBlock label={t("network.trustDocument")} value={share.document} filename={share.filename} kind="document" copied={copied === "document"} onCopy={onCopy} onDownload={onDownload} t={t} />
          {share.connection_key ? (
            <ShareBlock label={t("network.connectionKey")} value={share.connection_key} filename="pom-connection-key.txt" kind="key" copied={copied === "key"} onCopy={onCopy} onDownload={onDownload} t={t} />
          ) : <p className="text-xs leading-relaxed text-faint">{t("network.publicNoConnectionKey")}</p>}
        </div>
      )}
    </Panel>
  );
}

function ShareBlock({ label, value, filename, kind, copied, onCopy, onDownload, t }: { label: string; value: string; filename: string; kind: "document" | "key"; copied: boolean; onCopy: (kind: "document" | "key", value: string) => Promise<void>; onDownload: (value: string, filename: string) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  return (
    <div className="min-w-0 rounded-xl border border-line bg-raised">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
        <span className="text-sm font-medium text-fg">{label}</span>
        <div className="flex flex-wrap gap-2">
          <Button size="sm" icon={copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />} onClick={() => void onCopy(kind, value)}>{copied ? t("network.copied") : t("network.copy")}</Button>
          <Button size="sm" icon={<Download className="h-3.5 w-3.5" />} onClick={() => onDownload(value, filename)}>{t("network.download")}</Button>
        </div>
      </div>
      <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-words px-4 py-3 font-mono text-[0.7rem] leading-5 text-muted">{value}</pre>
    </div>
  );
}

function visibilityLabel(value: NetworkSettingsData["visibility"], t: (key: string, values?: Record<string, string | number>) => string) {
  return t(`network.visibility${value[0].toUpperCase()}${value.slice(1)}`);
}

function accessLabel(value: NetworkSettingsData["access_policy"], t: (key: string, values?: Record<string, string | number>) => string) {
  return t(`network.access${value === "anyone_with_anchor" ? "AnyoneWithAnchor" : value === "local_only" ? "LocalOnly" : "Authorized"}`);
}
