import { useEffect, useMemo, useState } from "react";
import { ArrowRight, Globe2, Link2, RefreshCw, ShieldCheck } from "lucide-react";
import { Confirm } from "../components/Confirm";
import { Badge, Button, ErrorState, PageHeader, Panel, Skeleton } from "../components/ui";
import { api } from "../lib/api";
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
  const [networks, setNetworks] = useState<PublicNetwork[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [viewerLocation, setViewerLocation] = useState<ViewerLocation | null>(null);
  const [joinTarget, setJoinTarget] = useState<PublicNetwork | null>(null);
  const [joining, setJoining] = useState(false);

  async function load() {
    setRefreshing(true);
    setError(null);
    try {
      setNetworks(await api.get<PublicNetwork[]>("/api/public/networks", { auth: false }));
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

  const orderedNetworks = useMemo(
    () => [...(networks ?? [])].sort((left, right) => {
      const leftDistance = distanceKm(left.location, viewerLocation);
      const rightDistance = distanceKm(right.location, viewerLocation);
      if (leftDistance === null && rightDistance === null) return 0;
      if (leftDistance === null) return 1;
      if (rightDistance === null) return -1;
      return leftDistance - rightDistance;
    }),
    [networks, viewerLocation],
  );
  const groupedNetworks = useMemo(() => {
    const groups = new Map<string, { country: string; city: string; networks: PublicNetwork[] }>();
    for (const network of orderedNetworks) {
      const country = network.country_name || network.country_code || t("publicNetworks.unknownCountry");
      const city = network.city || t("publicNetworks.unknownCity");
      const key = `${country}:${city}`;
      const group = groups.get(key) ?? { country, city, networks: [] };
      group.networks.push(network);
      groups.set(key, group);
    }
    return [...groups.values()];
  }, [orderedNetworks, t]);

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
    <div className="min-w-0">
      <PageHeader
        title={t("publicNetworks.title")}
        subtitle={t("publicNetworks.subtitle")}
        actions={
          <Button size="sm" loading={refreshing} onClick={() => void load()} icon={<RefreshCw className="h-3.5 w-3.5" />}>
            {t("publicNetworks.refresh")}
          </Button>
        }
      />
      <Panel className="overflow-hidden">
        <div className="flex flex-wrap items-center gap-3 border-b border-line px-5 py-4">
          <span className="flex h-9 w-9 items-center justify-center rounded-lg border border-iris/35 bg-iris/12 text-iris"><Globe2 className="h-4 w-4" /></span>
          <div className="min-w-0"><h2 className="font-display text-sm font-semibold text-fg">{t("publicNetworks.discoveredTitle")}</h2><p className="mt-1 text-xs text-muted">{t("publicNetworks.discoveredDetail")}</p></div>
        </div>
        {error ? <div className="px-5 py-5"><ErrorState title={t("publicNetworks.failed")} error={error} onRetry={() => void load()} /></div> : networks === null ? <div className="space-y-3 px-5 py-5"><Skeleton className="h-20 w-full" /><Skeleton className="h-20 w-full" /></div> : networks.length === 0 ? <div className="px-5 py-12 text-center"><Globe2 className="mx-auto h-7 w-7 text-faint" /><p className="mt-3 text-sm font-medium text-fg">{t("publicNetworks.emptyTitle")}</p><p className="mx-auto mt-1 max-w-md text-sm leading-relaxed text-muted">{t("publicNetworks.emptyDetail")}</p></div> : <div className="divide-y divide-line">{groupedNetworks.map((group) => <section key={`${group.country}:${group.city}`}><div className="flex items-center gap-2 bg-raised px-5 py-3"><MapPinIcon /> <span className="text-xs font-semibold uppercase tracking-[0.14em] text-faint">{group.city}</span><span className="text-xs text-muted">{group.country}</span></div>{group.networks.map((network) => <NetworkRow key={`${network.cluster_id}:${network.fingerprint}`} network={network} distance={distanceKm(network.location, viewerLocation)} joining={joining && joinTarget?.cluster_id === network.cluster_id} onJoin={() => setJoinTarget(network)} t={t} />)}</section>)}</div>}
      </Panel>
      <p className="mt-3 text-xs leading-relaxed text-faint">{t("publicNetworks.locationNote")}</p>
      <p className="mt-4 flex items-center gap-2 text-xs leading-relaxed text-faint"><ShieldCheck className="h-3.5 w-3.5 shrink-0" />{t("publicNetworks.securityNote")}</p>
      <Confirm open={joinTarget !== null} title={t("publicNetworks.joinTitle", { name: joinTarget?.name ?? "" })} description={t("publicNetworks.joinDetail")} confirmLabel={t("publicNetworks.joinConfirm")} busy={joining} onConfirm={() => joinTarget && void join(joinTarget)} onCancel={() => !joining && setJoinTarget(null)} />
    </div>
  );
}

function NetworkRow({ network, distance, joining, onJoin, t }: { network: PublicNetwork; distance: number | null; joining: boolean; onJoin: () => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  return <article className="flex min-w-0 flex-col gap-4 px-5 py-5 lg:flex-row lg:items-center lg:justify-between"><div className="min-w-0"><div className="flex min-w-0 flex-wrap items-center gap-2"><h3 className="min-w-0 break-words font-display text-base font-semibold text-fg">{network.name || network.cluster_id}</h3><Badge tone="aqua"><Link2 className="h-3 w-3" />{t("publicNetworks.pkarr")}</Badge>{distance !== null && <Badge tone="lime">{t("publicNetworks.distance", { value: distance.toFixed(0) })}</Badge>}</div>{network.organization && <p className="mt-1 text-sm text-muted">{network.organization}</p>}{network.description && <p className="mt-2 max-w-2xl text-sm leading-relaxed text-muted">{network.description}</p>}<dl className="mt-3 grid min-w-0 gap-2 text-xs sm:grid-cols-3"><div className="min-w-0"><dt className="text-faint">{t("publicNetworks.fingerprint")}</dt><dd className="mt-1 break-all font-mono text-muted">{network.fingerprint}</dd></div><div className="min-w-0"><dt className="text-faint">{t("publicNetworks.address")}</dt><dd className="mt-1 break-all font-mono text-muted">{network.address}</dd></div><div className="min-w-0"><dt className="text-faint">{t("publicNetworks.geoLabel")}</dt><dd className="mt-1 break-all font-mono text-muted">{network.geo_label || "-"}</dd></div></dl></div><Button variant="primary" className="shrink-0" loading={joining} disabled={joining} onClick={onJoin} icon={<ArrowRight className="h-4 w-4" />}>{t("publicNetworks.join")}</Button></article>;
}

function MapPinIcon() {
  return <span className="h-1.5 w-1.5 rounded-full bg-iris" aria-hidden="true" />;
}

function distanceKm(location: PublicNetwork["location"], viewer: ViewerLocation | null): number | null {
  if (!location || !viewer) return null;
  const earthRadiusKm = 6371;
  const latitude = (location.latitude - viewer.latitude) * Math.PI / 180;
  const longitude = (location.longitude - viewer.longitude) * Math.PI / 180;
  const a = Math.sin(latitude / 2) ** 2 + Math.cos(viewer.latitude * Math.PI / 180) * Math.cos(location.latitude * Math.PI / 180) * Math.sin(longitude / 2) ** 2;
  return earthRadiusKm * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
}
