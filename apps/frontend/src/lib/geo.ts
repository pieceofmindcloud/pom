/**
 * The node's offline city database, as the location editor reads it.
 *
 * A network's country and city are always a row of that database - the
 * backend refuses anything else - so the editor never sends a typed name: it
 * sends the row's id and the point the operator placed.
 */
import { api } from "./api";
import type { NetworkLocation } from "./types";

export type LocationPrecision = "exact" | "approximate" | "city";
export const LOCATION_PRECISIONS: LocationPrecision[] = ["approximate", "city", "exact"];

export interface GeoCountry {
  code: string;
  name: string;
}

export interface GeoCity {
  id: number;
  name: string;
  state: string;
  country_code: string;
  country_name: string;
  latitude: number;
  longitude: number;
  /** Only on a "nearest" answer. */
  distance_km?: number;
}

/** What a network says about where it is. */
export interface NetworkPlace {
  city_id?: number | null;
  country_code: string;
  country_name: string;
  state?: string;
  city: string;
  location_precision?: LocationPrecision;
  location: NetworkLocation | null;
}

/** Where the lookups are served: the operator routes or the setup wizard's. */
export interface GeoSource {
  base: string;
  auth: boolean;
}

export const NETWORK_GEO: GeoSource = { base: "/api/ui/network/geo", auth: true };
export const SETUP_GEO: GeoSource = { base: "/api/setup/geo", auth: false };

export function geoCountries(source: GeoSource, signal?: AbortSignal) {
  return api
    .get<{ countries: GeoCountry[] }>(`${source.base}/countries`, { auth: source.auth, signal })
    .then((response) => response.countries);
}

export function geoCities(source: GeoSource, country: string, query: string, signal?: AbortSignal) {
  const params = new URLSearchParams({ country, q: query, limit: "12" });
  return api
    .get<{ cities: GeoCity[] }>(`${source.base}/cities?${params}`, { auth: source.auth, signal })
    .then((response) => response.cities);
}

export function geoNearest(source: GeoSource, latitude: number, longitude: number, signal?: AbortSignal) {
  const params = new URLSearchParams({ latitude: String(latitude), longitude: String(longitude), limit: "6" });
  return api
    .get<{ cities: GeoCity[] }>(`${source.base}/nearest?${params}`, { auth: source.auth, signal })
    .then((response) => response.cities);
}

/** Side of the grid an approximate location is snapped to; the node's value. */
const APPROXIMATE_GRID_DEGREES = 0.1;

/**
 * The point the node will publish for a pin, or `null` when it publishes the
 * city's own reference point. A preview only: the node applies the rule again
 * before storing anything.
 */
export function publishedPoint(place: NetworkPlace): { latitude: number; longitude: number } | null {
  const pin = place.location;
  const precision = place.location_precision ?? "approximate";
  if (!pin || precision === "city") return null;
  if (precision === "exact") return { latitude: pin.latitude, longitude: pin.longitude };
  const snap = (degrees: number) => Math.round(degrees / APPROXIMATE_GRID_DEGREES) * APPROXIMATE_GRID_DEGREES;
  return { latitude: Number(snap(pin.latitude).toFixed(1)), longitude: Number(snap(pin.longitude).toFixed(1)) };
}

export function placeLabel(place: Pick<NetworkPlace, "city" | "state" | "country_name" | "country_code">): string {
  return [place.city, place.state, place.country_name || place.country_code].filter(Boolean).join(", ");
}
