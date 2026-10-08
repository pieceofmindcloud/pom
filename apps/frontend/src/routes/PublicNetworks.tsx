import { useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { CircleCheck, CircleHelp, CircleX, Globe2, LoaderCircle, LogOut, MessageCircle, RefreshCw, ShieldCheck, Waypoints } from "lucide-react";
import { Confirm, Modal } from "../components/Confirm";
import { LobbyChat } from "../components/LobbyChat";
import { Markdown } from "../components/Markdown";
import { Button, ErrorState, PageHeader, Select, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { networkStats, type NetworkStats } from "../lib/networkStats";
import { usePublicLocator, type PublicLocator } from "../lib/publicLocator";
import { usePublicStats } from "../lib/publicStats";
import { useCluster, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import type { EnrollmentRole } from "../lib/types";

interface PublicNetwork {
  cluster_id: string;
  name: string;
  fingerprint: string;
  /** Chave `pk:` do principal que publica a rede. A rede é achada por ela, nunca por IP. */
  publisher_key: string;
  relay_hints?: string[];
  trust_document: string;
  organization: string;
  description: string;
  rules: string;
  channels: string;
  country_code: string;
  country_name: string;
  city: string;
  geo_label: string;
  access_policy: "authorized" | "anyone_with_anchor" | "local_only";
  /** Roles the network advertises as open; empty means it takes no new members. */
  accepted_enrollments?: EnrollmentRole[];
  /** The network answers lobby chat from machines that are not members. */
  lobby_chat?: boolean;
  /** The network publishes models, node count and VRAM to non-members. */
  public_stats?: boolean;
  location: { latitude: number; longitude: number; accuracy_m?: number } | null;
  discovered_via: string;
  directory_replicas?: string[];
}

interface ViewerLocation {
  latitude: number;
  longitude: number;
}

export function PublicNetworks() {
  const { t } = useI18n();
  const toast = useToast();
  const cluster = useCluster();
  const node = useNode();
  const [networks, setNetworks] = useState<PublicNetwork[] | null>(null);
  const [localNetwork, setLocalNetwork] = useState<PublicNetwork | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [viewerLocation, setViewerLocation] = useState<ViewerLocation | null>(null);
  const [detailsTarget, setDetailsTarget] = useState<PublicNetwork | null>(null);
  const [chatTarget, setChatTarget] = useState<PublicNetwork | null>(null);
  const [leaveConfirm, setLeaveConfirm] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const [countryFilter, setCountryFilter] = useState("");
  const [cityFilter, setCityFilter] = useState("");
  const [reachabilityRefreshKey, setReachabilityRefreshKey] = useState(0);

  async function load() {
    setRefreshing(true);
    setError(null);
    setReachabilityRefreshKey((value) => value + 1);
    try {
      const discovered = await api.get<PublicNetwork[]>("/api/public/networks", { auth: false });
      setNetworks(discovered);
      void api.get<PublicNetwork>("/api/public/network", { auth: false })
        .then(setLocalNetwork)
        .catch(() => setLocalNetwork(null));
    } catch (reason) {
      setError(reason);
    } finally {
      setRefreshing(false);
    }
  }

  useEffect(() => {
    void load();
    if (navigator.geolocation) {
      navigator.geolocation.getCurrentPosition(
        (position) => setViewerLocation({ latitude: position.coords.latitude, longitude: position.coords.longitude }),
        () => undefined,
        { enableHighAccuracy: false, maximumAge: 300000, timeout: 8000 },
      );
    }
  }, []);

  const countryOptions = useMemo(
    () => Array.from(
      new Map(
        (networks ?? [])
          .filter((network) => network.country_code || network.country_name)
          .map((network) => [
            countryKey(network),
            network.country_name || network.country_code,
          ]),
      ),
    ).sort((left, right) => left[1].localeCompare(right[1])),
    [networks],
  );
  const cityOptions = useMemo(
    () => Array.from(new Set(
      (networks ?? [])
        .filter((network) => !countryFilter || countryKey(network) === countryFilter)
        .map((network) => network.city)
        .filter(Boolean),
    )).sort((left, right) => left.localeCompare(right)),
    [countryFilter, networks],
  );
  const selectedCity = cityOptions.includes(cityFilter) ? cityFilter : "";
  const filteredNetworks = useMemo(
    () => (networks ?? []).filter((network) =>
      (!countryFilter || countryKey(network) === countryFilter)
      && (!selectedCity || network.city === selectedCity),
    ),
    [countryFilter, networks, selectedCity],
  );
  const orderedNetworks = useMemo(
    () => [...filteredNetworks].sort((left, right) => {
      const leftDistance = distanceKm(left.location, viewerLocation);
      const rightDistance = distanceKm(right.location, viewerLocation);
      if (leftDistance === null && rightDistance === null) return 0;
      if (leftDistance === null) return 1;
      if (rightDistance === null) return -1;
      return leftDistance - rightDistance;
    }),
    [filteredNetworks, viewerLocation],
  );
  const metrics = networkStats(cluster.data);
  /*
   * The row of the network this node already belongs to never offers Join.
   * A member gets Leave; the principal gets no action at all, because the
   * network is published under its key and leaving would orphan it.
   */
  const isPrincipal = node.data?.authority_level === "primary";

  async function leave() {
    setLeaving(true);
    try {
      await api.post("/api/ui/network/leave");
      toast.success(t("network.forgetNetworkStarted"), t("network.forgetNetworkStartedDetail"));
      window.setTimeout(() => window.location.assign("/admin-ui/"), 1500);
    } catch (reason) {
      toast.fromError(t("network.forgetNetworkFailed"), reason);
      setLeaving(false);
      setLeaveConfirm(false);
    }
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <PageHeader
        title={t("publicNetworks.title")}
        subtitle={t("publicNetworks.subtitle")}
        className="!mb-0 border-b border-line bg-surface/75 px-3 py-3 sm:px-4"
        actions={<Button size="sm" loading={refreshing} onClick={() => void load()} icon={<RefreshCw className="h-3.5 w-3.5" />}>{t("publicNetworks.refresh")}</Button>}
      />
      <div className="flex min-h-0 flex-1 flex-col overflow-hidden border-y border-line bg-surface/35">
        <div className="flex shrink-0 flex-wrap items-end justify-between gap-3 border-b border-line bg-raised/60 px-4 py-3 sm:px-5">
          <div className="flex min-w-0 items-center gap-3">
            <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-iris/35 bg-iris/12 text-iris"><Globe2 className="h-4 w-4" /></span>
            <div className="min-w-0"><h2 className="font-display text-sm font-semibold text-fg">{t("publicNetworks.discoveredTitle")}</h2><p className="mt-0.5 text-xs text-muted">{t("publicNetworks.discoveredDetail")}</p></div>
          </div>
          <div className="flex min-w-0 flex-wrap items-end gap-2">
            <label className="flex w-[10rem] min-w-[8rem] flex-col gap-1">
              <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("publicNetworks.country")}</span>
              <Select aria-label={t("publicNetworks.country")} compact value={countryFilter} onChange={(event) => setCountryFilter(event.target.value)}>
                <option value="">{t("publicNetworks.allCountries")}</option>
                {countryOptions.map(([code, name]) => <option key={code} value={code}>{name}</option>)}
              </Select>
            </label>
            <label className="flex w-[10rem] min-w-[8rem] flex-col gap-1">
              <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("publicNetworks.city")}</span>
              <Select aria-label={t("publicNetworks.city")} compact value={selectedCity} disabled={cityOptions.length === 0} onChange={(event) => setCityFilter(event.target.value)}>
                <option value="">{t("publicNetworks.allCities")}</option>
                {cityOptions.map((city) => <option key={city} value={city}>{city}</option>)}
              </Select>
            </label>
          </div>
        </div>
        {error ? <div className="px-5 py-5"><ErrorState title={t("publicNetworks.failed")} error={error} onRetry={() => void load()} /></div> : networks === null ? <div className="space-y-3 px-5 py-5"><Skeleton className="h-10 w-full" /><Skeleton className="h-10 w-full" /></div> : filteredNetworks.length === 0 ? <div className="px-5 py-12 text-center"><Globe2 className="mx-auto h-7 w-7 text-faint" /><p className="mt-3 text-sm font-medium text-fg">{t(networks.length === 0 ? "publicNetworks.emptyTitle" : "publicNetworks.noMatches")}</p><p className="mx-auto mt-1 max-w-md text-sm leading-relaxed text-muted">{t(networks.length === 0 ? "publicNetworks.emptyDetail" : "publicNetworks.noMatchesDetail")}</p></div> : <div className="min-h-0 flex-1 overflow-auto"><table className="min-w-[1200px] w-full border-collapse text-left text-xs"><thead className="sticky top-0 z-10 bg-raised text-[0.66rem] uppercase tracking-[0.12em] text-faint"><tr>{["name", "organization", "location", "geoLabel", "reach", "nodes", "latency", "vram", "models", "action"].map((column) => <th key={column} className="border-b border-line px-4 py-3 font-medium">{t(`publicNetworks.${column}`)}</th>)}</tr></thead><tbody className="divide-y divide-line">{orderedNetworks.map((network) => { const isLocal = localNetwork?.fingerprint === network.fingerprint; return <NetworkRow key={`${network.cluster_id}:${network.fingerprint}`} network={network} distance={distanceKm(network.location, viewerLocation)} stats={isLocal ? metrics : null} membership={!isLocal ? "none" : isPrincipal ? "principal" : "member"} leaving={leaving} onLeave={() => setLeaveConfirm(true)} onChat={() => setChatTarget(network)} onDetails={() => setDetailsTarget(network)} t={t} refreshKey={reachabilityRefreshKey} />; })}</tbody></table></div>}
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-x-5 gap-y-2 px-1 py-3 text-xs leading-relaxed text-faint"><span>{t("publicNetworks.locationNote")}</span><span className="flex items-center gap-2"><ShieldCheck className="h-3.5 w-3.5 shrink-0" />{t("publicNetworks.securityNote")}</span></div>
      <Confirm open={leaveConfirm} title={t("network.forgetNetworkTitle")} description={t("network.forgetNetworkDescription")} confirmLabel={t("network.leave")} tone="danger" busy={leaving} onConfirm={() => void leave()} onCancel={() => !leaving && setLeaveConfirm(false)} />
      <LobbyChat network={chatTarget && { name: chatTarget.name || chatTarget.cluster_id, publisher_key: chatTarget.publisher_key, accepted_enrollments: chatTarget.accepted_enrollments }} endpoint="/api/ui/network-chat/lobby" onClose={() => setChatTarget(null)} />
      <PublicNetworkDetails network={detailsTarget} onClose={() => setDetailsTarget(null)} t={t} />
    </div>
  );
}

function NetworkRow({ network, distance, stats: localStats, membership, leaving, onLeave, onChat, onDetails, t, refreshKey }: { network: PublicNetwork; distance: number | null; stats: NetworkStats | null; membership: "none" | "member" | "principal"; leaving: boolean; onLeave: () => void; onChat: () => void; onDetails: () => void; t: (key: string, values?: Record<string, string | number>) => string; refreshKey: number }) {
  const unavailable = t("publicNetworks.unavailable");
  const location = [network.country_name || network.country_code, network.city].filter(Boolean).join(" / ") || unavailable;
  const reachability = usePublicLocator(network.publisher_key, refreshKey);
  // The local network is read from this node's own cluster; any other one is
  // asked from its principal, and only when it chose to publish.
  const publishedStats = usePublicStats("/api/ui/network-chat/lobby/info", network.publisher_key, membership === "none" && Boolean(network.public_stats), refreshKey);
  const stats = localStats ?? publishedStats;
  const models = stats?.models.length ? stats.models.join(", ") : unavailable;
  const closed = network.accepted_enrollments?.length === 0;
  return <tr className="align-top transition-colors hover:bg-raised/45"><td className="max-w-[15rem] px-4 py-3.5"><button type="button" className="group text-left" onClick={onDetails} aria-label={t("publicNetworks.openDetails", { name: network.name || network.cluster_id })}><span className="font-medium text-fg group-hover:text-iris">{network.name || network.cluster_id}</span>{distance !== null && <span className="ml-2 whitespace-nowrap text-[0.65rem] text-lime">{t((network.location?.accuracy_m ?? 0) >= 1000 ? "publicNetworks.distanceApproximate" : "publicNetworks.distance", { value: distance.toFixed(0) })}</span>}<p className="mt-1 break-all font-mono text-[0.64rem] text-faint">{network.fingerprint}</p></button></td><td className="max-w-[13rem] px-4 py-3.5 text-muted">{network.organization || unavailable}</td><td className="max-w-[13rem] px-4 py-3.5 text-muted">{location}</td><td className="max-w-[15rem] break-all px-4 py-3.5 font-mono text-[0.68rem] text-muted">{network.geo_label || unavailable}</td><td className="max-w-[18rem] px-4 py-3.5"><ReachabilityIndicator id={network.fingerprint} status={reachability} t={t} /></td><td className="px-4 py-3.5 font-mono text-muted">{stats ? stats.nodeCount : unavailable}</td><td className="px-4 py-3.5 font-mono text-muted">{stats?.averageLatencyMs != null ? `${stats.averageLatencyMs.toFixed(1)} ms` : unavailable}</td><td className="px-4 py-3.5 font-mono text-muted">{stats?.vramTotalMb != null ? `${(stats.vramTotalMb / 1024).toFixed(1)} GB` : unavailable}</td><td className="max-w-[18rem] px-4 py-3.5 text-muted">{models}</td><td className="px-4 py-3.5"><div className="flex flex-wrap items-center gap-2">{membership === "none" ? <Button size="sm" variant="primary" disabled={!network.lobby_chat} title={!network.lobby_chat ? t("publicNetworks.noLobbyDetail") : closed ? t("publicNetworks.closedDetail") : undefined} onClick={(event) => { event.stopPropagation(); onChat(); }} icon={<MessageCircle className="h-3.5 w-3.5" />}>{t(!network.lobby_chat ? "publicNetworks.noLobby" : closed ? "publicNetworks.chat" : "publicNetworks.join")}</Button> : membership === "member" ? <Button size="sm" variant="danger" loading={leaving} disabled={leaving} onClick={(event) => { event.stopPropagation(); onLeave(); }} icon={<LogOut className="h-3.5 w-3.5" />}>{t("network.leave")}</Button> : null}</div></td></tr>;
}

function PublicNetworkDetails({ network, onClose, t }: { network: PublicNetwork | null; onClose: () => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const unavailable = t("publicNetworks.unavailable");
  const value = (text: string) => text || unavailable;
  const location = network?.location ? `${network.location.latitude}, ${network.location.longitude}` : unavailable;
  if (!network) return null;
  return (
    <Modal open title={t("publicNetworks.detailsTitle", { name: network.name || network.cluster_id })} onClose={onClose} size="lg" footer={<Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>}>
      <div className="space-y-5">
        <div className="grid gap-4 sm:grid-cols-2">
          <DetailBlock label={t("network.organization")}><span>{value(network.organization)}</span></DetailBlock>
          <DetailBlock label={t("network.visibility")}><span>{t("network.visibilityPublic")}</span></DetailBlock>
          <DetailBlock label={t("network.accessPolicy")}><span>{accessPolicyLabel(network.access_policy, t)}</span></DetailBlock>
          <DetailBlock label={t("network.acceptedEnrollments")}><span>{acceptedEnrollmentsLabel(network.accepted_enrollments, t)}</span></DetailBlock>
          <DetailBlock label={t("network.country")}><span>{value(network.country_name || network.country_code)}</span></DetailBlock>
          <DetailBlock label={t("network.city")}><span>{value(network.city)}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.fingerprint")}><span className="break-all font-mono text-xs">{value(network.fingerprint)}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.geoLabel")}><span className="break-all font-mono text-xs">{value(network.geo_label)}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.publisherKey")}><span className="break-all font-mono text-xs">{value(network.publisher_key)}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.relayHints")}><span className="break-all font-mono text-xs">{value(network.relay_hints?.join(", ") ?? "")}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.location")}><span>{location}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.discoveredVia")}><span>{value(network.discovered_via)}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.directoryReplicas")}><span className="break-words">{value(network.directory_replicas?.join(", ") ?? "")}</span></DetailBlock>
        </div>
        <div className="grid gap-4 border-t border-line pt-4">
          <DetailBlock label={t("network.description")}><Markdown>{network.description || unavailable}</Markdown></DetailBlock>
          <DetailBlock label={t("network.rules")}><Markdown>{network.rules || unavailable}</Markdown></DetailBlock>
          <DetailBlock label={t("network.channels")}><Markdown>{network.channels || unavailable}</Markdown></DetailBlock>
        </div>
        <details className="border-t border-line pt-4">
          <summary className="cursor-pointer text-sm font-medium text-fg">{t("network.trustDocument")}</summary>
          <pre className="mt-3 max-h-48 overflow-auto rounded-lg border border-line bg-ink p-3 text-[0.68rem] leading-relaxed text-muted">{network.trust_document || unavailable}</pre>
        </details>
      </div>
    </Modal>
  );
}

function DetailBlock({ label, children }: { label: string; children: ReactNode }) {
  return <div className="min-w-0"><div className="eyebrow mb-1">{label}</div><div className="min-w-0 break-words text-sm text-fg">{children}</div></div>;
}

/** A record without the list predates it and is open to every role. */
function acceptedEnrollmentsLabel(roles: EnrollmentRole[] | undefined, t: (key: string) => string) {
  if (!roles) return t("network.enrollmentOpen");
  return roles.length === 0 ? t("network.enrollmentClosed") : roles.map((role) => t(`network.enrollment.${role}`)).join(", ");
}

function accessPolicyLabel(value: PublicNetwork["access_policy"], t: (key: string) => string) {
  return t(value === "anyone_with_anchor" ? "network.accessAnyoneWithAnchor" : value === "local_only" ? "network.accessLocalOnly" : "network.accessAuthorized");
}

function ReachabilityIndicator({ id, status, t }: { id: string; status: PublicLocator; t: (key: string) => string }) {
  const content = status === "direct"
    ? <CircleCheck className="h-4 w-4 shrink-0 text-lime" aria-hidden="true" />
    : status === "relayed"
      ? <Waypoints className="h-4 w-4 shrink-0 text-amber" aria-hidden="true" />
      : status === "unresolved"
        ? <CircleX className="h-4 w-4 shrink-0 text-rose" aria-hidden="true" />
        : status === "unknown"
          ? <CircleHelp className="h-4 w-4 shrink-0 text-faint" aria-hidden="true" />
          : <LoaderCircle className="h-4 w-4 shrink-0 animate-spin text-amber" aria-hidden="true" />;
  const label = status === "direct"
    ? t("publicNetworks.reachDirect")
    : status === "relayed"
      ? t("publicNetworks.reachRelayed")
      : status === "unresolved"
        ? t("publicNetworks.reachUnresolved")
        : status === "unknown"
          ? t("publicNetworks.reachUnknown")
          : t("publicNetworks.checking");
  const textClass = status === "direct"
    ? "text-[0.68rem] text-lime"
    : status === "unresolved"
      ? "text-[0.68rem] text-rose"
      : status === "unknown"
        ? "text-[0.68rem] text-faint"
        : "text-[0.68rem] text-amber";
  return <div data-testid={`public-network-reachability-${id}`} data-reachability={status} className="flex min-w-0 items-start gap-2" title={label}><span className="mt-0.5">{content}</span><span className={textClass}>{label}</span></div>;
}

function distanceKm(location: PublicNetwork["location"], viewer: ViewerLocation | null): number | null {
  if (!location || !viewer) return null;
  const earthRadiusKm = 6371;
  const latitude = (location.latitude - viewer.latitude) * Math.PI / 180;
  const longitude = (location.longitude - viewer.longitude) * Math.PI / 180;
  const a = Math.sin(latitude / 2) ** 2 + Math.cos(viewer.latitude * Math.PI / 180) * Math.cos(location.latitude * Math.PI / 180) * Math.sin(longitude / 2) ** 2;
  return earthRadiusKm * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
}

function countryKey(network: PublicNetwork) {
  return network.country_code || network.country_name;
}
