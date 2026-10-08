// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocationEditor } from "./LocationEditor";
import { NETWORK_GEO, type GeoCity, type NetworkPlace } from "../lib/geo";
import { api } from "../lib/api";

vi.mock("../lib/api", () => ({ api: { get: vi.fn() } }));
// The map is Leaflet over a real canvas; here it is one button that places a pin.
vi.mock("./LocationMap", () => ({
  default: ({ onPin }: { onPin: (point: { latitude: number; longitude: number }) => void }) => (
    <button type="button" data-testid="fake-map" onClick={() => onPin({ latitude: -25.44, longitude: -49.23 })}>map</button>
  ),
}));

const curitiba: GeoCity = { id: 7, name: "Curitiba", state: "Paraná", country_code: "BR", country_name: "Brazil", latitude: -25.42778, longitude: -49.27306, distance_km: 4.2 };
const pinhais: GeoCity = { id: 8, name: "Pinhais", state: "Paraná", country_code: "BR", country_name: "Brazil", latitude: -25.44472, longitude: -49.1925, distance_km: 4.6 };
const empty: NetworkPlace = { city_id: null, country_code: "", country_name: "", state: "", city: "", location: null };
const t = (key: string) => key;

function answer() {
  vi.mocked(api.get).mockImplementation(async (path: string) => {
    if (path.includes("/nearest")) return { cities: [curitiba, pinhais] };
    if (path.includes("/cities")) return { cities: [{ ...curitiba, distance_km: undefined }] };
    return { countries: [{ code: "BR", name: "Brazil" }] };
  });
}

function mount(value: NetworkPlace = empty) {
  const onChange = vi.fn();
  render(<LocationEditor value={value} onChange={onChange} source={NETWORK_GEO} required t={t} />);
  return onChange;
}

const geolocation = { getCurrentPosition: vi.fn() };

describe("LocationEditor", () => {
  beforeEach(() => {
    answer();
    geolocation.getCurrentPosition.mockReset();
    Object.defineProperty(navigator, "geolocation", { configurable: true, value: geolocation });
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: true });
  });
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("never asks the browser for its location until the admin presses the button", async () => {
    mount();
    fireEvent.click(screen.getByText("location.choose"));
    await waitFor(() => expect(screen.getByTestId("fake-map")).toBeTruthy());
    expect(geolocation.getCurrentPosition).not.toHaveBeenCalled();

    geolocation.getCurrentPosition.mockImplementation((ok: (position: unknown) => void) => ok({ coords: { latitude: -25.44, longitude: -49.23, accuracy: 30 } }));
    fireEvent.click(screen.getByTestId("location-browser"));
    expect(geolocation.getCurrentPosition).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(screen.getByText("Curitiba")).toBeTruthy());
  });

  it("turns a pin into a city of the node's database, and lets the admin pick a neighbour", async () => {
    const onChange = mount();
    fireEvent.click(screen.getByText("location.choose"));
    expect((screen.getByTestId("location-confirm") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(await screen.findByTestId("fake-map"));
    await waitFor(() => expect(screen.getByText("Pinhais")).toBeTruthy());
    expect(vi.mocked(api.get).mock.calls.some(([path]) => String(path).startsWith("/api/ui/network/geo/nearest?latitude=-25.44"))).toBe(true);

    fireEvent.click(screen.getByText("Pinhais"));
    fireEvent.click(screen.getByTestId("location-confirm"));
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({
      city_id: 8,
      city: "Pinhais",
      state: "Paraná",
      country_code: "BR",
      location: { latitude: -25.44, longitude: -49.23, accuracy_m: null },
    }));
  });

  it("finds a city by name without a map, and has no field to type one freely", async () => {
    const onChange = mount();
    fireEvent.click(screen.getByText("location.choose"));
    await waitFor(() => expect(screen.getByRole("option", { name: "Brazil" })).toBeTruthy());
    fireEvent.change(screen.getByRole("combobox", { name: "location.searchCountry" }), { target: { value: "BR" } });
    fireEvent.change(screen.getByTestId("location-search"), { target: { value: "curi" } });
    fireEvent.click(await screen.findByText("Curitiba"));
    await waitFor(() => expect((screen.getByTestId("location-confirm") as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByTestId("location-confirm"));
    // The typed text never becomes the city: the chosen row does.
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({
      city_id: 7,
      city: "Curitiba",
      location: { latitude: curitiba.latitude, longitude: curitiba.longitude, accuracy_m: null },
    }));
  });

  it("explains why the browser cannot be asked outside HTTPS or localhost", async () => {
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: false });
    mount();
    fireEvent.click(screen.getByText("location.choose"));
    fireEvent.click(await screen.findByTestId("location-browser"));
    expect(screen.getByText("location.browserUnavailable")).toBeTruthy();
    expect(geolocation.getCurrentPosition).not.toHaveBeenCalled();
  });

  it("shows what will be published for the chosen precision", () => {
    const onChange = mount({ ...empty, city_id: 7, city: "Curitiba", state: "Paraná", country_code: "BR", country_name: "Brazil", location: { latitude: -25.4412, longitude: -49.2337 } });
    expect(screen.getByTestId("location-place").textContent).toBe("Curitiba, Paraná, Brazil");
    expect((screen.getByTestId("location-precision") as HTMLSelectElement).value).toBe("approximate");
    fireEvent.change(screen.getByTestId("location-precision"), { target: { value: "city" } });
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ location_precision: "city" }));
  });
});
