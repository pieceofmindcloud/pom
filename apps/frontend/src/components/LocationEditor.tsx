/**
 * Where a network is, as its administrator declares it.
 *
 * Three rules shape this component:
 *
 * - The browser's location is only asked for when the administrator presses
 *   the button. Nothing here runs `geolocation` on its own.
 * - The city is never typed. It is chosen among the cities the node's offline
 *   database returns, either around the placed point or by name.
 * - The point placed here is not what the network publishes: the node reduces
 *   it to the chosen precision before storing it, and this component only
 *   previews that.
 */
import { Crosshair, LocateFixed, MapPin } from "lucide-react";
import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Modal } from "./Confirm";
import { Button, Field, Input, Select, Skeleton } from "./ui";
import {
  geoCities,
  geoCountries,
  geoNearest,
  LOCATION_PRECISIONS,
  placeLabel,
  publishedPoint,
  type GeoCity,
  type GeoCountry,
  type GeoSource,
  type LocationPrecision,
  type NetworkPlace,
} from "../lib/geo";
import type { MapPoint } from "./LocationMap";

const LocationMap = lazy(() => import("./LocationMap"));

type Translate = (key: string, values?: Record<string, string | number>) => string;

interface Pin extends MapPoint {
  accuracy_m?: number | null;
}

const PRECISION_LABEL: Record<LocationPrecision, string> = {
  exact: "location.precisionExact",
  approximate: "location.precisionApproximate",
  city: "location.precisionCity",
};

const PRECISION_HINT: Record<LocationPrecision, string> = {
  exact: "location.precisionExactHint",
  approximate: "location.precisionApproximateHint",
  city: "location.precisionCityHint",
};

function coordinate(value: number): string {
  return value.toFixed(4);
}

export function LocationEditor({
  value,
  onChange,
  source,
  required = false,
  t,
}: {
  value: NetworkPlace;
  onChange: (next: NetworkPlace) => void;
  source: GeoSource;
  required?: boolean;
  t: Translate;
}) {
  const [open, setOpen] = useState(false);
  const precision = value.location_precision ?? "approximate";
  const chosen = value.city_id != null;
  const published = publishedPoint(value);
  return (
    <div className="grid min-w-0 gap-4 md:grid-cols-2" data-testid="location-editor">
      <Field label={t("location.title")} hint={required && !chosen ? t("location.required") : undefined}>
        <div className="flex min-w-0 flex-wrap items-center gap-3">
          <span className="flex min-w-0 items-center gap-2 text-sm text-fg">
            <MapPin className="h-4 w-4 shrink-0 text-faint" />
            <span className="break-words" data-testid="location-place">{chosen ? placeLabel(value) : t("location.none")}</span>
          </span>
          <Button size="sm" onClick={() => setOpen(true)} icon={<Crosshair className="h-3.5 w-3.5" />}>
            {t(chosen ? "location.change" : "location.choose")}
          </Button>
        </div>
      </Field>
      <Field label={t("location.precision")} hint={t(PRECISION_HINT[precision])}>
        <Select
          value={precision}
          data-testid="location-precision"
          onChange={(event) => onChange({ ...value, location_precision: event.target.value as LocationPrecision })}
        >
          {LOCATION_PRECISIONS.map((option) => <option key={option} value={option}>{t(PRECISION_LABEL[option])}</option>)}
        </Select>
      </Field>
      {chosen && (
        <p className="text-xs text-faint md:col-span-2" data-testid="location-published">
          {published
            ? t("location.published", { latitude: coordinate(published.latitude), longitude: coordinate(published.longitude) })
            : t("location.publishedCity", { city: value.city })}
        </p>
      )}
      <LocationDialog
        open={open}
        value={value}
        source={source}
        t={t}
        onClose={() => setOpen(false)}
        onConfirm={(city, pin) => {
          setOpen(false);
          onChange({
            ...value,
            city_id: city.id,
            country_code: city.country_code,
            country_name: city.country_name,
            state: city.state,
            city: city.name,
            location: { latitude: pin.latitude, longitude: pin.longitude, accuracy_m: pin.accuracy_m ?? null },
          });
        }}
      />
    </div>
  );
}

function LocationDialog({
  open,
  value,
  source,
  t,
  onClose,
  onConfirm,
}: {
  open: boolean;
  value: NetworkPlace;
  source: GeoSource;
  t: Translate;
  onClose: () => void;
  onConfirm: (city: GeoCity, pin: Pin) => void;
}) {
  const [pin, setPin] = useState<Pin | null>(null);
  const [nearby, setNearby] = useState<GeoCity[]>([]);
  const [selected, setSelected] = useState<GeoCity | null>(null);
  const [locating, setLocating] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [lookupFailed, setLookupFailed] = useState(false);
  const [mapFailed, setMapFailed] = useState(false);
  const [countries, setCountries] = useState<GeoCountry[]>([]);
  const [country, setCountry] = useState("");
  const [query, setQuery] = useState("");
  const [found, setFound] = useState<GeoCity[] | null>(null);
  const nearestRequest = useRef<AbortController | null>(null);

  /** The cities around a point, the closest one selected unless told otherwise. */
  function lookUp(point: Pin, keep?: GeoCity) {
    nearestRequest.current?.abort();
    const controller = new AbortController();
    nearestRequest.current = controller;
    geoNearest(source, point.latitude, point.longitude, controller.signal)
      .then((cities) => {
        setLookupFailed(false);
        const list = keep && !cities.some((city) => city.id === keep.id) ? [keep, ...cities] : cities;
        setNearby(list);
        setSelected(keep ?? list[0] ?? null);
      })
      .catch(() => {
        if (!controller.signal.aborted) setLookupFailed(true);
      });
  }

  function place(point: Pin, keep?: GeoCity) {
    setPin(point);
    lookUp(point, keep);
  }

  useEffect(() => {
    if (!open) return;
    setNotice(null);
    setMapFailed(false);
    setLookupFailed(false);
    setQuery("");
    setFound(null);
    setNearby([]);
    setSelected(null);
    setCountry(value.country_code);
    const current = value.location ? { latitude: value.location.latitude, longitude: value.location.longitude } : null;
    setPin(current);
    if (current) {
      nearestRequest.current?.abort();
      const controller = new AbortController();
      nearestRequest.current = controller;
      geoNearest(source, current.latitude, current.longitude, controller.signal)
        .then((cities) => {
          setNearby(cities);
          setSelected(cities.find((city) => city.id === value.city_id) ?? cities[0] ?? null);
        })
        .catch(() => {
          if (!controller.signal.aborted) setLookupFailed(true);
        });
    }
    const controller = new AbortController();
    geoCountries(source, controller.signal).then(setCountries).catch(() => {
      if (!controller.signal.aborted) setLookupFailed(true);
    });
    return () => {
      controller.abort();
      nearestRequest.current?.abort();
    };
    // Opening is the only moment the dialog reads the saved value.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const text = query.trim();
    if (!country || text.length < 2) {
      setFound(null);
      return;
    }
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      geoCities(source, country, text, controller.signal)
        .then((cities) => {
          setLookupFailed(false);
          setFound(cities);
        })
        .catch(() => {
          if (!controller.signal.aborted) setLookupFailed(true);
        });
    }, 200);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [open, country, query, source]);

  const canLocate = typeof window !== "undefined" && window.isSecureContext && Boolean(navigator.geolocation);

  function useBrowserLocation() {
    if (!canLocate) {
      setNotice(t("location.browserUnavailable"));
      return;
    }
    setLocating(true);
    setNotice(null);
    navigator.geolocation.getCurrentPosition(
      (position) => {
        setLocating(false);
        place({
          latitude: position.coords.latitude,
          longitude: position.coords.longitude,
          accuracy_m: position.coords.accuracy,
        });
      },
      () => {
        setLocating(false);
        setNotice(t("location.browserDenied"));
      },
      { enableHighAccuracy: true, maximumAge: 60000, timeout: 10000 },
    );
  }

  return (
    <Modal
      open={open}
      title={t("location.dialogTitle")}
      size="xl"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t("common.cancel")}</Button>
          <Button
            variant="primary"
            disabled={!pin || !selected}
            data-testid="location-confirm"
            onClick={() => {
              if (pin && selected) onConfirm(selected, pin);
            }}
          >
            {t("location.confirm")}
          </Button>
        </>
      }
    >
      <p>{t("location.dialogIntro")}</p>
      <p className="mt-2 text-xs text-faint">{t("location.privacyNote")}</p>
      <div className="mt-4 flex flex-wrap items-center gap-3">
        <Button size="sm" loading={locating} disabled={locating} data-testid="location-browser" onClick={useBrowserLocation} icon={<LocateFixed className="h-3.5 w-3.5" />}>
          {t(locating ? "location.locating" : "location.useBrowser")}
        </Button>
        {pin && <span className="font-mono text-xs text-faint">{coordinate(pin.latitude)}, {coordinate(pin.longitude)}</span>}
      </div>
      {notice && <p className="mt-2 text-xs text-amber" role="status">{notice}</p>}
      {lookupFailed && <p className="mt-2 text-xs text-rose" role="alert">{t("location.lookupFailed")}</p>}
      <div className="mt-4 grid gap-5 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <div className="min-w-0">
          <Suspense fallback={<Skeleton className="h-72 w-full" />}>
            <LocationMap pin={pin} label={t("location.mapLabel")} onPin={(point) => place(point)} onTilesFailed={() => setMapFailed(true)} />
          </Suspense>
          {mapFailed && <p className="mt-2 text-xs text-amber" role="status">{t("location.mapUnavailable")}</p>}
        </div>
        <div className="min-w-0 space-y-5">
          <section>
            <h3 className="text-xs font-medium uppercase tracking-[0.12em] text-faint">{t("location.nearest")}</h3>
            {nearby.length === 0 ? (
              <p className="mt-2 text-xs text-faint">{t("location.nearestEmpty")}</p>
            ) : (
              <ul className="mt-2 space-y-1" role="radiogroup" aria-label={t("location.nearest")}>
                {nearby.map((city) => (
                  <li key={city.id}>
                    <CityChoice city={city} selected={selected?.id === city.id} onSelect={() => setSelected(city)} t={t} />
                  </li>
                ))}
              </ul>
            )}
          </section>
          <section>
            <h3 className="text-xs font-medium uppercase tracking-[0.12em] text-faint">{t("location.searchTitle")}</h3>
            <div className="mt-2 grid gap-2">
              <Select aria-label={t("location.searchCountry")} value={country} onChange={(event) => { setCountry(event.target.value); setFound(null); }}>
                <option value="">{t("location.searchCountry")}</option>
                {countries.map((item) => <option key={item.code} value={item.code}>{item.name}</option>)}
              </Select>
              <Input
                aria-label={t("location.searchCity")}
                placeholder={t("location.searchCity")}
                value={query}
                disabled={!country}
                data-testid="location-search"
                onChange={(event) => setQuery(event.target.value)}
              />
            </div>
            {found && (found.length === 0 ? (
              <p className="mt-2 text-xs text-faint">{t("location.searchEmpty")}</p>
            ) : (
              <ul className="mt-2 max-h-44 space-y-1 overflow-y-auto">
                {found.map((city) => (
                  <li key={city.id}>
                    <CityChoice
                      city={city}
                      selected={selected?.id === city.id}
                      onSelect={() => place({ latitude: city.latitude, longitude: city.longitude }, city)}
                      t={t}
                    />
                  </li>
                ))}
              </ul>
            ))}
          </section>
        </div>
      </div>
      <p className="mt-4 text-[0.68rem] text-faint">{t("location.attribution")}</p>
    </Modal>
  );
}

function CityChoice({ city, selected, onSelect, t }: { city: GeoCity; selected: boolean; onSelect: () => void; t: Translate }) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onSelect}
      className={
        "flex w-full items-baseline justify-between gap-3 rounded-lg border px-3 py-2 text-left text-xs transition-colors " +
        (selected ? "border-iris bg-iris/10 text-fg" : "border-line text-muted hover:border-line-strong")
      }
    >
      <span className="min-w-0 break-words">
        <span className="font-medium text-fg">{city.name}</span>
        <span className="text-faint"> - {[city.state, city.country_name].filter(Boolean).join(", ")}</span>
      </span>
      {city.distance_km != null && <span className="shrink-0 font-mono text-faint">{t("location.distance", { value: city.distance_km.toFixed(0) })}</span>}
    </button>
  );
}
