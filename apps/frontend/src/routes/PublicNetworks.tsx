import { useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import { ChevronLeft, ChevronRight, CircleCheck, CircleHelp, CircleX, Download, Gauge, Globe2, House, LoaderCircle, LocateFixed, Lock, LogIn, LogOut, Maximize2, MessageCircle, Plus, RefreshCw, Search, ShieldCheck, Trash2, WandSparkles, Waypoints, X } from "lucide-react";
import { Confirm, Modal } from "../components/Confirm";
import { LobbyChat } from "../components/LobbyChat";
import { Markdown } from "../components/Markdown";
import { ENROLLMENT_ICONS, NetworkQuickSetup, VISIBILITY_ICONS, VISIBILITY_LABELS } from "../components/NetworkQuickSetup";
import { Badge, Button, ErrorState, Input, PageHeader, Select, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { since } from "../lib/format";
import { networkStats, type NetworkStats } from "../lib/networkStats";
import { usePublicLatencies, type PublicLatency } from "../lib/publicLatency";
import { usePublicLocator, type PublicLocator } from "../lib/publicLocator";
import { usePublicStats } from "../lib/publicStats";
import { useCluster, useInvalidate, useNetworkInfo, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { ENROLLMENT_ROLES } from "../lib/types";
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
  /**
   * The network's Chat is on, so its lobby carries a conversation. Without it
   * the lobby only takes admission requests, and opens as a window.
   */
  lobby_chat?: boolean;
  location: { latitude: number; longitude: number; accuracy_m?: number } | null;
  discovered_via: string;
  directory_replicas?: string[];
}

/** A row of the node's table of every network it has heard of. Times are Unix seconds. */
interface KnownNetwork {
  network: PublicNetwork;
  first_seen_at: number;
  last_seen_at: number;
}

/** `GET /api/ui/networks/known`: one page, filtered and ordered by the node. */
interface KnownNetworksPage {
  networks: KnownNetwork[];
  total: number;
  page: number;
  page_size: number;
  countries: { code: string; name: string }[];
  cities: string[];
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

type Membership = "none" | "member" | "principal";

interface Row {
  network: PublicNetwork;
  kind: RowKind;
  visibility: Visibility;
  /** When the node last heard of it, for a directory row. */
  lastSeenAt?: number;
}

interface OpenLobby {
  network: PublicNetwork;
  /** Opened from "Ask to join": the lobby puts the focus on the request. */
  focusAdmission: boolean;
}

interface ViewerLocation {
  latitude: number;
  longitude: number;
}

type SortOrder = "distance" | "latency" | "name";
/** Radius choices of the distance filter, in km. */
const DISTANCE_FILTERS = [100, 500, 2000];
const PAGE_SIZE = 25;
/** How long the search waits for the operator to stop typing. */
const SEARCH_DEBOUNCE_MS = 300;

type T = (key: string, values?: Record<string, string | number>) => string;

export function PublicNetworks() {
  const { t } = useI18n();
  const toast = useToast();
  const cluster = useCluster();
  const node = useNode();
  const [known, setKnown] = useState<KnownNetworksPage | null>(null);
  const [knownError, setKnownError] = useState<unknown>(null);
  const [knownVersion, setKnownVersion] = useState(0);
  /** Fingerprints the live listing confirmed in this visit; `null` until it answers. */
  const [liveFingerprints, setLiveFingerprints] = useState<Set<string> | null>(null);
  const [localNetwork, setLocalNetwork] = useState<PublicNetwork | null>(null);
  const [pinned, setPinned] = useState<PinnedNetworks | null>(null);
  const [importing, setImporting] = useState(false);
  const [removeTarget, setRemoveTarget] = useState<PublicNetwork | null>(null);
  const [removing, setRemoving] = useState(false);
  const importInput = useRef<HTMLInputElement>(null);
  const [refreshing, setRefreshing] = useState(false);
  const networkInfo = useNetworkInfo(Boolean(node.data));
  const [browserLocation, setBrowserLocation] = useState<ViewerLocation | null>(null);
  const [locating, setLocating] = useState(false);
  const [sortOrder, setSortOrder] = useState<SortOrder>("distance");
  const [withinKm, setWithinKm] = useState(0);
  const [latencyAsked, setLatencyAsked] = useState(false);
  const [selected, setSelected] = useState<Row | null>(null);
  const [detailsOpen, setDetailsOpen] = useState(false);
  /** Lobbies kept open, each a full-screen tab; `activeLobby` is the one shown. */
  const [lobbies, setLobbies] = useState<OpenLobby[]>([]);
  const [activeLobby, setActiveLobby] = useState<string | null>(null);
  /** A network whose lobby does not answer: only the request, in a window over the list. */
  const [requestLobby, setRequestLobby] = useState<PublicNetwork | null>(null);
  const [leaveConfirm, setLeaveConfirm] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const [countryFilter, setCountryFilter] = useState("");
  const [cityFilter, setCityFilter] = useState("");
  const [search, setSearch] = useState("");
  const [searchQuery, setSearchQuery] = useState("");
  const [page, setPage] = useState(1);
  const [reachabilityRefreshKey, setReachabilityRefreshKey] = useState(0);
  const [quickSetupOpen, setQuickSetupOpen] = useState(false);
  const invalidate = useInvalidate();

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
      setKnownVersion((value) => value + 1);
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
      setKnownVersion((value) => value + 1);
      if (selected?.network.fingerprint === removeTarget.fingerprint) setSelected(null);
    } catch (reason) {
      toast.fromError(t("publicNetworks.removeFailed"), reason);
    } finally {
      setRemoving(false);
      setRemoveTarget(null);
    }
  }

  /*
   * The live listing (pkarr, relays, directory) is slow and only sees what
   * answers now. The node records what it returns in its table of known
   * networks, so when it is done the page is read again from there.
   */
  async function refreshLive() {
    setRefreshing(true);
    setReachabilityRefreshKey((value) => value + 1);
    void loadPinned();
    void api.get<PublicNetwork>("/api/public/network", { auth: false })
      .then(setLocalNetwork)
      .catch(() => setLocalNetwork(null));
    try {
      const discovered = await api.get<PublicNetwork[]>("/api/public/networks", { auth: false });
      setLiveFingerprints(new Set(discovered.map((network) => network.fingerprint)));
    } catch (reason) {
      toast.fromError(t("publicNetworks.liveFailed"), reason);
    } finally {
      setRefreshing(false);
      setKnownVersion((value) => value + 1);
    }
  }

  useEffect(() => {
    void refreshLive();
  }, []);

  useEffect(() => {
    const timer = window.setTimeout(() => setSearchQuery(search.trim()), SEARCH_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [search]);

  /*
   * Distances need a point to measure from. This network's own location is
   * there without asking anyone; the browser's is asked only when the
   * operator presses the button, and it never leaves the browser.
   */
  const ownLocation = networkInfo.data?.network.location ?? null;
  const viewerLocation: ViewerLocation | null = browserLocation ?? (ownLocation ? { latitude: ownLocation.latitude, longitude: ownLocation.longitude } : null);
  const radiusKm = viewerLocation ? withinKm : 0;

  // Any change of what is asked starts again from the first page.
  useEffect(() => {
    setPage(1);
  }, [searchQuery, countryFilter, cityFilter, sortOrder, radiusKm]);

  useEffect(() => {
    let current = true;
    const params = new URLSearchParams({ page: String(page), page_size: String(PAGE_SIZE) });
    if (searchQuery) params.set("q", searchQuery);
    if (countryFilter) params.set("country", countryFilter);
    if (cityFilter) params.set("city", cityFilter);
    // Latency is measured in the browser, so the page comes by distance and
    // is reordered here.
    params.set("sort", sortOrder === "latency" ? "distance" : sortOrder);
    if (viewerLocation) {
      params.set("latitude", String(viewerLocation.latitude));
      params.set("longitude", String(viewerLocation.longitude));
      if (radiusKm) params.set("within_km", String(radiusKm));
    }
    api.get<KnownNetworksPage>(`/api/ui/networks/known?${params}`)
      .then((answer) => {
        if (!current) return;
        setKnown(answer);
        setKnownError(null);
        // A page past the end, after a removal, is answered as the last one.
        if (answer.page !== page) setPage(answer.page);
      })
      .catch((reason) => {
        if (current) setKnownError(reason);
      });
    return () => {
      current = false;
    };
  }, [searchQuery, countryFilter, cityFilter, sortOrder, radiusKm, viewerLocation?.latitude, viewerLocation?.longitude, page, knownVersion]);

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

  const countryOptions = known?.countries ?? [];
  const cityOptions = known?.cities ?? [];
  /*
   * Two kinds of rows stay at the top of every page whatever the search, the
   * filters and the order: the network this machine is in, then the ones
   * imported here. The node leaves both out of the known list, and the
   * check below covers an own network it does not describe the same way.
   */
  const ownNetwork = pinned?.own?.network ?? localNetwork;
  const ownFingerprint = ownNetwork?.fingerprint;
  const allCustomNetworks = (pinned?.custom ?? []).filter((entry) => entry.network.fingerprint !== ownFingerprint);
  const pinnedFingerprints = new Set([ownFingerprint, ...allCustomNetworks.map((entry) => entry.network.fingerprint)].filter(Boolean));
  /*
   * The node searches and filters only the list; the pinned rows are filtered
   * here by the same rules, or a search would always leave them on screen
   * and look like it did nothing.
   */
  const pinnedMatches = (network: PublicNetwork) => {
    const words = foldText(searchQuery).split(/\s+/).filter(Boolean);
    const text = foldText([network.name, network.cluster_id, network.organization, network.description, network.city, network.state ?? "", network.country_name, network.country_code].join(" "));
    if (!words.every((word) => text.includes(word))) return false;
    if (countryFilter && (network.country_code || network.country_name) !== countryFilter) return false;
    if (cityFilter && network.city !== cityFilter) return false;
    if (radiusKm) {
      const distance = distanceKm(network.location, viewerLocation);
      if (distance === null || distance > radiusKm) return false;
    }
    return true;
  };
  const ownShown = ownNetwork && pinnedMatches(ownNetwork) ? ownNetwork : null;
  const customNetworks = allCustomNetworks.filter((entry) => pinnedMatches(entry.network));
  const pageNetworks = useMemo(
    () => (known?.networks ?? []).filter((entry) => !pinnedFingerprints.has(entry.network.fingerprint)),
    [known, ownFingerprint, allCustomNetworks.map((entry) => entry.network.fingerprint).join(",")],
  );
  // Measuring dials every listed network, so it waits for the operator to ask.
  const measuring = latencyAsked || sortOrder === "latency";
  const latencies = usePublicLatencies(
    [...customNetworks.map((entry) => entry.network), ...pageNetworks.map((entry) => entry.network)].map((network) => network.publisher_key),
    measuring,
    reachabilityRefreshKey,
  );
  const orderedPage = useMemo(() => {
    if (sortOrder !== "latency") return pageNetworks;
    // Whatever has no latency goes last, in page order.
    const value = (entry: KnownNetwork) => {
      const latency = latencies[entry.network.publisher_key];
      return typeof latency === "number" ? latency : null;
    };
    return [...pageNetworks].sort((left, right) => {
      const leftValue = value(left);
      const rightValue = value(right);
      if (leftValue === null && rightValue === null) return 0;
      if (leftValue === null) return 1;
      if (rightValue === null) return -1;
      return leftValue - rightValue;
    });
  }, [latencies, pageNetworks, sortOrder]);
  const rows: Row[] = [
    ...(ownShown ? [{ network: ownShown, kind: "own" as const, visibility: pinned?.own?.visibility ?? "public" as const }] : []),
    ...customNetworks.map((entry) => ({ network: entry.network, kind: "custom" as const, visibility: entry.visibility })),
    ...orderedPage.map((entry) => ({ network: entry.network, kind: "public" as const, visibility: "public" as const, lastSeenAt: entry.last_seen_at })),
  ];
  // The panel follows the freshest copy of the selected network.
  const selectedRow = selected ? rows.find((row) => row.network.fingerprint === selected.network.fingerprint) ?? selected : null;
  const metrics = networkStats(cluster.data);
  /*
   * The row of the network this node already belongs to never offers Join.
   * A member gets Leave; the principal gets no action at all, because the
   * network is published under its key and leaving would orphan it.
   */
  const isPrincipal = node.data?.authority_level === "primary";
  /*
   * Quick setup is for the principal of a network that is not published yet:
   * discovery off or hidden is how a new installation starts.
   */
  const ownSettings = networkInfo.data?.network ?? null;
  const canQuickSetup = isPrincipal && Boolean(networkInfo.data?.can_edit) && ownSettings !== null && ownSettings.visibility !== "public";
  const membershipOf = (kind: RowKind): Membership => kind !== "own" ? "none" : isPrincipal ? "principal" : "member";
  const total = known?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const firstShown = total === 0 ? 0 : (page - 1) * PAGE_SIZE + 1;
  const lastShown = Math.min(total, page * PAGE_SIZE);
  const filtering = Boolean(searchQuery || countryFilter || cityFilter || radiusKm);

  /*
   * A lobby replaces the list on this screen instead of a window over it, and
   * stays open (and polled, which keeps this machine listed there) when the
   * operator goes back, so several conversations can run at once.
   */
  function openLobby(network: PublicNetwork, focusAdmission: boolean) {
    setLobbies((open) => open.some((entry) => entry.network.fingerprint === network.fingerprint)
      ? open.map((entry) => entry.network.fingerprint === network.fingerprint ? { network, focusAdmission: entry.focusAdmission || focusAdmission } : entry)
      : [...open, { network, focusAdmission }]);
    setActiveLobby(network.fingerprint);
  }

  /** The network's lobby is off: there is nothing to fill a screen with, so the request goes in a window. */
  function lobbyUnavailable(network: PublicNetwork) {
    closeLobby(network.fingerprint);
    setRequestLobby(network);
  }

  function closeLobby(fingerprint: string) {
    setLobbies((open) => open.filter((entry) => entry.network.fingerprint !== fingerprint));
    setActiveLobby((active) => active === fingerprint ? null : active);
  }

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

  /** The other open lobbies, reachable from inside a lobby without going back to the list. */
  const lobbySwitcher = (
    <div className="flex min-w-0 items-center gap-1 overflow-x-auto" aria-label={t("publicNetworks.openLobbies")}>
      {lobbies.map(({ network }) => (
        <button key={network.fingerprint} type="button" aria-pressed={activeLobby === network.fingerprint} onClick={() => setActiveLobby(network.fingerprint)} className={`flex shrink-0 items-center gap-1.5 rounded-md px-2 py-1 text-xs ${activeLobby === network.fingerprint ? "bg-iris/15 text-fg" : "text-muted hover:text-fg"}`}><MessageCircle className="h-3.5 w-3.5" />{network.name || network.cluster_id}</button>
      ))}
    </div>
  );

  const body = knownError && !known
    ? <div className="px-5 py-5"><ErrorState title={t("publicNetworks.failed")} error={knownError} onRetry={() => setKnownVersion((value) => value + 1)} /></div>
    : known === null || (rows.length === 0 && refreshing && !filtering)
      ? <div className="space-y-3 px-5 py-5"><Skeleton className="h-10 w-full" /><Skeleton className="h-10 w-full" /></div>
      : rows.length === 0
        ? <div className="px-5 py-12 text-center"><Globe2 className="mx-auto h-7 w-7 text-faint" /><p className="mt-3 text-sm font-medium text-fg">{t(filtering ? "publicNetworks.noMatches" : "publicNetworks.emptyTitle")}</p><p className="mx-auto mt-1 max-w-md text-sm leading-relaxed text-muted">{t(filtering ? "publicNetworks.noMatchesDetail" : "publicNetworks.emptyDetail")}</p></div>
        : <div className="min-h-0 flex-1 overflow-auto"><table className="min-w-[1200px] w-full border-collapse text-left text-xs"><thead className="sticky top-0 z-10 bg-raised text-[0.66rem] uppercase tracking-[0.12em] text-faint"><tr>{["name", "organization", "location", "reach", "nodes", "latency", "vram", "models", "action"].map((column) => <th key={column} className="border-b border-line px-4 py-3 font-medium">{t(`publicNetworks.${column}`)}</th>)}</tr></thead><tbody className="divide-y divide-line">{rows.map((row) => { const { network, kind } = row; const isLocal = kind === "own"; return <NetworkRow key={`${network.cluster_id}:${network.fingerprint}`} network={network} kind={kind} visibility={row.visibility} selected={selectedRow?.network.fingerprint === network.fingerprint} cachedSince={kind === "public" && liveFingerprints && !liveFingerprints.has(network.fingerprint) ? row.lastSeenAt : undefined} onExport={() => exportTrustDocument(network)} onRemove={() => setRemoveTarget(network)} distance={distanceKm(network.location, viewerLocation)} latency={isLocal ? undefined : latencies[network.publisher_key]} stats={isLocal ? metrics : null} membership={membershipOf(kind)} leaving={leaving} onLeave={() => setLeaveConfirm(true)} onChat={() => openLobby(network, network.accepted_enrollments?.length !== 0)} onQuickSetup={isLocal && canQuickSetup ? () => setQuickSetupOpen(true) : undefined} onSelect={() => setSelected(row)} t={t} refreshKey={reachabilityRefreshKey} />; })}</tbody></table></div>;

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      {!activeLobby && <PageHeader
        title={t("publicNetworks.title")}
        subtitle={t("publicNetworks.subtitle")}
        className="!mb-0 border-b border-line bg-surface/75 px-3 py-3 sm:px-4"
        actions={<div className="flex items-center gap-2">
          <input ref={importInput} type="file" accept=".json,application/json" className="hidden" data-testid="network-import-file" onChange={(event) => void importNetwork(event.target.files?.[0])} />
          <Button size="sm" variant="primary" loading={importing} title={t("publicNetworks.addDetail")} onClick={() => importInput.current?.click()} icon={<Plus className="h-3.5 w-3.5" />}>{t("publicNetworks.add")}</Button>
          {canQuickSetup && <Button size="sm" variant="primary" onClick={() => setQuickSetupOpen(true)} icon={<WandSparkles className="h-3.5 w-3.5" />}>{t("quickSetup.action")}</Button>}
          <Button size="sm" loading={refreshing} onClick={() => void refreshLive()} icon={<RefreshCw className="h-3.5 w-3.5" />}>{t("publicNetworks.refresh")}</Button>
        </div>}
      />}
      {lobbies.length > 0 && !activeLobby && (
        <div className="flex shrink-0 items-center gap-1 overflow-x-auto border-b border-line bg-surface/60 px-2 py-1" role="tablist" aria-label={t("publicNetworks.openLobbies")}>
          <button type="button" role="tab" aria-selected={activeLobby === null} onClick={() => setActiveLobby(null)} className={`flex shrink-0 items-center gap-1.5 rounded-md px-2.5 py-1 text-xs ${activeLobby === null ? "bg-iris/15 text-fg" : "text-muted hover:text-fg"}`}><Globe2 className="h-3.5 w-3.5" />{t("publicNetworks.title")}</button>
          {lobbies.map(({ network }) => {
            const name = network.name || network.cluster_id;
            const active = activeLobby === network.fingerprint;
            return (
              <span key={network.fingerprint} className={`flex shrink-0 items-center rounded-md ${active ? "bg-iris/15 text-fg" : "text-muted hover:text-fg"}`}>
                <button type="button" role="tab" aria-selected={active} onClick={() => setActiveLobby(network.fingerprint)} className="flex items-center gap-1.5 py-1 pl-2.5 pr-1 text-xs"><MessageCircle className="h-3.5 w-3.5" />{name}</button>
                <button type="button" aria-label={t("publicNetworks.closeLobby", { name })} title={t("publicNetworks.closeLobby", { name })} onClick={() => closeLobby(network.fingerprint)} className="rounded p-1 hover:text-rose"><X className="h-3 w-3" /></button>
              </span>
            );
          })}
        </div>
      )}
      {lobbies.map(({ network, focusAdmission }) => (
        <div key={network.fingerprint} className={activeLobby === network.fingerprint ? "min-h-0 flex-1 bg-surface/35" : "hidden"}>
          <LobbyChat layout="page" backLabel={t("publicNetworks.backToNetworks")} headerActions={lobbies.length > 1 ? lobbySwitcher : undefined} focusAdmission={focusAdmission} network={{ name: network.name || network.cluster_id, publisher_key: network.publisher_key, accepted_enrollments: network.accepted_enrollments }} endpoint="/api/ui/network-chat/lobby" onClose={() => setActiveLobby(null)} onUnavailable={() => lobbyUnavailable(network)} />
        </div>
      ))}
      <div className={activeLobby ? "hidden" : "flex min-h-0 flex-1 overflow-hidden border-y border-line bg-surface/35"}>
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <div className="flex shrink-0 flex-wrap items-end justify-between gap-3 border-b border-line bg-raised/60 px-4 py-3 sm:px-5">
            <div className="flex min-w-0 items-center gap-3">
              <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-iris/35 bg-iris/12 text-iris"><Globe2 className="h-4 w-4" /></span>
              <div className="min-w-0"><h2 className="font-display text-sm font-semibold text-fg">{t("publicNetworks.discoveredTitle")}</h2><p className="mt-0.5 text-xs text-muted">{t("publicNetworks.discoveredDetail")}</p></div>
            </div>
            <div className="flex min-w-0 flex-wrap items-end gap-2">
              <label className="flex w-[14rem] min-w-[10rem] flex-col gap-1">
                <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("publicNetworks.search")}</span>
                <span className="relative block">
                  <Search className="pointer-events-none absolute left-2 top-1/2 h-3 w-3 -translate-y-1/2 text-faint" aria-hidden="true" />
                  <Input type="search" aria-label={t("publicNetworks.search")} placeholder={t("publicNetworks.searchPlaceholder")} value={search} onChange={(event) => setSearch(event.target.value)} className="h-8 rounded-md pl-6 text-xs" />
                </span>
              </label>
              <label className="flex w-[10rem] min-w-[8rem] flex-col gap-1">
                <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("publicNetworks.country")}</span>
                <Select aria-label={t("publicNetworks.country")} compact value={countryFilter} onChange={(event) => { setCountryFilter(event.target.value); setCityFilter(""); }}>
                  <option value="">{t("publicNetworks.allCountries")}</option>
                  {countryOptions.map((country) => <option key={country.code} value={country.code}>{country.name}</option>)}
                </Select>
              </label>
              <label className="flex w-[10rem] min-w-[8rem] flex-col gap-1">
                <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("publicNetworks.city")}</span>
                <Select aria-label={t("publicNetworks.city")} compact value={cityFilter} disabled={cityOptions.length === 0} onChange={(event) => setCityFilter(event.target.value)}>
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
          {body}
          {known && total > 0 && (
            <div className="flex shrink-0 flex-wrap items-center justify-between gap-2 border-t border-line bg-raised/40 px-4 py-2 text-[0.7rem] text-muted sm:px-5" data-testid="public-networks-pagination">
              <span className="flex items-center gap-2">
                {t("publicNetworks.pageRange", { from: firstShown, to: lastShown, total })}
                {refreshing && <span className="flex items-center gap-1 text-amber"><LoaderCircle className="h-3 w-3 animate-spin" aria-hidden="true" />{t("publicNetworks.updatingLive")}</span>}
              </span>
              <span className="flex items-center gap-2">
                <Button size="sm" variant="ghost" aria-label={t("publicNetworks.previousPage")} disabled={page <= 1} onClick={() => setPage((value) => Math.max(1, value - 1))} icon={<ChevronLeft className="h-3.5 w-3.5" />} />
                <span>{t("publicNetworks.pageOf", { page, pages })}</span>
                <Button size="sm" variant="ghost" aria-label={t("publicNetworks.nextPage")} disabled={page >= pages} onClick={() => setPage((value) => Math.min(pages, value + 1))} icon={<ChevronRight className="h-3.5 w-3.5" />} />
              </span>
            </div>
          )}
        </div>
        {selectedRow && (
          <NetworkSidePanel
            row={selectedRow}
            membership={membershipOf(selectedRow.kind)}
            leaving={leaving}
            onClose={() => setSelected(null)}
            onExpand={() => setDetailsOpen(true)}
            onLobby={(focusAdmission) => openLobby(selectedRow.network, focusAdmission)}
            onLeave={() => setLeaveConfirm(true)}
            onExport={() => exportTrustDocument(selectedRow.network)}
            onRemove={() => setRemoveTarget(selectedRow.network)}
            onQuickSetup={selectedRow.kind === "own" && canQuickSetup ? () => setQuickSetupOpen(true) : undefined}
            t={t}
          />
        )}
      </div>
      <div className={activeLobby ? "hidden" : "flex shrink-0 flex-wrap items-center gap-x-5 gap-y-2 px-1 py-3 text-xs leading-relaxed text-faint"}><span data-testid="distance-origin">{t(browserLocation ? "publicNetworks.distanceFromBrowser" : ownLocation ? "publicNetworks.distanceFromNetwork" : "publicNetworks.distanceFromNone")}</span><span>{t("publicNetworks.locationNote")}</span><span className="flex items-center gap-2"><ShieldCheck className="h-3.5 w-3.5 shrink-0" />{t("publicNetworks.securityNote")}</span></div>
      <Confirm open={leaveConfirm} title={t("network.forgetNetworkTitle")} description={t("network.forgetNetworkDescription")} confirmLabel={t("network.leave")} tone="danger" busy={leaving} onConfirm={() => void leave()} onCancel={() => !leaving && setLeaveConfirm(false)} />
      <LobbyChat network={requestLobby && { name: requestLobby.name || requestLobby.cluster_id, publisher_key: requestLobby.publisher_key, accepted_enrollments: requestLobby.accepted_enrollments }} focusAdmission endpoint="/api/ui/network-chat/lobby" onClose={() => setRequestLobby(null)} />
      <Confirm open={removeTarget !== null} title={t("publicNetworks.removeTitle")} description={t("publicNetworks.removeDescription", { name: removeTarget?.name || removeTarget?.cluster_id || "" })} confirmLabel={t("publicNetworks.remove")} tone="danger" busy={removing} onConfirm={() => void removeCustom()} onCancel={() => !removing && setRemoveTarget(null)} />
      <NetworkQuickSetup
        open={quickSetupOpen}
        network={ownSettings}
        onClose={() => setQuickSetupOpen(false)}
        onSaved={(saved) => {
          setQuickSetupOpen(false);
          toast.success(t(saved.network.visibility === "public" ? "quickSetup.published" : "network.infoSaved"), t(saved.network.visibility === "public" ? "quickSetup.publishedDetail" : "network.infoSavedDetail"));
          invalidate();
          void networkInfo.refetch();
          void refreshLive();
        }}
        t={t}
      />
      <PublicNetworkDetails network={detailsOpen ? selectedRow?.network ?? null : null} visibility={selectedRow?.visibility ?? "public"} onClose={() => setDetailsOpen(false)} t={t} />
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

function NetworkRow({ network, kind, visibility, selected, cachedSince, onExport, onRemove, distance, latency, stats: localStats, membership, leaving, onLeave, onChat, onQuickSetup, onSelect, t, refreshKey }: { network: PublicNetwork; kind: RowKind; visibility: Visibility; selected: boolean; cachedSince: number | undefined; onExport: () => void; onRemove: () => void; distance: number | null; latency: PublicLatency | undefined; stats: NetworkStats | null; membership: Membership; leaving: boolean; onLeave: () => void; onChat: () => void; onQuickSetup?: () => void; onSelect: () => void; t: T; refreshKey: number }) {
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
  const tone = selected
    ? "bg-iris/[0.12] shadow-[inset_3px_0_0_rgb(var(--iris)/0.9)] hover:bg-iris/[0.15]"
    : kind === "custom"
    ? "bg-amber/[0.05] shadow-[inset_3px_0_0_rgb(var(--amber)/0.7)] hover:bg-amber/[0.09]"
    : kind === "own"
      ? "bg-aqua/[0.05] shadow-[inset_3px_0_0_rgb(var(--aqua)/0.7)] hover:bg-aqua/[0.09]"
      : "hover:bg-raised/45";
  return <tr data-testid={`network-row-${network.fingerprint}`} data-network-kind={kind} aria-selected={selected} onClick={onSelect} className={`cursor-pointer align-top transition-colors ${tone}`}><td className="max-w-[15rem] px-4 py-3.5">{kind !== "public" && <div className="mb-1.5">{kind === "own" ? <Badge tone="aqua"><House className="h-3 w-3" />{t("publicNetworks.kindOwn")}</Badge> : <Badge tone="amber"><Lock className="h-3 w-3" />{t("publicNetworks.kindCustom")}</Badge>}</div>}<button type="button" className="group text-left" onClick={(event) => { event.stopPropagation(); onSelect(); }} aria-label={t("publicNetworks.openDetails", { name: network.name || network.cluster_id })}><VisibilityIcon visibility={visibility} t={t} /><span className="font-medium text-fg group-hover:text-iris">{network.name || network.cluster_id}</span>{distance !== null && <span className="ml-2 whitespace-nowrap text-[0.65rem] text-lime">{t((network.location?.accuracy_m ?? 0) >= 1000 ? "publicNetworks.distanceApproximate" : "publicNetworks.distance", { value: distance.toFixed(0) })}</span>}{closed ? <span className="ml-2 whitespace-nowrap text-[0.65rem] text-rose">{t("publicNetworks.closedShort")}</span> : kind !== "own" && <EnrollmentIcons roles={network.accepted_enrollments} t={t} />}{cachedSince !== undefined && <p className="mt-1 text-[0.62rem] text-faint" data-testid={`network-cached-${network.fingerprint}`} title={t("publicNetworks.cachedDetail")}>{t("publicNetworks.cached", { when: since(new Date(cachedSince * 1000).toISOString()) })}</p>}</button></td><td className="max-w-[13rem] px-4 py-3.5 text-muted">{network.organization || unavailable}</td><td className="max-w-[16rem] px-4 py-3.5 text-muted">{location}</td><td className="max-w-[18rem] px-4 py-3.5"><ReachabilityIndicator id={network.fingerprint} status={reachability} t={t} /></td><td className="px-4 py-3.5 font-mono text-muted">{stats ? stats.nodeCount : unavailable}</td><td className="whitespace-nowrap px-4 py-3.5 font-mono text-muted" data-testid={`public-network-latency-${network.fingerprint}`}>{latencyText}</td><td className="px-4 py-3.5 font-mono text-muted">{stats?.vramTotalMb != null ? `${(stats.vramTotalMb / 1024).toFixed(1)} GB` : unavailable}</td><td className="max-w-[18rem] px-4 py-3.5 text-muted">{models}</td><td className="px-4 py-3.5"><div className="flex flex-wrap items-center gap-2">{onQuickSetup && <Button size="sm" variant="primary" data-testid="network-quick-setup-row" onClick={(event) => { event.stopPropagation(); onQuickSetup(); }} icon={<WandSparkles className="h-3.5 w-3.5" />}>{t("quickSetup.action")}</Button>}<MembershipAction network={network} membership={membership} leaving={leaving} onChat={onChat} onLeave={onLeave} t={t} />{kind === "own" && <Button size="sm" variant="ghost" aria-label={t("publicNetworks.exportTrust", { name: network.name || network.cluster_id })} title={t("publicNetworks.exportTrustDetail")} disabled={!network.trust_document} onClick={(event) => { event.stopPropagation(); onExport(); }} icon={<Download className="h-3.5 w-3.5" />} />}{kind === "custom" && <Button size="sm" variant="ghost" aria-label={t("publicNetworks.removeNamed", { name: network.name || network.cluster_id })} title={t("publicNetworks.remove")} onClick={(event) => { event.stopPropagation(); onRemove(); }} icon={<Trash2 className="h-3.5 w-3.5" />} />}</div></td></tr>;
}

/** Join (or just talk, when closed) for another network; Leave for a member of this one; nothing for its principal. */
function MembershipAction({ network, membership, leaving, onChat, onLeave, t }: { network: PublicNetwork; membership: Membership; leaving: boolean; onChat: () => void; onLeave: () => void; t: T }) {
  const closed = network.accepted_enrollments?.length === 0;
  // Without the network's Chat there is no talking, only the request.
  if (membership === "none" && !network.lobby_chat) return <Button size="sm" variant="primary" data-guide={!closed ? "network-join" : undefined} disabled={closed} title={closed ? t("publicNetworks.closedDetail") : undefined} onClick={(event) => { event.stopPropagation(); onChat(); }} icon={<LogIn className="h-3.5 w-3.5" />}>{t("publicNetworks.askToJoin")}</Button>;
  if (membership === "none") return <Button size="sm" variant="primary" data-guide={!closed ? "network-join" : undefined} title={closed ? t("publicNetworks.closedDetail") : undefined} onClick={(event) => { event.stopPropagation(); onChat(); }} icon={<MessageCircle className="h-3.5 w-3.5" />}>{t(closed ? "publicNetworks.chat" : "publicNetworks.join")}</Button>;
  if (membership === "member") return <Button size="sm" variant="danger" loading={leaving} disabled={leaving} onClick={(event) => { event.stopPropagation(); onLeave(); }} icon={<LogOut className="h-3.5 w-3.5" />}>{t("network.leave")}</Button>;
  return null;
}

/**
 * The side panel of the selected network, drawn like the Dashboard's property
 * inspector. The row already shows organization, place, reach and stats, so
 * the panel adds only what the network's owner wrote for people to read -
 * whether it takes new members, description, channels and rules - and leaves
 * keys, fingerprints and relays to the full view. A field the owner left
 * empty is not drawn. Every action for the network sits at the bottom.
 */
function NetworkSidePanel({ row, membership, leaving, onClose, onExpand, onLobby, onLeave, onExport, onRemove, onQuickSetup, t }: { row: Row; membership: Membership; leaving: boolean; onClose: () => void; onExpand: () => void; onLobby: (focusAdmission: boolean) => void; onLeave: () => void; onExport: () => void; onRemove: () => void; onQuickSetup?: () => void; t: T }) {
  const { network, kind } = row;
  const name = network.name || network.cluster_id;
  const closed = network.accepted_enrollments?.length === 0;
  return (
    <aside className="fixed inset-y-0 right-0 z-40 flex w-full max-w-[21rem] shrink-0 flex-col overflow-hidden border-l border-line bg-surface shadow-lift lg:static lg:z-auto lg:bg-surface/80 lg:shadow-none" aria-label={t("publicNetworks.panelLabel", { name })} data-testid="network-side-panel">
      <div className="flex items-start justify-between gap-2 border-b border-line bg-surface px-3 py-2">
        <h3 className="min-w-0 break-words font-display text-sm font-semibold leading-snug text-fg">{name}</h3>
        <div className="flex shrink-0 items-center gap-0.5">
          <button type="button" aria-label={t("publicNetworks.expandDetails")} title={t("publicNetworks.expandDetails")} onClick={onExpand} className="rounded p-1 text-muted hover:text-fg"><Maximize2 className="h-3.5 w-3.5" /></button>
          <button type="button" aria-label={t("publicNetworks.closePanel")} title={t("publicNetworks.closePanel")} onClick={onClose} className="rounded p-1 text-muted hover:text-fg"><X className="h-3.5 w-3.5" /></button>
        </div>
      </div>
      <div className="min-h-0 flex-1 space-y-3 overflow-y-auto overscroll-y-contain px-3 py-3 text-[0.72rem] leading-relaxed">
        <div className={`rounded-md border px-2 py-1.5 ${closed ? "border-rose/30 bg-rose/[0.06] text-rose" : "border-lime/30 bg-lime/[0.06] text-lime"}`} data-testid="network-enrollment-state">
          <p className="font-semibold">{t(closed ? "publicNetworks.enrollmentClosed" : "publicNetworks.enrollmentOpen")}</p>
          <ul className="mt-1 space-y-0.5" data-testid="network-enrollment-roles">
            {ENROLLMENT_ROLES.map((role) => {
              const Icon = ENROLLMENT_ICONS[role];
              const accepted = !network.accepted_enrollments || network.accepted_enrollments.includes(role);
              return (
                <li key={role} data-role={role} data-accepted={accepted} className={`flex items-center gap-1.5 text-[0.68rem] ${accepted ? "text-fg" : "text-faint line-through"}`}>
                  <Icon className={`h-3.5 w-3.5 shrink-0 ${accepted ? "text-lime" : "text-faint"}`} aria-hidden="true" />
                  <span>{t(`network.enrollment.${role}`)}</span>
                  <span className={`ml-auto ${accepted ? "text-lime" : "text-rose"}`}>{t(accepted ? "publicNetworks.roleAccepted" : "publicNetworks.roleNotAccepted")}</span>
                </li>
              );
            })}
          </ul>
        </div>
        {network.description && <PanelField label={t("network.description")}><Markdown compact>{network.description}</Markdown></PanelField>}
        {network.channels && <PanelField label={t("network.channels")}><Markdown compact>{network.channels}</Markdown></PanelField>}
        {network.rules && <PanelField label={t("network.rules")}><Markdown compact>{network.rules}</Markdown></PanelField>}
      </div>
      <div className="shrink-0 space-y-2 border-t border-line bg-raised/60 px-3 py-2.5" data-testid="network-panel-actions">
        {onQuickSetup && <Button size="sm" variant="primary" className="w-full" onClick={onQuickSetup} icon={<WandSparkles className="h-3.5 w-3.5" />}>{t("quickSetup.action")}</Button>}
        {membership === "none" && (
          <div className="flex flex-wrap gap-2">
            {(!closed || !network.lobby_chat) && <Button size="sm" variant="primary" disabled={closed} title={closed ? t("publicNetworks.closedDetail") : undefined} onClick={() => onLobby(true)} icon={<LogIn className="h-3.5 w-3.5" />}>{t("publicNetworks.askToJoin")}</Button>}
            {network.lobby_chat && <Button size="sm" variant={closed ? "primary" : "outline"} onClick={() => onLobby(false)} icon={<MessageCircle className="h-3.5 w-3.5" />}>{t("publicNetworks.lobby")}</Button>}
          </div>
        )}
        {membership === "member" && <Button size="sm" variant="danger" loading={leaving} disabled={leaving} onClick={onLeave} icon={<LogOut className="h-3.5 w-3.5" />}>{t("network.leave")}</Button>}
        <div className="flex flex-wrap gap-1">
          <Button size="sm" variant="ghost" onClick={onExpand} icon={<Maximize2 className="h-3.5 w-3.5" />}>{t("publicNetworks.moreDetails")}</Button>
          {kind === "own" && <Button size="sm" variant="ghost" disabled={!network.trust_document} title={t("publicNetworks.exportTrustDetail")} onClick={onExport} icon={<Download className="h-3.5 w-3.5" />}>{t("publicNetworks.exportShort")}</Button>}
          {kind === "custom" && <Button size="sm" variant="ghost" onClick={onRemove} icon={<Trash2 className="h-3.5 w-3.5" />}>{t("publicNetworks.remove")}</Button>}
        </div>
      </div>
    </aside>
  );
}

function PanelField({ label, children }: { label: string; children: ReactNode }) {
  return <section className="min-w-0"><p className="eyebrow mb-0.5">{label}</p><div className="min-w-0 break-words">{children}</div></section>;
}

/** Everything known about a network, technical fields included, nearly full screen. */
function PublicNetworkDetails({ network, visibility, onClose, t }: { network: PublicNetwork | null; visibility: Visibility; onClose: () => void; t: T }) {
  const unavailable = t("publicNetworks.unavailable");
  const value = (text: string) => text || unavailable;
  const location = network?.location ? `${network.location.latitude}, ${network.location.longitude}` : unavailable;
  if (!network) return null;
  return (
    <Modal open title={t("publicNetworks.detailsTitle", { name: network.name || network.cluster_id })} onClose={onClose} size="full" footer={<Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>}>
      <div className="grid gap-6 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <div className="space-y-5">
          <DetailBlock label={t("network.description")}><Markdown>{network.description || unavailable}</Markdown></DetailBlock>
          <DetailBlock label={t("network.rules")}><Markdown>{network.rules || unavailable}</Markdown></DetailBlock>
          <DetailBlock label={t("network.channels")}><Markdown>{network.channels || unavailable}</Markdown></DetailBlock>
        </div>
        <div className="space-y-5">
          <div className="grid gap-4 sm:grid-cols-2">
            <DetailBlock label={t("network.organization")}><span>{value(network.organization)}</span></DetailBlock>
            <DetailBlock label={t("publicNetworks.place")}><span>{value(placeOf(network))}</span></DetailBlock>
            <DetailBlock label={t("network.acceptedEnrollments")}><span>{acceptedEnrollmentsLabel(network.accepted_enrollments, t)}</span></DetailBlock>
            <DetailBlock label={t("network.accessPolicy")}><span>{accessPolicyLabel(network.access_policy, t)}</span></DetailBlock>
            <DetailBlock label={t("network.visibility")}><span>{t(visibility === "private" ? "network.visibilityPrivate" : visibility === "local" ? "network.visibilityLocal" : "network.visibilityPublic")}</span></DetailBlock>
            <DetailBlock label={t("publicNetworks.location")}><span>{location}</span></DetailBlock>
            <DetailBlock label={t("publicNetworks.discoveredVia")}><span>{value(network.discovered_via)}</span></DetailBlock>
            <DetailBlock label={t("publicNetworks.geoLabel")}><span className="break-all font-mono text-xs">{value(network.geo_label)}</span></DetailBlock>
          </div>
          <div className="grid gap-4 border-t border-line pt-4">
            <DetailBlock label={t("publicNetworks.fingerprint")}><span className="break-all font-mono text-xs">{value(network.fingerprint)}</span></DetailBlock>
            <DetailBlock label={t("publicNetworks.publisherKey")}><span className="break-all font-mono text-xs">{value(network.publisher_key)}</span></DetailBlock>
            <DetailBlock label={t("publicNetworks.relayHints")}><span className="break-all font-mono text-xs">{value(network.relay_hints?.join(", ") ?? "")}</span></DetailBlock>
            <DetailBlock label={t("publicNetworks.directoryReplicas")}><span className="break-words">{value(network.directory_replicas?.join(", ") ?? "")}</span></DetailBlock>
          </div>
          <details className="border-t border-line pt-4">
            <summary className="cursor-pointer text-sm font-medium text-fg">{t("network.trustDocument")}</summary>
            <pre className="mt-3 max-h-72 overflow-auto rounded-lg border border-line bg-ink p-3 text-[0.68rem] leading-relaxed text-muted">{network.trust_document || unavailable}</pre>
          </details>
        </div>
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

/** How the network is discovered, with the icon Network settings uses for the same choice. */
function VisibilityIcon({ visibility, t }: { visibility: Visibility; t: T }) {
  const Icon = VISIBILITY_ICONS[visibility];
  const label = `${t("network.visibility")}: ${t(VISIBILITY_LABELS[visibility])}`;
  const tone = visibility === "public" ? "text-aqua" : visibility === "private" ? "text-amber" : "text-faint";
  return <Icon className={`mr-1.5 inline-block h-3.5 w-3.5 -translate-y-px align-middle ${tone}`} aria-label={label} data-testid="network-visibility-icon" data-visibility={visibility}><title>{label}</title></Icon>;
}

/** The roles a network takes, as icons next to its name; the names are in the tooltip. */
function EnrollmentIcons({ roles, t }: { roles: EnrollmentRole[] | undefined; t: T }) {
  return (
    <span className="ml-2 inline-flex items-center gap-1 align-middle" data-testid="network-enrollment-icons">
      {ENROLLMENT_ROLES.filter((role) => !roles || roles.includes(role)).map((role) => {
        const Icon = ENROLLMENT_ICONS[role];
        const label = t("publicNetworks.roleAcceptedNamed", { role: t(`network.enrollment.${role}`) });
        return <Icon key={role} className="h-3 w-3 text-lime" aria-label={label}><title>{label}</title></Icon>;
      })}
    </span>
  );
}

/** Lower case without accents, like the node folds its search text. */
function foldText(text: string): string {
  return text.normalize("NFD").replace(/[\u0300-\u036f]/g, "").toLowerCase();
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
