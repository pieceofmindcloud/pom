import { useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import { CircleCheck, CircleHelp, CircleX, Download, Gauge, Globe2, House, LoaderCircle, LocateFixed, Lock, LogOut, MessageCircle, Plus, RefreshCw, ShieldCheck, Trash2, Waypoints } from "lucide-react";
import { Confirm, Modal } from "../components/Confirm";
import { LobbyChat } from "../components/LobbyChat";
import { Markdown } from "../components/Markdown";
import { Badge, Button, ErrorState, PageHeader, Select, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { networkStats, type NetworkStats } from "../lib/networkStats";
import { usePublicLatencies, type PublicLatency } from "../lib/publicLatency";
import { usePublicLocator, type PublicLocator } from "../lib/publicLocator";
import { usePublicStats } from "../lib/publicStats";
import { useCluster, useNetworkInfo, useNode } from "../lib/queries";
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
  /**
   * Country, state and city are what this node's offline city database says
   * is at the coordinates the network publishes, not text the network sent.
   */
  country_code: string;
  country_name: string;
  state?: string;
  city: string;
  geo_label: string;
  access_policy: "authorized" | "auto_approve" | "anyone_with_anchor" | "local_only";
  /** Roles the network advertises as open; empty means it takes no new members. */
  accepted_enrollments?: EnrollmentRole[];
  /** The network answers lobby chat from machines that are not members. */
  /** The network publishes models, node count and VRAM to non-members. */
  public_stats?: boolean;
  location: { latitude: number; longitude: number; accuracy_m?: number } | null;
  discovered_via: string;
  directory_replicas?: string[];
}

type Visibility = "local" | "private" | "public";

/** A network the node keeps at the top of the list, with how it is announced. */
interface PinnedNetwork {
  network: PublicNetwork;
  visibility: Visibility;
}

interface PinnedNetworks {
  /** The network this machine is in, whatever its visibility. */
  own: PinnedNetwork | null;
  /** Networks imported from a trust document on this machine. */
  custom: PinnedNetwork[];
}

/** Where a row comes from: this machine's network, an imported one, or the directory. */
type RowKind = "own" | "custom" | "public";

interface ViewerLocation {
  latitude: number;
  longitude: number;
}

type SortOrder = "distance" | "latency" | "name";
/** Radius choices of the distance filter, in km. */
const DISTANCE_FILTERS = [100, 500, 2000];

export function PublicNetworks() {
  const { t } = useI18n();
  const toast = useToast();
  const cluster = useCluster();
  const node = useNode();
  const [networks, setNetworks] = useState<PublicNetwork[] | null>(null);
  const [localNetwork, setLocalNetwork] = useState<PublicNetwork | null>(null);
  const [pinned, setPinned] = useState<PinnedNetworks | null>(null);
  const [importing, setImporting] = useState(false);
  const [removeTarget, setRemoveTarget] = useState<PublicNetwork | null>(null);
  const [removing, setRemoving] = useState(false);
  const importInput = useRef<HTMLInputElement>(null);
  const [error, setError] = useState<unknown>(null);
  const [refreshing, setRefreshing] = useState(false);
  const networkInfo = useNetworkInfo(Boolean(node.data));
  const [browserLocation, setBrowserLocation] = useState<ViewerLocation | null>(null);
  const [locating, setLocating] = useState(false);
  const [sortOrder, setSortOrder] = useState<SortOrder>("distance");
  const [withinKm, setWithinKm] = useState(0);
  const [latencyAsked, setLatencyAsked] = useState(false);
  const [detailsTarget, setDetailsTarget] = useState<PublicNetwork | null>(null);
  const [chatTarget, setChatTarget] = useState<PublicNetwork | null>(null);
  const [leaveConfirm, setLeaveConfirm] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const [countryFilter, setCountryFilter] = useState("");
  const [cityFilter, setCityFilter] = useState("");
  const [reachabilityRefreshKey, setReachabilityRefreshKey] = useState(0);

  /** This machine's own network and the imported ones; an older node has neither. */
  function loadPinned() {
    return api.get<PinnedNetworks>("/api/ui/networks/pinned")
      .then(setPinned)
      .catch(() => setPinned(null));
  }

  async function importNetwork(file: File | undefined) {
    if (!file) return;
    setImporting(true);
    try {
      const imported = await api.post<PinnedNetwork>("/api/ui/networks/custom", { trust_document: await file.text() });
      await loadPinned();
      toast.success(t("publicNetworks.importDone"), imported.network.name || imported.network.cluster_id);
    } catch (reason) {
      toast.fromError(t("publicNetworks.importFailed"), reason);
    } finally {
      setImporting(false);
      if (importInput.current) importInput.current.value = "";
    }
  }

  async function removeCustom() {
    if (!removeTarget) return;
    setRemoving(true);
    try {
      await api.del(`/api/ui/networks/custom/${encodeURIComponent(removeTarget.fingerprint)}`);
      await loadPinned();
    } catch (reason) {
      toast.fromError(t("publicNetworks.removeFailed"), reason);
    } finally {
      setRemoving(false);
      setRemoveTarget(null);
    }
  }

  async function load() {
    setRefreshing(true);
    setError(null);
    setReachabilityRefreshKey((value) => value + 1);
    try {
      const discovered = await api.get<PublicNetwork[]>("/api/public/networks", { auth: false });
      setNetworks(discovered);
      void loadPinned();
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
  }, []);

  /*
   * Distances need a point to measure from. This network's own location is
   * there without asking anyone; the browser's is asked only when the
   * operator presses the button, and it never leaves the browser.
   */
  const ownLocation = networkInfo.data?.network.location ?? null;
  const viewerLocation: ViewerLocation | null = browserLocation ?? (ownLocation ? { latitude: ownLocation.latitude, longitude: ownLocation.longitude } : null);

  function useBrowserLocation() {
    if (!window.isSecureContext || !navigator.geolocation) {
      toast.warning(t("publicNetworks.browserLocationUnavailable"));
      return;
    }
    setLocating(true);
    navigator.geolocation.getCurrentPosition(
      (position) => {
        setLocating(false);
        setBrowserLocation({ latitude: position.coords.latitude, longitude: position.coords.longitude });
      },
      () => {
        setLocating(false);
        toast.warning(t("publicNetworks.browserLocationDenied"));
      },
      { enableHighAccuracy: false, maximumAge: 300000, timeout: 8000 },
    );
  }

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
  /*
   * Two kinds of rows stay at the top whatever the filters and the order:
   * the network this machine is in, then the ones imported here. The
   * directory's copy of either is dropped so no network shows twice.
   */
  const ownNetwork = pinned?.own?.network ?? localNetwork;
  const ownFingerprint = ownNetwork?.fingerprint;
  const customNetworks = (pinned?.custom ?? []).filter((entry) => entry.network.fingerprint !== ownFingerprint);
  const pinnedFingerprints = new Set([ownFingerprint, ...customNetworks.map((entry) => entry.network.fingerprint)].filter(Boolean));
  const radiusKm = viewerLocation ? withinKm : 0;
  const filteredNetworks = useMemo(
    () => (networks ?? []).filter((network) => {
      if (countryFilter && countryKey(network) !== countryFilter) return false;
      if (selectedCity && network.city !== selectedCity) return false;
      if (!radiusKm) return true;
      const distance = distanceKm(network.location, viewerLocation);
      return distance !== null && distance <= radiusKm;
    }),
    [countryFilter, networks, radiusKm, selectedCity, viewerLocation?.latitude, viewerLocation?.longitude],
  );
  // Measuring dials every listed network, so it waits for the operator to ask.
  const measuring = latencyAsked || sortOrder === "latency";
  const latencies = usePublicLatencies(
    [...(pinned?.custom ?? []).map((entry) => entry.network), ...(networks ?? [])].filter((network) => network.fingerprint !== ownFingerprint).map((network) => network.publisher_key),
    measuring,
    reachabilityRefreshKey,
  );
  const orderedNetworks = useMemo(
    () => {
      // Whatever has no value for the chosen order goes last, in list order.
      const by = (value: (network: PublicNetwork) => number | null) => (left: PublicNetwork, right: PublicNetwork) => {
        const leftValue = value(left);
        const rightValue = value(right);
        if (leftValue === null && rightValue === null) return 0;
        if (leftValue === null) return 1;
        if (rightValue === null) return -1;
        return leftValue - rightValue;
      };
      const sorted = [...filteredNetworks];
      if (sortOrder === "name") return sorted.sort((left, right) => (left.name || left.cluster_id).localeCompare(right.name || right.cluster_id));
      if (sortOrder === "latency") {
        return sorted.sort(by((network) => {
          // This node's own network is as close as a network gets.
          if (network.fingerprint === ownFingerprint) return 0;
          const latency = latencies[network.publisher_key];
          return typeof latency === "number" ? latency : null;
        }));
      }
      return sorted.sort(by((network) => distanceKm(network.location, viewerLocation)));
    },
    [filteredNetworks, latencies, ownFingerprint, sortOrder, viewerLocation?.latitude, viewerLocation?.longitude],
  );
  const rows: { network: PublicNetwork; kind: RowKind; visibility: Visibility }[] = [
    ...(ownNetwork ? [{ network: ownNetwork, kind: "own" as const, visibility: pinned?.own?.visibility ?? "public" as const }] : []),
    ...customNetworks.map((entry) => ({ network: entry.network, kind: "custom" as const, visibility: entry.visibility })),
    ...orderedNetworks.filter((network) => !pinnedFingerprints.has(network.fingerprint)).map((network) => ({ network, kind: "public" as const, visibility: "public" as const })),
  ];
  const detailsVisibility = rows.find((row) => row.network.fingerprint === detailsTarget?.fingerprint)?.visibility ?? "public";
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
        actions={<div className="flex items-center gap-2">
          <input ref={importInput} type="file" accept=".json,application/json" className="hidden" data-testid="network-import-file" onChange={(event) => void importNetwork(event.target.files?.[0])} />
          <Button size="sm" variant="primary" loading={importing} title={t("publicNetworks.addDetail")} onClick={() => importInput.current?.click()} icon={<Plus className="h-3.5 w-3.5" />}>{t("publicNetworks.add")}</Button>
          <Button size="sm" loading={refreshing} onClick={() => void load()} icon={<RefreshCw className="h-3.5 w-3.5" />}>{t("publicNetworks.refresh")}</Button>
        </div>}
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
            <label className="flex w-[9rem] min-w-[7rem] flex-col gap-1">
              <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("publicNetworks.within")}</span>
              <Select aria-label={t("publicNetworks.within")} compact value={radiusKm} disabled={!viewerLocation} title={!viewerLocation ? t("publicNetworks.distanceFromNone") : undefined} onChange={(event) => setWithinKm(Number(event.target.value))}>
                <option value={0}>{t("publicNetworks.withinAny")}</option>
                {DISTANCE_FILTERS.map((km) => <option key={km} value={km}>{t("publicNetworks.withinKm", { value: km })}</option>)}
              </Select>
            </label>
            <label className="flex w-[10rem] min-w-[8rem] flex-col gap-1">
              <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("publicNetworks.sort")}</span>
              <Select aria-label={t("publicNetworks.sort")} compact value={sortOrder} onChange={(event) => setSortOrder(event.target.value as SortOrder)}>
                <option value="distance">{t("publicNetworks.sortDistance")}</option>
                <option value="latency">{t("publicNetworks.sortLatency")}</option>
                <option value="name">{t("publicNetworks.sortName")}</option>
              </Select>
            </label>
            <Button size="sm" loading={locating} disabled={locating} onClick={useBrowserLocation} icon={<LocateFixed className="h-3.5 w-3.5" />}>{t("publicNetworks.useBrowserLocation")}</Button>
            <Button size="sm" disabled={measuring} onClick={() => setLatencyAsked(true)} icon={<Gauge className="h-3.5 w-3.5" />}>{t("publicNetworks.measureLatency")}</Button>
          </div>
        </div>
        {error ? <div className="px-5 py-5"><ErrorState title={t("publicNetworks.failed")} error={error} onRetry={() => void load()} /></div> : networks === null ? <div className="space-y-3 px-5 py-5"><Skeleton className="h-10 w-full" /><Skeleton className="h-10 w-full" /></div> : rows.length === 0 ? <div className="px-5 py-12 text-center"><Globe2 className="mx-auto h-7 w-7 text-faint" /><p className="mt-3 text-sm font-medium text-fg">{t(networks.length === 0 ? "publicNetworks.emptyTitle" : "publicNetworks.noMatches")}</p><p className="mx-auto mt-1 max-w-md text-sm leading-relaxed text-muted">{t(networks.length === 0 ? "publicNetworks.emptyDetail" : "publicNetworks.noMatchesDetail")}</p></div> : <div className="min-h-0 flex-1 overflow-auto"><table className="min-w-[1200px] w-full border-collapse text-left text-xs"><thead className="sticky top-0 z-10 bg-raised text-[0.66rem] uppercase tracking-[0.12em] text-faint"><tr>{["name", "organization", "location", "reach", "nodes", "latency", "vram", "models", "action"].map((column) => <th key={column} className="border-b border-line px-4 py-3 font-medium">{t(`publicNetworks.${column}`)}</th>)}</tr></thead><tbody className="divide-y divide-line">{rows.map(({ network, kind }) => { const isLocal = kind === "own"; return <NetworkRow key={`${network.cluster_id}:${network.fingerprint}`} network={network} kind={kind} onExport={() => exportTrustDocument(network)} onRemove={() => setRemoveTarget(network)} distance={distanceKm(network.location, viewerLocation)} latency={isLocal ? undefined : latencies[network.publisher_key]} stats={isLocal ? metrics : null} membership={!isLocal ? "none" : isPrincipal ? "principal" : "member"} leaving={leaving} onLeave={() => setLeaveConfirm(true)} onChat={() => setChatTarget(network)} onDetails={() => setDetailsTarget(network)} t={t} refreshKey={reachabilityRefreshKey} />; })}</tbody></table></div>}
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-x-5 gap-y-2 px-1 py-3 text-xs leading-relaxed text-faint"><span data-testid="distance-origin">{t(browserLocation ? "publicNetworks.distanceFromBrowser" : ownLocation ? "publicNetworks.distanceFromNetwork" : "publicNetworks.distanceFromNone")}</span><span>{t("publicNetworks.locationNote")}</span><span className="flex items-center gap-2"><ShieldCheck className="h-3.5 w-3.5 shrink-0" />{t("publicNetworks.securityNote")}</span></div>
      <Confirm open={leaveConfirm} title={t("network.forgetNetworkTitle")} description={t("network.forgetNetworkDescription")} confirmLabel={t("network.leave")} tone="danger" busy={leaving} onConfirm={() => void leave()} onCancel={() => !leaving && setLeaveConfirm(false)} />
      <LobbyChat network={chatTarget && { name: chatTarget.name || chatTarget.cluster_id, publisher_key: chatTarget.publisher_key, accepted_enrollments: chatTarget.accepted_enrollments }} endpoint="/api/ui/network-chat/lobby" onClose={() => setChatTarget(null)} />
      <Confirm open={removeTarget !== null} title={t("publicNetworks.removeTitle")} description={t("publicNetworks.removeDescription", { name: removeTarget?.name || removeTarget?.cluster_id || "" })} confirmLabel={t("publicNetworks.remove")} tone="danger" busy={removing} onConfirm={() => void removeCustom()} onCancel={() => !removing && setRemoveTarget(null)} />
      <PublicNetworkDetails network={detailsTarget} visibility={detailsVisibility} onClose={() => setDetailsTarget(null)} t={t} />
    </div>
  );
}

/** Saves a network's trust document as the file another node imports. */
function exportTrustDocument(network: PublicNetwork) {
  const name = (network.name || network.cluster_id).toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "network";
  const url = URL.createObjectURL(new Blob([network.trust_document], { type: "application/json;charset=utf-8" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = `${name}-trust.json`;
  link.click();
  URL.revokeObjectURL(url);
}

function NetworkRow({ network, kind, onExport, onRemove, distance, latency, stats: localStats, membership, leaving, onLeave, onChat, onDetails, t, refreshKey }: { network: PublicNetwork; kind: RowKind; onExport: () => void; onRemove: () => void; distance: number | null; latency: PublicLatency | undefined; stats: NetworkStats | null; membership: "none" | "member" | "principal"; leaving: boolean; onLeave: () => void; onChat: () => void; onDetails: () => void; t: (key: string, values?: Record<string, string | number>) => string; refreshKey: number }) {
  const unavailable = t("publicNetworks.unavailable");
  const location = placeOf(network) || unavailable;
  const reachability = usePublicLocator(network.publisher_key, refreshKey);
  // The local network is read from this node's own cluster; any other one is
  // asked from its principal, and only when it chose to publish.
  const publishedStats = usePublicStats("/api/ui/network-chat/lobby/info", network.publisher_key, membership === "none" && Boolean(network.public_stats), refreshKey);
  const stats = localStats ?? publishedStats;
  // The local network shows the average between its own machines; any other
  // one shows the round trip this node measured to it, once asked.
  const latencyText = stats?.averageLatencyMs != null
    ? `${stats.averageLatencyMs.toFixed(1)} ms`
    : typeof latency === "number"
      ? t("publicNetworks.latencyValue", { value: latency })
      : latency === "checking"
        ? t("publicNetworks.latencyChecking")
        : latency === "failed"
          ? t("publicNetworks.latencyFailed")
          : unavailable;
  const models = stats?.models.length ? stats.models.join(", ") : unavailable;
  const closed = network.accepted_enrollments?.length === 0;
  return <tr data-testid={`network-row-${network.fingerprint}`} data-network-kind={kind} className={kind === "custom" ? "align-top bg-amber/[0.05] shadow-[inset_3px_0_0_rgb(var(--amber)/0.7)] transition-colors hover:bg-amber/[0.09]" : kind === "own" ? "align-top bg-aqua/[0.05] shadow-[inset_3px_0_0_rgb(var(--aqua)/0.7)] transition-colors hover:bg-aqua/[0.09]" : "align-top transition-colors hover:bg-raised/45"}><td className="max-w-[15rem] px-4 py-3.5">{kind !== "public" && <div className="mb-1.5">{kind === "own" ? <Badge tone="aqua"><House className="h-3 w-3" />{t("publicNetworks.kindOwn")}</Badge> : <Badge tone="amber"><Lock className="h-3 w-3" />{t("publicNetworks.kindCustom")}</Badge>}</div>}<button type="button" className="group text-left" onClick={onDetails} aria-label={t("publicNetworks.openDetails", { name: network.name || network.cluster_id })}><span className="font-medium text-fg group-hover:text-iris">{network.name || network.cluster_id}</span>{distance !== null && <span className="ml-2 whitespace-nowrap text-[0.65rem] text-lime">{t((network.location?.accuracy_m ?? 0) >= 1000 ? "publicNetworks.distanceApproximate" : "publicNetworks.distance", { value: distance.toFixed(0) })}</span>}<p className="mt-1 break-all font-mono text-[0.64rem] text-faint">{network.fingerprint}</p></button></td><td className="max-w-[13rem] px-4 py-3.5 text-muted">{network.organization || unavailable}</td><td className="max-w-[16rem] px-4 py-3.5 text-muted">{location}</td><td className="max-w-[18rem] px-4 py-3.5"><ReachabilityIndicator id={network.fingerprint} status={reachability} t={t} /></td><td className="px-4 py-3.5 font-mono text-muted">{stats ? stats.nodeCount : unavailable}</td><td className="whitespace-nowrap px-4 py-3.5 font-mono text-muted" data-testid={`public-network-latency-${network.fingerprint}`}>{latencyText}</td><td className="px-4 py-3.5 font-mono text-muted">{stats?.vramTotalMb != null ? `${(stats.vramTotalMb / 1024).toFixed(1)} GB` : unavailable}</td><td className="max-w-[18rem] px-4 py-3.5 text-muted">{models}</td><td className="px-4 py-3.5"><div className="flex flex-wrap items-center gap-2">{membership === "none" ? <Button size="sm" variant="primary" data-guide={!closed ? "network-join" : undefined} title={closed ? t("publicNetworks.closedDetail") : undefined} onClick={(event) => { event.stopPropagation(); onChat(); }} icon={<MessageCircle className="h-3.5 w-3.5" />}>{t(closed ? "publicNetworks.chat" : "publicNetworks.join")}</Button> : membership === "member" ? <Button size="sm" variant="danger" loading={leaving} disabled={leaving} onClick={(event) => { event.stopPropagation(); onLeave(); }} icon={<LogOut className="h-3.5 w-3.5" />}>{t("network.leave")}</Button> : null}<Button size="sm" variant="ghost" aria-label={t("publicNetworks.exportTrust", { name: network.name || network.cluster_id })} title={t("publicNetworks.exportTrustDetail")} disabled={!network.trust_document} onClick={(event) => { event.stopPropagation(); onExport(); }} icon={<Download className="h-3.5 w-3.5" />} />{kind === "custom" && <Button size="sm" variant="ghost" aria-label={t("publicNetworks.removeNamed", { name: network.name || network.cluster_id })} title={t("publicNetworks.remove")} onClick={(event) => { event.stopPropagation(); onRemove(); }} icon={<Trash2 className="h-3.5 w-3.5" />} />}</div></td></tr>;
}

function PublicNetworkDetails({ network, visibility, onClose, t }: { network: PublicNetwork | null; visibility: Visibility; onClose: () => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const unavailable = t("publicNetworks.unavailable");
  const value = (text: string) => text || unavailable;
  const location = network?.location ? `${network.location.latitude}, ${network.location.longitude}` : unavailable;
  if (!network) return null;
  return (
    <Modal open title={t("publicNetworks.detailsTitle", { name: network.name || network.cluster_id })} onClose={onClose} size="lg" footer={<Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>}>
      <div className="space-y-5">
        <div className="grid gap-4 sm:grid-cols-2">
          <DetailBlock label={t("network.organization")}><span>{value(network.organization)}</span></DetailBlock>
          <DetailBlock label={t("network.visibility")}><span>{t(visibility === "private" ? "network.visibilityPrivate" : visibility === "local" ? "network.visibilityLocal" : "network.visibilityPublic")}</span></DetailBlock>
          <DetailBlock label={t("network.accessPolicy")}><span>{accessPolicyLabel(network.access_policy, t)}</span></DetailBlock>
          <DetailBlock label={t("network.acceptedEnrollments")}><span>{acceptedEnrollmentsLabel(network.accepted_enrollments, t)}</span></DetailBlock>
          <DetailBlock label={t("publicNetworks.place")}><span>{value(placeOf(network))}</span></DetailBlock>
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
  return t(value === "anyone_with_anchor" ? "network.accessAnyoneWithAnchor" : value === "auto_approve" ? "network.accessAutoApprove" : value === "local_only" ? "network.accessLocalOnly" : "network.accessAuthorized");
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

/** "City, State, Country", as the node resolved it from the coordinates. */
function placeOf(network: PublicNetwork): string {
  return [network.city, network.state, network.country_name || network.country_code].filter(Boolean).join(", ");
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
