import { useEffect, useMemo, useState } from "react";
import { ArrowRight, Globe2, RefreshCw, ShieldCheck } from "lucide-react";
import { Confirm } from "../components/Confirm";
import { Button, ErrorState, PageHeader, Select, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { networkStats, type NetworkStats } from "../lib/networkStats";
import { useCluster } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

interface PublicNetwork {
  cluster_id: string;
  name: string;
  fingerprint: string;
  address: string;
  bootstrap_address: string;
  trust_document: string;
  organization: string;
  description: string;
  country_code: string;
  country_name: string;
  city: string;
  geo_label: string;
  access_policy: "authorized" | "anyone_with_anchor" | "local_only";
  location: { latitude: number; longitude: number; accuracy_m?: number } | null;
  discovered_via: string;
}

interface ViewerLocation {
  latitude: number;
  longitude: number;
}

export function PublicNetworks() {
  const { t } = useI18n();
  const toast = useToast();
  const cluster = useCluster();
  const [networks, setNetworks] = useState<PublicNetwork[] | null>(null);
  const [localNetwork, setLocalNetwork] = useState<PublicNetwork | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [viewerLocation, setViewerLocation] = useState<ViewerLocation | null>(null);
  const [joinTarget, setJoinTarget] = useState<PublicNetwork | null>(null);
  const [joining, setJoining] = useState(false);
  const [countryFilter, setCountryFilter] = useState("");
  const [cityFilter, setCityFilter] = useState("");

  async function load() {
    setRefreshing(true);
    setError(null);
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

  async function join(network: PublicNetwork) {
    setJoining(true);
    try {
      sessionStorage.setItem("pom.public-network-join", JSON.stringify(network));
      await api.post("/api/ui/network/leave");
      window.setTimeout(() => window.location.assign("/setup"), 1500);
    } catch (reason) {
      sessionStorage.removeItem("pom.public-network-join");
      toast.fromError(t("publicNetworks.joinFailed"), reason);
      setJoining(false);
      setJoinTarget(null);
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
        {error ? <div className="px-5 py-5"><ErrorState title={t("publicNetworks.failed")} error={error} onRetry={() => void load()} /></div> : networks === null ? <div className="space-y-3 px-5 py-5"><Skeleton className="h-10 w-full" /><Skeleton className="h-10 w-full" /></div> : filteredNetworks.length === 0 ? <div className="px-5 py-12 text-center"><Globe2 className="mx-auto h-7 w-7 text-faint" /><p className="mt-3 text-sm font-medium text-fg">{t(networks.length === 0 ? "publicNetworks.emptyTitle" : "publicNetworks.noMatches")}</p><p className="mx-auto mt-1 max-w-md text-sm leading-relaxed text-muted">{t(networks.length === 0 ? "publicNetworks.emptyDetail" : "publicNetworks.noMatchesDetail")}</p></div> : <div className="min-h-0 flex-1 overflow-auto"><table className="min-w-[1100px] w-full border-collapse text-left text-xs"><thead className="sticky top-0 z-10 bg-raised text-[0.66rem] uppercase tracking-[0.12em] text-faint"><tr>{["name", "organization", "location", "geoLabel", "nodes", "latency", "vram", "models", "action"].map((column) => <th key={column} className="border-b border-line px-4 py-3 font-medium">{t(`publicNetworks.${column}`)}</th>)}</tr></thead><tbody className="divide-y divide-line">{orderedNetworks.map((network) => <NetworkRow key={`${network.cluster_id}:${network.fingerprint}`} network={network} distance={distanceKm(network.location, viewerLocation)} stats={localNetwork?.fingerprint === network.fingerprint ? metrics : null} joining={joining && joinTarget?.cluster_id === network.cluster_id} onJoin={() => setJoinTarget(network)} t={t} />)}</tbody></table></div>}
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-x-5 gap-y-2 px-1 py-3 text-xs leading-relaxed text-faint"><span>{t("publicNetworks.locationNote")}</span><span className="flex items-center gap-2"><ShieldCheck className="h-3.5 w-3.5 shrink-0" />{t("publicNetworks.securityNote")}</span></div>
      <Confirm open={joinTarget !== null} title={t("publicNetworks.joinTitle", { name: joinTarget?.name ?? "" })} description={t("publicNetworks.joinDetail")} confirmLabel={t("publicNetworks.joinConfirm")} busy={joining} onConfirm={() => joinTarget && void join(joinTarget)} onCancel={() => !joining && setJoinTarget(null)} />
    </div>
  );
}

function NetworkRow({ network, distance, stats, joining, onJoin, t }: { network: PublicNetwork; distance: number | null; stats: NetworkStats | null; joining: boolean; onJoin: () => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  const unavailable = t("publicNetworks.unavailable");
  const location = [network.country_name || network.country_code, network.city].filter(Boolean).join(" / ") || unavailable;
  const models = stats?.models.length ? stats.models.join(", ") : unavailable;
  return <tr className="align-top transition-colors hover:bg-raised/45"><td className="max-w-[15rem] px-4 py-3.5"><div className="flex items-center gap-2"><span className="font-medium text-fg">{network.name || network.cluster_id}</span>{distance !== null && <span className="whitespace-nowrap text-[0.65rem] text-lime">{t("publicNetworks.distance", { value: distance.toFixed(0) })}</span>}</div><p className="mt-1 break-all font-mono text-[0.64rem] text-faint">{network.fingerprint}</p></td><td className="max-w-[13rem] px-4 py-3.5 text-muted">{network.organization || unavailable}</td><td className="max-w-[13rem] px-4 py-3.5 text-muted">{location}</td><td className="max-w-[15rem] break-all px-4 py-3.5 font-mono text-[0.68rem] text-muted">{network.geo_label || unavailable}</td><td className="px-4 py-3.5 font-mono text-muted">{stats ? stats.nodeCount : unavailable}</td><td className="px-4 py-3.5 font-mono text-muted">{stats?.averageLatencyMs != null ? `${stats.averageLatencyMs.toFixed(1)} ms` : unavailable}</td><td className="px-4 py-3.5 font-mono text-muted">{stats?.vramTotalMb != null ? `${(stats.vramTotalMb / 1024).toFixed(1)} GB` : unavailable}</td><td className="max-w-[18rem] px-4 py-3.5 text-muted">{models}</td><td className="px-4 py-3.5"><Button size="sm" variant="primary" loading={joining} disabled={joining} onClick={onJoin} icon={<ArrowRight className="h-3.5 w-3.5" />}>{t("publicNetworks.join")}</Button></td></tr>;
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
