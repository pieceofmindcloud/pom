/**
 * The network as an administrator sees it: who is connected, who is blocked,
 * and whether this machine is even taking part.
 */
import {
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
import { ClusterVersion } from "../components/ClusterVersion";
import { Confirm } from "../components/Confirm";
import { formatClusterVersion } from "../lib/clusterVersion";
import { MachineIcon } from "../components/MachineIcon";
import { Markdown } from "../components/Markdown";
import { AccessFields, DiscoveryChoices, InfoFields, SetupSection } from "../components/NetworkQuickSetup";
import { RelayOptions } from "../components/RelayOptions";
import {
  Badge,
  Button,
  Dot,
  Empty,
  ErrorState,
  Footnote,
  KeyValue,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { layerRange } from "../lib/format";
import { useAdminNodes, useCluster, useInvalidate, useNetworkInfo, useNode, useWindowsFirewall } from "../lib/queries";
import { LocationEditor } from "../components/LocationEditor";
import { NETWORK_GEO, placeLabel } from "../lib/geo";
import { networkStats } from "../lib/networkStats";
import { admissionsApi, useAdmissions } from "../lib/admissions";
import { useToast } from "../lib/toast";
import type { AdminNode, ClusterView, EnrollmentRole, NetworkInfoView, NetworkSettings as NetworkSettingsData, PeerRoute, TraversalView, WindowsFirewallStatus } from "../lib/types";
import { useI18n } from "../i18n/provider";

export function NetworkSettings({ participantsOnly = false }: { participantsOnly?: boolean }) {
  const { t } = useI18n();
  const node = useNode();
  const cluster = useCluster();
  const networkInfo = useNetworkInfo(Boolean(node.data));
  const firewall = useWindowsFirewall(Boolean(node.data));
  const isInfer = node.data?.profile === "visitor";
  /*
   * Reading the network's picture belongs to `node` and `admin`; blocking and
   * unblocking a machine remains an administrator's act, and only with the grant -
   * `profile: admin` alone grants no capability. Offering the button to someone
   * who will get a 403 is the menu defect this codebase already names.
   */
  // Members, admissions and settings are the principal's: a secondary admin
  // reads this screen as a node does.
  const isAdmin = node.data?.authority_level === "primary";
  const networkEditable = networkInfo.data?.can_edit ?? isAdmin;
  const nodes = useAdminNodes(participantsOnly && node.data !== undefined && !isInfer);
  const toast = useToast();
  const invalidate = useInvalidate();

  const [pending, setPending] = useState<{ id: string; hostname: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [forgetNetworkConfirm, setForgetNetworkConfirm] = useState(false);
  const [forgettingNetwork, setForgettingNetwork] = useState(false);

  const [networkDraft, setNetworkDraft] = useState<NetworkSettingsData | null>(null);
  const [networkSaving, setNetworkSaving] = useState(false);
  const [firewallBusy, setFirewallBusy] = useState(false);

  useEffect(() => {
    if (!networkInfo.data || (networkDraft && networkEditable)) return;
    setNetworkDraft(networkInfo.data.network);
  }, [networkDraft, networkEditable, networkInfo.data]);

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
        <ParticipantsPanel nodes={nodes} cluster={cluster.data} isAdmin={isAdmin} devMode={node.data?.pom_dev_mode === true} setPending={setPending} setBlocked={setBlocked} t={t} />
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
            editable={networkEditable}
            saving={networkSaving}
            forgetting={forgettingNetwork}
            onChange={setNetworkDraft}
            onSave={() => void saveNetworkInfo()}
            // A machine's own network is inherent to it and cannot be left;
            // leaving is for a network it joined, and returns to its own.
            onForgetNetwork={node.data?.own_network && !node.data?.standby_network ? undefined : () => setForgetNetworkConfirm(true)}
            t={t}
          />
        )}

        {/* Whoever cannot edit the network still decides how this machine
            relays; the editor draws it as the form's last question. */}
        {networkDraft && !networkEditable && <RelayOptions />}

        {isAdmin && <AdmittedMachines t={t} />}

        <ClusterVersion />

        <NetworkMetricsPanel metrics={metrics} t={t} />

        {cluster.data?.traversal && <TraversalPanel traversal={cluster.data.traversal} t={t} />}

        {/* An inference node does not become a network node in place: the
            role is what the network admitted it as, so changing it means
            leaving and asking to be admitted again with the other role. */}
        {isInfer && (
          <Panel>
            <PanelHeader
              eyebrow={t("network.profile")}
              title={t("profile.visitor")}
              subtitle={t("network.inferRequestDetail")}
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
 * On which rung the NAT traversal stopped. Two nodes behind NAT that cannot
 * see each other fail in one of three places - AutoNAT never confirmed an
 * address, no relay accepted a reservation, or the hole punch failed - and
 * before this panel the only way to know which was to read the debug log on
 * the right machine at the right moment.
 */
function TraversalPanel({ traversal, t }: { traversal: TraversalView; t: (key: string, values?: Record<string, string | number>) => string }) {
  const natTone = traversal.nat === "public" ? "aqua" : traversal.nat === "private" ? "amber" : "neutral";
  const isRelay = (addr: string) => addr.startsWith("https://") || addr.startsWith("http://");
  const circuits = traversal.addrs.filter(isRelay);
  const direct = traversal.addrs.filter((addr) => !isRelay(addr));
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
 * Route in use with a peer: direct (HTTP or P2P, the latter sometimes by hole
 * punch) or through the relay, when the hole punch did not close. It is what the
 * operator needs to see; the announced address stays in the row's `title`.
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

/**
 * The POM release a participant runs, amber when it is not this node's: a
 * machine on another release may speak another protocol. The build commit
 * shows only in development mode.
 */
function PomVersionBadge({ node, ownVersion, devMode, t }: { node: AdminNode; ownVersion: string | undefined; devMode: boolean; t: (key: string, values?: Record<string, string | number>) => string }) {
  if (!node.connected) return null;
  if (!node.pom_version) {
    return <Badge tone="neutral" data-testid="participant-version">{t("network.pomVersionUnknown")}</Badge>;
  }
  const differs = Boolean(ownVersion) && node.pom_version !== ownVersion;
  return (
    <span title={differs ? t("network.pomVersionDiffers", { version: ownVersion ?? "" }) : t("network.pomVersionSame")}>
      <Badge tone={differs ? "amber" : "neutral"} mono data-testid="participant-version">
        {node.pom_version}
        {devMode && node.pom_build ? ` · ${node.pom_build}` : null}
      </Badge>
    </span>
  );
}

function ParticipantsPanel({ nodes, cluster, isAdmin, devMode, setPending, setBlocked, t }: { nodes: ReturnType<typeof useAdminNodes>; cluster: ClusterView | undefined; isAdmin: boolean; devMode: boolean; setPending: (value: { id: string; hostname: string } | null) => void; setBlocked: (id: string, blocked: boolean) => Promise<void>; t: (key: string, values?: Record<string, string | number>) => string }) {
  // The version this node holds; a machine showing another one is out of step.
  const ownVersion = nodes.data?.find((n) => n.is_self)?.trust_version;
  const ownPomVersion = nodes.data?.find((n) => n.is_self)?.pom_version;
  const systemOf = (id: string) => cluster?.peers.find((peer) => peer.node_id === id)?.node?.system ?? null;
  return (
    <>
      {nodes.isError && <ErrorState title={t("network.listFailed")} error={nodes.error} onRetry={() => void nodes.refetch()} />}
      {nodes.isLoading && <Skeleton className="h-48 w-full" />}
      {nodes.data && <Panel>
        <PanelHeader title={t("network.knownCount", { count: nodes.data.length })} subtitle={t("network.knownDetail")} />
        {nodes.data.length === 0 ? <Empty icon={<Network className="h-5 w-5" />} title={t("network.noNodes")}>{t("network.noNodesDetail")}</Empty> : <div className="divide-y divide-line">{nodes.data.map((n: AdminNode) => <div key={n.node_id} className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3.5"><MachineIcon system={systemOf(n.node_id)} size="sm" /><div className="min-w-[12rem] flex-1"><div className="flex flex-wrap items-center gap-2"><span className="font-medium text-fg">{n.hostname || n.short_id}</span>{n.is_self && <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">{t("network.here")}</Badge>}{n.blocked && <Badge tone="rose">{t("network.blockedLabel")}</Badge>}{n.verified && <Badge tone="aqua">{t("network.verified")}</Badge>}<PomVersionBadge node={n} ownVersion={ownPomVersion} devMode={devMode} t={t} />{!n.is_self && <RouteBadge route={n.route} t={t} />}</div><p className="mt-0.5 font-mono text-[0.7rem] text-faint" title={n.addr ? t("network.routeAdvertised", { addr: n.addr }) : undefined}>{n.short_id} · {n.is_self ? n.addr || t("network.noAddress") : routeAddress(n.route, t)}{n.trust_version ? <span className={n.trust_version !== ownVersion ? "text-amber" : undefined} title={t(n.trust_version !== ownVersion ? "network.clusterVersionDiffers" : "network.clusterVersionSame")}> · {t("network.clusterVersionShort", { version: formatClusterVersion(n.trust_version) })}</span> : null}</p></div>{n.total_layers ? <Mono className="shrink-0 text-iris">{layerRange(n.start_layer ?? 0, n.end_layer ?? 0)}</Mono> : null}<span className="flex shrink-0 items-center gap-1.5 text-[0.78rem] text-muted"><Dot tone={n.connected ? "aqua" : "neutral"} />{n.connected ? t("network.connected") : t("network.away")}</span>{isAdmin && !n.is_self && <Button size="sm" variant={n.blocked ? "outline" : "danger"} icon={n.blocked ? <Undo2 className="h-3.5 w-3.5" /> : <Ban className="h-3.5 w-3.5" />} onClick={() => n.blocked ? void setBlocked(n.node_id, false) : setPending({ id: n.node_id, hostname: n.hostname || n.short_id })}>{n.blocked ? t("network.unblock") : t("network.block")}</Button>}</div>)}</div>}
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
  onChange: (next: NetworkSettingsData) => void;
  onSave: () => void;
  onForgetNetwork?: () => void;
  t: (key: string, values?: Record<string, string | number>) => string;
}) {
  const update = (patch: Partial<NetworkSettingsData>) => onChange({ ...draft, ...patch });
  const publicNetwork = draft.visibility === "public";
  const discoverable = draft.visibility !== "local";
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("network.networkInfoEyebrow")}
        title={t("network.networkInfoTitle")}
        subtitle={editable ? t("network.networkInfoEditSubtitle") : t("network.networkInfoReadOnlySubtitle")}
        actions={
          <>
            {onForgetNetwork && (
              <Button
                variant="danger"
                icon={<Trash2 className="h-3.5 w-3.5" />}
                loading={forgetting}
                onClick={onForgetNetwork}
              >
                {t("network.forgetNetwork")}
              </Button>
            )}
            {editable ? <Button variant="primary" icon={<Save className="h-3.5 w-3.5" />} loading={saving} onClick={onSave}>{t("network.infoSave")}</Button> : <Badge tone="neutral">{t("network.readOnly")}</Badge>}
          </>
        }
      />
      {editable ? (
        /* The same questions, in the same order and drawing, as the quick
           setup of the Networks screen: one identity for both. A question a
           discovery choice makes meaningless is not drawn. */
        <div className="min-w-0 space-y-7 px-5 py-5">
          <SetupSection step="discovery" number={1} t={t}><DiscoveryChoices draft={draft} update={update} t={t} /></SetupSection>
          <SetupSection step="info" number={2} t={t}><InfoFields draft={draft} update={update} t={t} /></SetupSection>
          {discoverable && <SetupSection step="access" number={3} t={t}><AccessFields draft={draft} update={update} t={t} full /></SetupSection>}
          {publicNetwork && (
            <SetupSection step="location" number={4} t={t}>
              <LocationEditor value={draft} source={NETWORK_GEO} required t={t} onChange={(place) => update(place)} />
              <div className="mt-4 min-w-0"><KeyValue label={t("network.geoLabel")}><Mono className="break-all">{draft.geo_label || t("network.notPublished")}</Mono></KeyValue></div>
            </SetupSection>
          )}
          {/* This machine's own setting, applied as soon as it changes. */}
          <RelayOptions section={(content) => <SetupSection step="relay" number={2 + (discoverable ? 1 : 0) + (publicNetwork ? 1 : 0) + 1} t={t}>{content}</SetupSection>} />
        </div>
      ) : (
        <dl className="grid min-w-0 gap-x-6 gap-y-4 px-5 py-5 sm:grid-cols-2">
          <KeyValue label={t("network.publicName")}><span className="break-words">{draft.public_name || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.organization")}><span className="break-words">{draft.organization || t("network.notProvided")}</span></KeyValue>
          <KeyValue label={t("network.description")}>{draft.description ? <Markdown>{draft.description}</Markdown> : <span>{t("network.notProvided")}</span>}</KeyValue>
          <KeyValue label={t("network.rules")}>{draft.rules ? <Markdown>{draft.rules}</Markdown> : <span>{t("network.notProvided")}</span>}</KeyValue>
          <KeyValue label={t("network.channels")}>{draft.channels ? <Markdown>{draft.channels}</Markdown> : <span>{t("network.notProvided")}</span>}</KeyValue>
          <KeyValue label={t("network.visibility")}><span>{visibilityLabel(draft.visibility, t)}</span></KeyValue>
          {discoverable && <KeyValue label={t("network.accessPolicy")}><span>{accessLabel(draft.access_policy, t)}</span></KeyValue>}
          {discoverable && <KeyValue label={t("network.acceptedEnrollments")}><span>{acceptedEnrollmentsLabel(draft.accepted_enrollments, t)}</span></KeyValue>}
          {publicNetwork && <KeyValue label={t("network.publicStats")}><span>{t(draft.public_stats ? "network.publicStatsOn" : "network.publicStatsOff")}</span></KeyValue>}
          {publicNetwork && <KeyValue label={t("location.title")}><span className="break-words">{placeLabel(draft) || t("network.notProvided")}</span></KeyValue>}
          {publicNetwork && draft.location && <KeyValue label={t("network.location")}><span>{t("network.locationCoordinates", { latitude: draft.location.latitude, longitude: draft.location.longitude })}</span></KeyValue>}
        </dl>
      )}
      {info?.network.visibility === "public" && <Footnote className="border-t border-line px-5 py-3" icon={<Radio className="h-3.5 w-3.5" />}>{t("network.publicInfoPropagation")}</Footnote>}
    </Panel>
  );
}

/**
 * The machines this network admitted: its member list, kept by the principal.
 * Revoking one takes it off the list and gives the other members a new
 * connection key, so it has to ask to be admitted again.
 */
export function AdmittedMachines({ t }: { t: (key: string, values?: Record<string, string | number>) => string }) {
  const admissions = useAdmissions(true);
  const toast = useToast();
  const [revoking, setRevoking] = useState<string | null>(null);
  const [target, setTarget] = useState<{ node_id: string; name: string } | null>(null);
  const admitted = (admissions.data ?? []).filter((record) => record.status === "approved");
  if (admitted.length === 0) return null;

  async function revoke(nodeId: string) {
    setRevoking(nodeId);
    try {
      await admissionsApi.decide(nodeId, "forget");
      toast.success(t("network.admittedRevoked"));
      await admissions.refetch();
    } catch (e) {
      toast.fromError(t("network.admittedRevokeFailed"), e);
    } finally {
      setRevoking(null);
      setTarget(null);
    }
  }

  return (
    <Panel>
      <PanelHeader eyebrow={t("network.networkInfoEyebrow")} title={t("network.admittedTitle")} subtitle={t("network.admittedSubtitle")} />
      <ul className="divide-y divide-line" data-testid="admitted-machines">
        {admitted.map((record) => {
          const name = record.card.hostname || record.node_id.slice(0, 12);
          return (
            <li key={record.node_id} className="flex flex-wrap items-center justify-between gap-3 px-5 py-3">
              <div className="min-w-0">
                <p className="truncate text-sm text-fg">{name}</p>
                <p className="truncate font-mono text-[0.68rem] text-faint">{record.node_id.slice(0, 12)} · {t(`network.enrollment.${record.role}`)}</p>
              </div>
              <Button size="sm" variant="danger" loading={revoking === record.node_id} onClick={() => setTarget({ node_id: record.node_id, name })}>
                {t("network.admittedRevoke")}
              </Button>
            </li>
          );
        })}
      </ul>
      <Confirm
        open={target !== null}
        title={t("network.admittedRevokeTitle", { name: target?.name ?? "" })}
        description={t("network.admittedRevokeDetail")}
        confirmLabel={t("network.admittedRevoke")}
        tone="danger"
        busy={revoking !== null}
        onConfirm={() => target && void revoke(target.node_id)}
        onCancel={() => revoking === null && setTarget(null)}
      />
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
  return t(`network.access${value === "anyone_with_anchor" ? "AnyoneWithAnchor" : value === "auto_approve" ? "AutoApprove" : value === "local_only" ? "LocalOnly" : "Authorized"}`);
}
