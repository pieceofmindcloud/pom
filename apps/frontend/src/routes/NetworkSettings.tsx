/**
 * The network as an administrator sees it: who is connected, who is blocked,
 * and whether this machine is even taking part.
 */
import {
  ArrowDownToLine,
  ArrowUp,
  Ban,
  Network,
  Radio,
  RefreshCw,
  Save,
  ShieldAlert,
  ShieldCheck,
  ShieldOff,
  Trash2,
  Undo2,
} from "lucide-react";
import { useEffect, useState } from "react";
import { Confirm } from "../components/Confirm";
import { MachineIcon } from "../components/MachineIcon";
import { Markdown } from "../components/Markdown";
import { MarkdownField } from "../components/MarkdownField";
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
import { useAdminNodes, useCluster, useInvalidate, useNetworkInfo, useNode, useWindowsFirewall } from "../lib/queries";
import { useGeoReference } from "../lib/queries";
import { networkStats } from "../lib/networkStats";
import { useToast } from "../lib/toast";
import { ENROLLMENT_ROLES } from "../lib/types";
import type { AdminNode, ClusterView, EnrollmentRole, GeoReferenceResponse, NetworkInfoView, NetworkSettings as NetworkSettingsData, PeerRoute, TraversalView, WindowsFirewallStatus } from "../lib/types";
import { useI18n } from "../i18n/provider";

export function NetworkSettings({ participantsOnly = false }: { participantsOnly?: boolean }) {
  const { t } = useI18n();
  const node = useNode();
  const cluster = useCluster();
  const networkInfo = useNetworkInfo(Boolean(node.data));
  const firewall = useWindowsFirewall(Boolean(node.data));
  const geoReference = useGeoReference(Boolean(node.data));
  const isInfer = node.data?.profile === "infer";
  /*
   * Ler o retrato da rede é de `node` e de `admin`; bloquear e desbloquear
   * uma máquina continua sendo ato de administrador, e só com a concessão -
   * `profile: admin` sozinho não concede capacidade. Oferecer o botão a quem
   * vai levar 403 é o defeito de menu que esta base já nomeia.
   */
  const isAdmin = node.data?.authority_level != null;
  const networkEditable = networkInfo.data?.can_edit ?? isAdmin;
  const nodes = useAdminNodes(participantsOnly && node.data !== undefined && !isInfer);
  const toast = useToast();
  const invalidate = useInvalidate();

  const [pending, setPending] = useState<{ id: string; hostname: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [nodeRequestBusy, setNodeRequestBusy] = useState(false);
  const [nodeRequestState, setNodeRequestState] = useState<"none" | "pending" | "rejected">("none");
  const [nodeRequestLoaded, setNodeRequestLoaded] = useState(false);
  const [relinquishConfirm, setRelinquishConfirm] = useState(false);
  const [relinquishing, setRelinquishing] = useState(false);
  const [forgetNetworkConfirm, setForgetNetworkConfirm] = useState(false);
  const [forgettingNetwork, setForgettingNetwork] = useState(false);

  const [networkDraft, setNetworkDraft] = useState<NetworkSettingsData | null>(null);
  const [networkSaving, setNetworkSaving] = useState(false);
  const [firewallBusy, setFirewallBusy] = useState(false);

  useEffect(() => {
    if (!networkInfo.data || (networkDraft && networkEditable)) return;
    setNetworkDraft(networkInfo.data.network);
  }, [networkDraft, networkEditable, networkInfo.data]);

  useEffect(() => {
    if (nodeRequestLoaded || !node.data) return;
    setNodeRequestState(node.data.node_promotion_request ?? "none");
    setNodeRequestLoaded(true);
  }, [node.data, nodeRequestLoaded]);

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

  async function retryFirewall() {
    setFirewallBusy(true);
    try {
      await api.post("/api/ui/network/firewall/retry");
      await firewall.refetch();
      toast.success(t("network.firewallRetrying"));
    } catch (e) {
      toast.fromError(t("network.firewallRetryFailed"), e);
    } finally {
      setFirewallBusy(false);
    }
  }

  const metrics = networkStats(cluster.data);

  if (participantsOnly) {
    return (
      <>
        <PageHeader title={t("network.participantsTitle")} subtitle={t("network.participantsSubtitle")} />
        <ParticipantsPanel nodes={nodes} cluster={cluster.data} isAdmin={isAdmin} setPending={setPending} setBlocked={setBlocked} t={t} />
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
      </>
    );
  }

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

  async function forgetNetwork() {
    setForgettingNetwork(true);
    try {
      await api.post("/api/ui/network/leave");
      toast.success(t("network.forgetNetworkStarted"), t("network.forgetNetworkStartedDetail"));
      window.setTimeout(() => window.location.assign("/admin-ui/"), 1500);
    } catch (e) {
      toast.fromError(t("network.forgetNetworkFailed"), e);
    } finally {
      setForgettingNetwork(false);
      setForgetNetworkConfirm(false);
    }
  }

  return (
    <>
      <PageHeader
        title={t("network.title")}
        subtitle={t("network.subtitle")}
      />

      <div className="space-y-5">
        {firewall.data?.supported && (
          <WindowsFirewallPanel
            status={firewall.data}
            busy={firewallBusy}
            onRetry={() => void retryFirewall()}
            t={t}
          />
        )}

        {networkDraft && (
          <NetworkInfoPanel
            info={networkInfo.data}
            draft={networkDraft}
            geo={geoReference.data}
            editable={networkEditable}
            saving={networkSaving}
            forgetting={forgettingNetwork}
            onChange={setNetworkDraft}
            onSave={() => void saveNetworkInfo()}
            onForgetNetwork={() => setForgetNetworkConfirm(true)}
            t={t}
          />
        )}

        <NetworkMetricsPanel metrics={metrics} t={t} />

        {cluster.data?.traversal && <TraversalPanel traversal={cluster.data.traversal} t={t} />}

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
      <Confirm
        open={forgetNetworkConfirm}
        title={t("network.forgetNetworkTitle")}
        description={t("network.forgetNetworkDescription")}
        confirmLabel={t("network.forgetNetwork")}
        tone="danger"
        busy={forgettingNetwork}
        onConfirm={() => void forgetNetwork()}
        onCancel={() => setForgetNetworkConfirm(false)}
      />
    </>
  );
}

function WindowsFirewallPanel({
  status,
  busy,
  onRetry,
  t,
}: {
  status: WindowsFirewallStatus;
  busy: boolean;
  onRetry: () => void;
  t: (key: string, values?: Record<string, string | number>) => string;
}) {
  const ready = status.all_allowed;
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("network.firewallEyebrow")}
        title={t("network.firewallTitle")}
        subtitle={t("network.firewallSubtitle")}
        actions={
          <Button
            variant={ready ? "outline" : "primary"}
            size="sm"
            loading={busy || status.in_progress}
            disabled={!status.can_retry}
            icon={<RefreshCw className="h-3.5 w-3.5" />}
            onClick={onRetry}
          >
            {t("network.firewallRetry")}
          </Button>
        }
      />
      <div className="space-y-3 px-5 py-4">
        <div className="flex items-start gap-3">
          {ready ? (
            <ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-emerald-500" />
          ) : (
            <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber-500" />
          )}
          <div className="min-w-0">
            <p className="text-sm font-medium text-fg">
              {ready ? t("network.firewallReady") : t("network.firewallNeeded")}
            </p>
            <p className="mt-1 text-sm leading-relaxed text-muted">
              {t("network.firewallPortSummary", {
                ports: status.rules.map((rule) => `${rule.protocol.toUpperCase()} ${rule.port}`).join(", "),
              })}
            </p>
          </div>
        </div>
        {!ready && !status.automatic_attempted && (
          <p className="text-sm text-muted">{t("network.firewallUacDetail")}</p>
        )}
        {status.last_error && (
          <p className="text-sm text-rose">{t("network.firewallLastError")}</p>
        )}
      </div>
    </Panel>
  );
}

export function NetworkParticipants() {
  return <NetworkSettings participantsOnly />;
}

function NetworkMetricsPanel({ metrics, t }: { metrics: ReturnType<typeof networkStats>; t: (key: string, values?: Record<string, string | number>) => string }) {
  return (
    <Panel>
      <PanelHeader eyebrow={t("network.metricsEyebrow")} title={t("network.metricsTitle")} subtitle={t("network.metricsSubtitle")} />
      <dl className="grid gap-x-6 gap-y-4 px-5 py-4 sm:grid-cols-4">
        <KeyValue label={t("network.metricsNodes")}><Mono>{metrics.nodeCount}</Mono></KeyValue>
        <KeyValue label={t("network.metricsLatency")}><Mono>{metrics.averageLatencyMs == null ? t("network.notAvailable") : `${metrics.averageLatencyMs.toFixed(1)} ms`}</Mono></KeyValue>
        <KeyValue label={t("network.metricsVram")}><Mono>{metrics.vramTotalMb == null ? t("network.notAvailable") : `${(metrics.vramTotalMb / 1024).toFixed(1)} GB`}</Mono></KeyValue>
        <KeyValue label={t("network.metricsModels")}><span className="break-words">{metrics.models.length ? metrics.models.join(", ") : t("network.notAvailable")}</span></KeyValue>
      </dl>
    </Panel>
  );
}

/**
 * Em que degrau a travessia de NAT parou. Dois nós atrás de NAT que não se
 * enxergam falham em um de três lugares - o AutoNAT nunca confirmou um
 * endereço, nenhum relé aceitou reserva, ou o hole punch falhou - e antes
 * deste painel a única forma de saber qual era ler o log de depuração na
 * máquina certa no momento certo.
 */
function TraversalPanel({ traversal, t }: { traversal: TraversalView; t: (key: string, values?: Record<string, string | number>) => string }) {
  const natTone = traversal.nat === "public" ? "aqua" : traversal.nat === "private" ? "amber" : "neutral";
  const circuits = traversal.addrs.filter((addr) => addr.includes("/p2p-circuit"));
  const direct = traversal.addrs.filter((addr) => !addr.includes("/p2p-circuit"));
  return (
    <Panel>
      <PanelHeader eyebrow={t("network.traversalEyebrow")} title={t("network.traversalTitle")} subtitle={t("network.traversalSubtitle")} />
      <dl className="grid gap-x-6 gap-y-4 px-5 py-4 sm:grid-cols-3">
        <KeyValue label={t("network.traversalNat")}>
          <Badge tone={natTone}>{t(`network.traversalNat.${traversal.nat}`)}</Badge>
        </KeyValue>
        <KeyValue label={t("network.traversalReservations")}>
          <Mono>{traversal.reservations.length}</Mono>
        </KeyValue>
        <KeyValue label={t("network.traversalHolePunch")}>
          <Mono>{t("network.traversalHolePunchCounts", { ok: traversal.hole_punch_ok, failed: traversal.hole_punch_failed })}</Mono>
        </KeyValue>
      </dl>
      <dl className="grid gap-x-6 gap-y-4 border-t border-line px-5 py-4">
        <KeyValue label={t("network.traversalDirect")}>
          {direct.length ? <div className="space-y-0.5">{direct.map((addr) => <p key={addr} className="break-all font-mono text-[0.7rem] text-muted">{addr}</p>)}</div> : <span className="text-faint">{t("network.notAvailable")}</span>}
        </KeyValue>
        <KeyValue label={t("network.traversalCircuits")}>
          {circuits.length ? <div className="space-y-0.5">{circuits.map((addr) => <p key={addr} className="break-all font-mono text-[0.7rem] text-muted">{addr}</p>)}</div> : <span className="text-faint">{t("network.notAvailable")}</span>}
        </KeyValue>
      </dl>
      {traversal.last_hole_punch_error && (
        <Footnote className="border-t border-line px-5 py-3" icon={<ShieldAlert className="h-3.5 w-3.5" />}>
          {t("network.traversalLastError", { error: traversal.last_hole_punch_error })}
        </Footnote>
      )}
    </Panel>
  );
}

/**
 * Rota em uso com um peer: direta (HTTP ou P2P, esta às vezes por hole punch)
 * ou pelo relé, quando o hole punch não fechou. É o que o operador precisa ver;
 * o endereço anunciado fica no `title` da linha.
 */
function RouteBadge({ route, t }: { route: PeerRoute | null | undefined; t: (key: string, values?: Record<string, string | number>) => string }) {
  switch (route?.kind) {
    case "http":
      return <Badge tone="aqua">{t("network.routeDirect")}</Badge>;
    case "p2p_direct":
      return <Badge tone="aqua">{t(route.hole_punched ? "network.routeHolePunched" : "network.routeP2pDirect")}</Badge>;
    case "relayed":
      return <Badge tone="amber">{t("network.routeRelayed")}</Badge>;
    default:
      return <Badge tone="neutral">{t("network.routeNone")}</Badge>;
  }
}

function routeAddress(route: PeerRoute | null | undefined, t: (key: string, values?: Record<string, string | number>) => string): string {
  if (!route?.address) return t("network.noAddress");
  if (route.kind !== "relayed") return route.address;
  return t(route.relay_kind === "member" ? "network.routeViaMemberRelay" : "network.routeViaRelay", { address: route.address });
}

function ParticipantsPanel({ nodes, cluster, isAdmin, setPending, setBlocked, t }: { nodes: ReturnType<typeof useAdminNodes>; cluster: ClusterView | undefined; isAdmin: boolean; setPending: (value: { id: string; hostname: string } | null) => void; setBlocked: (id: string, blocked: boolean) => Promise<void>; t: (key: string, values?: Record<string, string | number>) => string }) {
  const systemOf = (id: string) => cluster?.peers.find((peer) => peer.node_id === id)?.node?.system ?? null;
  return (
    <>
      {nodes.isError && <ErrorState title={t("network.listFailed")} error={nodes.error} onRetry={() => void nodes.refetch()} />}
      {nodes.isLoading && <Skeleton className="h-48 w-full" />}
      {nodes.data && <Panel>
        <PanelHeader title={t("network.knownCount", { count: nodes.data.length })} subtitle={t("network.knownDetail")} />
        {nodes.data.length === 0 ? <Empty icon={<Network className="h-5 w-5" />} title={t("network.noNodes")}>{t("network.noNodesDetail")}</Empty> : <div className="divide-y divide-line">{nodes.data.map((n: AdminNode) => <div key={n.node_id} className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3.5"><MachineIcon system={systemOf(n.node_id)} size="sm" /><div className="min-w-[12rem] flex-1"><div className="flex flex-wrap items-center gap-2"><span className="font-medium text-fg">{n.hostname || n.short_id}</span>{n.is_self && <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">{t("network.here")}</Badge>}{n.blocked && <Badge tone="rose">{t("network.blockedLabel")}</Badge>}{n.verified && <Badge tone="aqua">{t("network.verified")}</Badge>}{!n.is_self && <RouteBadge route={n.route} t={t} />}</div><p className="mt-0.5 font-mono text-[0.7rem] text-faint" title={n.addr ? t("network.routeAdvertised", { addr: n.addr }) : undefined}>{n.short_id} · {n.is_self ? n.addr || t("network.noAddress") : routeAddress(n.route, t)}</p></div>{n.total_layers ? <Mono className="shrink-0 text-iris">{layerRange(n.start_layer ?? 0, n.end_layer ?? 0)}</Mono> : null}<span className="flex shrink-0 items-center gap-1.5 text-[0.78rem] text-muted"><Dot tone={n.connected ? "aqua" : "neutral"} />{n.connected ? t("network.connected") : t("network.away")}</span>{isAdmin && !n.is_self && <Button size="sm" variant={n.blocked ? "outline" : "danger"} icon={n.blocked ? <Undo2 className="h-3.5 w-3.5" /> : <Ban className="h-3.5 w-3.5" />} onClick={() => n.blocked ? void setBlocked(n.node_id, false) : setPending({ id: n.node_id, hostname: n.hostname || n.short_id })}>{n.blocked ? t("network.unblock") : t("network.block")}</Button>}</div>)}</div>}
        <Footnote className="border-t border-line px-5 py-3" icon={<ShieldOff className="h-3.5 w-3.5" />}>{t("network.identityNote")}</Footnote>
      </Panel>}
    </>
  );
}

function NetworkInfoPanel({
  info,
  draft,
  editable,
  saving,
  forgetting,
  geo,
  onChange,
  onSave,
  onForgetNetwork,
  t,
}: {
  info: NetworkInfoView | undefined;
  draft: NetworkSettingsData;
  editable: boolean;
  saving: boolean;
  forgetting: boolean;
  geo: GeoReferenceResponse | undefined;
  onChange: (next: NetworkSettingsData) => void;
  onSave: () => void;
  onForgetNetwork: () => void;
  t: (key: string, values?: Record<string, string | number>) => string;
}) {
  const update = (patch: Partial<NetworkSettingsData>) => onChange({ ...draft, ...patch });
  const publicNetwork = draft.visibility === "public";
  const selectedCountry = geo?.countries.find((country) => country.code === draft.country_code);
  const selectedCities = selectedCountry?.cities ?? [];
  const currentCountryOption = draft.country_code && !selectedCountry ? <option value={draft.country_code}>{draft.country_name || t("network.notProvided")}</option> : null;
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("network.networkInfoEyebrow")}
        title={t("network.networkInfoTitle")}
        subtitle={editable ? t("network.networkInfoEditSubtitle") : t("network.networkInfoReadOnlySubtitle")}
        actions={
          <>
            <Button
              variant="danger"
              icon={<Trash2 className="h-3.5 w-3.5" />}
              loading={forgetting}
              onClick={onForgetNetwork}
            >
              {t("network.forgetNetwork")}
            </Button>
            {editable ? <Button variant="primary" icon={<Save className="h-3.5 w-3.5" />} loading={saving} onClick={onSave}>{t("network.infoSave")}</Button> : <Badge tone="neutral">{t("network.readOnly")}</Badge>}
          </>
        }
      />
      {editable ? (
        <div className="grid min-w-0 gap-4 px-5 py-5 md:grid-cols-2">
          <Field label={t("network.publicName")} hint={publicNetwork ? t("network.publicNameRequired") : undefined}>
            <Input required={publicNetwork} value={draft.public_name} onChange={(event) => update({ public_name: event.target.value })} />
          </Field>
          <Field label={t("network.organization")}>
            <Input value={draft.organization} onChange={(event) => update({ organization: event.target.value })} />
          </Field>
          <Field label={t("network.description")} className="md:col-span-2">
            <MarkdownField value={draft.description} onChange={(description) => update({ description })} previewTestId="markdown-preview-description" previewLabel={t("network.markdownPreview")} ariaLabel={t("network.description")} />
          </Field>
          <Field label={t("network.rules")} className="md:col-span-2">
            <MarkdownField value={draft.rules} onChange={(rules) => update({ rules })} previewTestId="markdown-preview-rules" previewLabel={t("network.markdownPreview")} ariaLabel={t("network.rules")} />
          </Field>
          <Field label={t("network.channels")} className="md:col-span-2">
            <MarkdownField value={draft.channels} onChange={(channels) => update({ channels })} previewTestId="markdown-preview-channels" previewLabel={t("network.markdownPreview")} ariaLabel={t("network.channels")} />
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
              {/* The open mode is no longer offered. A network already on it
                  keeps showing it, so saving another field does not silently
                  close the network to its current members. */}
              {draft.access_policy === "anyone_with_anchor" && <option value="anyone_with_anchor">{t("network.accessAnyoneWithAnchor")}</option>}
              <option value="local_only">{t("network.accessLocalOnly")}</option>
            </Select>
          </Field>
          <Field label={t("network.acceptedEnrollments")} hint={t("network.acceptedEnrollmentsHint")} className="md:col-span-2">
            <div className="flex flex-wrap gap-x-5 gap-y-2">
              {ENROLLMENT_ROLES.map((role) => (
                <label key={role} className="flex items-center gap-2 text-sm text-fg">
                  <input
                    type="checkbox"
                    checked={draft.accepted_enrollments.includes(role)}
                    onChange={(event) => update({ accepted_enrollments: ENROLLMENT_ROLES.filter((item) => item === role ? event.target.checked : draft.accepted_enrollments.includes(item)) })}
                  />
                  {t(`network.enrollment.${role}`)}
                </label>
              ))}
            </div>
          </Field>
          <Field label={t("network.lobbyChat")} hint={t("network.lobbyChatHint")} className="md:col-span-2">
            <label className="flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                checked={Boolean(draft.lobby_chat)}
                disabled={!publicNetwork}
                onChange={(event) => update({ lobby_chat: event.target.checked })}
              />
              {t("network.lobbyChatEnable")}
            </label>
          </Field>
          <Field label={t("network.publicStats")} hint={t("network.publicStatsHint")} className="md:col-span-2">
            <label className="flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                checked={Boolean(draft.public_stats)}
                disabled={!publicNetwork}
                onChange={(event) => update({ public_stats: event.target.checked })}
              />
              {t("network.publicStatsEnable")}
            </label>
          </Field>
          <Field label={t("network.country")} hint={publicNetwork ? t("network.countryRequired") : undefined}>
            <Select required={publicNetwork} value={draft.country_code} onChange={(event) => {
              const country = geo?.countries.find((item) => item.code === event.target.value);
              const city = country?.cities[0];
              update({ country_code: event.target.value, country_name: country?.name ?? draft.country_name, city: city?.name ?? "", location: city ? { latitude: city.latitude, longitude: city.longitude } : null });
            }}>
              <option value="">{t("network.chooseCountry")}</option>
              {currentCountryOption}
              {(geo?.countries ?? []).map((country) => <option key={country.code} value={country.code}>{country.name}</option>)}
            </Select>
          </Field>
          <Field label={t("network.city")} hint={publicNetwork ? t("network.cityRequired") : undefined}>
            <Select required={publicNetwork} value={draft.city} onChange={(event) => {
              const city = selectedCities.find((item) => item.name === event.target.value);
              update({ city: event.target.value, location: city ? { latitude: city.latitude, longitude: city.longitude } : draft.location });
            }}>
              <option value="">{t("network.chooseCity")}</option>
              {draft.city && !selectedCities.some((city) => city.name === draft.city) && <option value={draft.city}>{draft.city}</option>}
              {selectedCities.map((city) => <option key={city.id} value={city.name}>{city.name}</option>)}
            </Select>
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
          <KeyValue label={t("network.description")}>{draft.description ? <Markdown>{draft.description}</Markdown> : <span>{t("network.notProvided")}</span>}</KeyValue>
          <KeyValue label={t("network.rules")}>{draft.rules ? <Markdown>{draft.rules}</Markdown> : <span>{t("network.notProvided")}</span>}</KeyValue>
          <KeyValue label={t("network.channels")}>{draft.channels ? <Markdown>{draft.channels}</Markdown> : <span>{t("network.notProvided")}</span>}</KeyValue>
          <KeyValue label={t("network.visibility")}><span>{visibilityLabel(draft.visibility, t)}</span></KeyValue>
          <KeyValue label={t("network.accessPolicy")}><span>{accessLabel(draft.access_policy, t)}</span></KeyValue>
          <KeyValue label={t("network.acceptedEnrollments")}><span>{acceptedEnrollmentsLabel(draft.accepted_enrollments, t)}</span></KeyValue>
          <KeyValue label={t("network.publicStats")}><span>{t(draft.public_stats && publicNetwork ? "network.publicStatsOn" : "network.publicStatsOff")}</span></KeyValue>
          <KeyValue label={t("network.lobbyChat")}><span>{t(draft.lobby_chat && publicNetwork ? "network.lobbyChatOn" : "network.lobbyChatOff")}</span></KeyValue>
          <KeyValue label={t("network.country")}><span>{draft.country_name || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.city")}><span>{draft.city || t("network.notProvided")}</span></KeyValue>
          {draft.location && <KeyValue label={t("network.location")}><span>{t("network.locationCoordinates", { latitude: draft.location.latitude, longitude: draft.location.longitude })}</span></KeyValue>}
        </dl>
      )}
      {info?.network.visibility === "public" && <Footnote className="border-t border-line px-5 py-3" icon={<Radio className="h-3.5 w-3.5" />}>{t("network.publicInfoPropagation")}</Footnote>}
    </Panel>
  );
}

function visibilityLabel(value: NetworkSettingsData["visibility"], t: (key: string, values?: Record<string, string | number>) => string) {
  return t(`network.visibility${value[0].toUpperCase()}${value.slice(1)}`);
}

/** "Not accepting new members" for an empty list, the role names otherwise. */
export function acceptedEnrollmentsLabel(roles: EnrollmentRole[], t: (key: string) => string) {
  return roles.length === 0 ? t("network.enrollmentClosed") : roles.map((role) => t(`network.enrollment.${role}`)).join(", ");
}

function accessLabel(value: NetworkSettingsData["access_policy"], t: (key: string, values?: Record<string, string | number>) => string) {
  return t(`network.access${value === "anyone_with_anchor" ? "AnyoneWithAnchor" : value === "local_only" ? "LocalOnly" : "Authorized"}`);
}
