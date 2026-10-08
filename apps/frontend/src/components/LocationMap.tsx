/**
 * The map of the location dialog: OpenStreetMap tiles and one pin.
 *
 * It is its own module so Leaflet is only downloaded when the dialog opens.
 * The tiles are the only thing here that leaves the node: the city under the
 * pin is answered by the node's own database, never by a map service.
 */
import L from "leaflet";
import "leaflet/dist/leaflet.css";
import { useEffect, useRef } from "react";

export interface MapPoint {
  latitude: number;
  longitude: number;
}

const TILES = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
const ATTRIBUTION = '&copy; <a href="https://www.openstreetmap.org/copyright" target="_blank" rel="noreferrer">OpenStreetMap</a> contributors';
const WORLD: [number, number] = [20, 0];
const PIN_ZOOM = 11;

const PIN = L.divIcon({
  className: "",
  iconSize: [22, 22],
  iconAnchor: [11, 11],
  html: '<span style="display:block;width:22px;height:22px;border-radius:9999px;background:#35c9f0;border:3px solid #fff;box-shadow:0 1px 6px rgba(0,0,0,.55)"></span>',
});

export default function LocationMap({
  pin,
  onPin,
  onTilesFailed,
  label,
}: {
  pin: MapPoint | null;
  onPin: (point: MapPoint) => void;
  onTilesFailed: () => void;
  label: string;
}) {
  const container = useRef<HTMLDivElement>(null);
  const map = useRef<L.Map | null>(null);
  const marker = useRef<L.Marker | null>(null);
  const handlers = useRef({ onPin, onTilesFailed });
  handlers.current = { onPin, onTilesFailed };

  useEffect(() => {
    if (!container.current) return;
    const created = L.map(container.current, { worldCopyJump: true }).setView(WORLD, 2);
    const tiles = L.tileLayer(TILES, { attribution: ATTRIBUTION, maxZoom: 18 }).addTo(created);
    tiles.on("tileerror", () => handlers.current.onTilesFailed());
    created.on("click", (event: L.LeafletMouseEvent) => {
      const point = event.latlng.wrap();
      handlers.current.onPin({ latitude: point.lat, longitude: point.lng });
    });
    map.current = created;
    return () => {
      created.remove();
      map.current = null;
      marker.current = null;
    };
  }, []);

  useEffect(() => {
    const current = map.current;
    if (!current) return;
    if (!pin) {
      marker.current?.remove();
      marker.current = null;
      return;
    }
    const position: [number, number] = [pin.latitude, pin.longitude];
    if (!marker.current) {
      const created = L.marker(position, { draggable: true, icon: PIN, keyboard: false }).addTo(current);
      created.on("dragend", () => {
        const point = created.getLatLng().wrap();
        handlers.current.onPin({ latitude: point.lat, longitude: point.lng });
      });
      marker.current = created;
    } else {
      marker.current.setLatLng(position);
    }
    if (!current.getBounds().contains(position) || current.getZoom() < 5) {
      current.setView(position, Math.max(current.getZoom(), PIN_ZOOM));
    }
  }, [pin?.latitude, pin?.longitude]);

  return <div ref={container} role="application" aria-label={label} className="h-72 w-full overflow-hidden rounded-lg border border-line bg-ink" />;
}
