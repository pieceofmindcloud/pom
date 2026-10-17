// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { resetPublicLatencyCache } from "../lib/publicLatency";
import { resetPublicLocatorCache } from "../lib/publicLocator";
import { resetPublicStatsCache } from "../lib/publicStats";
import { latencyBand, PublicNetworks } from "./PublicNetworks";

const mocks = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
  del: vi.fn(),
  node: { data: undefined as { authority_level: "primary" | "secondary" | null } | undefined },
  networkInfo: { data: undefined as { network: { location: { latitude: number; longitude: number } | null } } | undefined },
  warning: vi.fn(),
  /** Networks the node's table holds beyond what the live listing returns now. */
  stored: [] as Record<string, unknown>[],
  /** Every page the screen asked the node for. */
  knownRequests: [] as string[],
}));

vi.mock("../lib/api", () => ({
  api: { get: (path: string, options?: unknown) => getThroughFakeNode(path, options), post: mocks.post, del: mocks.del },
}));

type Listed = { fingerprint: string; name: string; cluster_id: string; organization: string; description: string; city: string; country_code: string; country_name: string; location: { latitude: number; longitude: number } | null };

function distance(a: { latitude: number; longitude: number }, b: { latitude: number; longitude: number }) {
  const rad = Math.PI / 180;
  const h = Math.sin((b.latitude - a.latitude) * rad / 2) ** 2 + Math.cos(a.latitude * rad) * Math.cos(b.latitude * rad) * Math.sin((b.longitude - a.longitude) * rad / 2) ** 2;
  return 6371 * 2 * Math.atan2(Math.sqrt(h), Math.sqrt(1 - h));
}

/*
 * The node records what the live listing returns and pages it on
 * `/api/ui/networks/known`. The fake does the same over whatever the test
 * serves on `/api/public/networks`, plus `mocks.stored`.
 */
async function getThroughFakeNode(path: string, options?: unknown) {
  if (!path.startsWith("/api/ui/networks/known?")) return mocks.get(path, options);
  mocks.knownRequests.push(path);
  const params = new URLSearchParams(path.split("?")[1]);
  const live = await Promise.resolve(mocks.get("/api/public/networks", { auth: false })).catch(() => []) as Listed[];
  const all = [...live, ...(mocks.stored as Listed[]).filter((stored) => !live.some((network) => network.fingerprint === stored.fingerprint))];
  const country = params.get("country") ?? "";
  const city = params.get("city") ?? "";
  const words = (params.get("q") ?? "").toLowerCase().split(/\s+/).filter(Boolean);
  const viewer = params.has("latitude") ? { latitude: Number(params.get("latitude")), longitude: Number(params.get("longitude")) } : null;
  const within = Number(params.get("within_km") ?? 0);
  const countryOf = (network: Listed) => network.country_code || network.country_name;
  let rows = all
    .filter((network) => words.every((word) => [network.name, network.organization, network.description, network.city, network.country_name].join(" ").toLowerCase().includes(word)))
    .filter((network) => !country || countryOf(network) === country)
    .filter((network) => !city || network.city === city)
    .filter((network) => !within || (viewer && network.location && distance(viewer, network.location) <= within));
  if (params.get("sort") === "name") rows = [...rows].sort((left, right) => left.name.localeCompare(right.name));
  if (params.get("sort") === "distance" && viewer) {
    const away = (network: Listed) => network.location ? distance(viewer, network.location) : Infinity;
    rows = [...rows].sort((left, right) => away(left) - away(right));
  }
  const pageSize = Number(params.get("page_size") ?? 25);
  const pages = Math.max(1, Math.ceil(rows.length / pageSize));
  const page = Math.min(pages, Math.max(1, Number(params.get("page") ?? 1)));
  const countries = Array.from(new Map(all.filter(countryOf).map((network) => [countryOf(network), network.country_name || network.country_code])))
    .map(([code, name]) => ({ code, name }));
  const cities = Array.from(new Set(all.filter((network) => !country || countryOf(network) === country).map((network) => network.city).filter(Boolean))).sort();
  return {
    networks: rows.slice((page - 1) * pageSize, page * pageSize).map((network) => ({ network, first_seen_at: 1, last_seen_at: Math.floor(Date.now() / 1000) - 3 * 86_400 })),
    total: rows.length,
    page,
    page_size: pageSize,
    countries,
    cities,
  };
}
vi.mock("../lib/queries", () => ({
  useCluster: () => ({ data: undefined }),
  useNode: () => mocks.node,
  useNetworkInfo: () => mocks.networkInfo,
  useInvalidate: () => () => undefined,
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ fromError: vi.fn(), success: vi.fn(), warning: mocks.warning }),
}));

const networks = [
  {
    cluster_id: "curitiba",
    name: "Rede Curitiba",
    fingerprint: "fingerprint-curitiba",
    lobby_chat: true,
    publisher_key: "pk:curitiba",
    trust_document: "{}",
    organization: "Equipe Sul",
    description: "# Rede para colaborar\n\n**Bem-vindos**",
    rules: "- Respeite os peers",
    channels: "[Discord](https://discord.example/pom)",
    country_code: "BR",
    country_name: "Brasil",
    city: "Curitiba",
    geo_label: "br.curitiba",
    access_policy: "authorized" as const,
    location: null,
    discovered_via: "pkarr",
  },
  {
    cluster_id: "sao-paulo",
    name: "Rede São Paulo",
    fingerprint: "fingerprint-sao-paulo",
    lobby_chat: true,
    publisher_key: "pk:sao-paulo",
    trust_document: "{}",
    organization: "Equipe Sudeste",
    description: "",
    rules: "",
    channels: "",
    country_code: "BR",
    country_name: "Brasil",
    city: "São Paulo",
    geo_label: "br.sao-paulo",
    access_policy: "authorized" as const,
    location: null,
    discovered_via: "pkarr",
  },
  {
    cluster_id: "lisboa",
    name: "Rede Lisboa",
    fingerprint: "fingerprint-lisboa",
    lobby_chat: true,
    publisher_key: "pk:lisboa",
    trust_document: "{}",
    organization: "Equipe Europa",
    description: "",
    rules: "",
    channels: "",
    country_code: "PT",
    country_name: "Portugal",
    city: "Lisboa",
    geo_label: "pt.lisboa",
    access_policy: "authorized" as const,
    location: null,
    discovered_via: "pkarr",
  },
];

const publicHealth = vi.fn();

/** Serves the catalog and, when given, the network this node already belongs to. */
function serve(local?: (typeof networks)[number]) {
  mocks.get.mockImplementation((path: string) => {
    if (path === "/api/public/networks") return Promise.resolve(networks);
    if (path === "/api/public/network" && local) return Promise.resolve(local);
    if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
    return Promise.reject(new Error("no local network"));
  });
}

beforeEach(() => {
  vi.stubGlobal("fetch", publicHealth);
  mocks.stored = [];
  mocks.knownRequests = [];
  mocks.node.data = undefined;
  mocks.networkInfo.data = undefined;
  mocks.post.mockResolvedValue({});
  mocks.get.mockImplementation((path: string) => {
    if (path === "/api/public/networks") return Promise.resolve(networks);
    if (path.startsWith("/api/public/networks/locate?")) {
      const key = new URLSearchParams(path.split("?")[1]).get("key");
      const reachability = key === "pk:curitiba" ? "direct" : key === "pk:sao-paulo" ? "relayed" : "unresolved";
      return Promise.resolve({ reachability, relays: reachability === "relayed" ? 1 : 0 });
    }
    return Promise.reject(new Error("no local network"));
  });
});

afterEach(() => {
  cleanup();
  resetPublicLocatorCache();
  resetPublicLatencyCache();
  resetPublicStatsCache();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

/** The filters sit behind the "+" toggle. */
function openFilters() {
  fireEvent.click(screen.getByRole("button", { name: /^(Filtros|Filters)$/ }));
}

describe("PublicNetworks", () => {
  it("filters networks by country and city", async () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    expect(await screen.findByText("Rede Curitiba")).toBeTruthy();
    expect(screen.getByText("Rede São Paulo")).toBeTruthy();
    expect(screen.getByText("Rede Lisboa")).toBeTruthy();

    openFilters();
    const country = screen.getByRole("combobox", { name: "País" });
    fireEvent.change(country, { target: { value: "BR" } });
    await waitFor(() => expect(screen.queryByText("Rede Lisboa")).toBeNull());
    expect(screen.getByText("Rede Curitiba")).toBeTruthy();

    const city = screen.getByRole("combobox", { name: "Cidade" });
    await waitFor(() => expect(within(city).queryByRole("option", { name: "Lisboa" })).toBeNull());
    expect(within(city).getByRole("option", { name: "Curitiba" })).toBeTruthy();
    fireEvent.change(city, { target: { value: "São Paulo" } });
    await waitFor(() => {
      expect(screen.queryByText("Rede Curitiba")).toBeNull();
      expect(screen.getByText("Rede São Paulo")).toBeTruthy();
    });
  });

  /** Names of the listed networks, top to bottom. */
  function order() {
    return screen.getAllByRole("button", { name: /Abrir detalhes|Open details/ }).map((button) => button.textContent ?? "").map((text) => ["Curitiba", "São Paulo", "Lisboa"].find((name) => text.includes(`Rede ${name}`)));
  }

  function mount() {
    return render(
      <LocaleProvider initialLocale="en">
        <PublicNetworks />
      </LocaleProvider>,
    );
  }

  it("measures distances from this network's location and never asks the browser by itself", async () => {
    const geolocation = { getCurrentPosition: vi.fn() };
    Object.defineProperty(navigator, "geolocation", { configurable: true, value: geolocation });
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: true });
    // This node's network is in Lisbon.
    mocks.networkInfo.data = { network: { location: { latitude: 38.72, longitude: -9.14 } } };
    const points = [{ latitude: -25.43, longitude: -49.27 }, { latitude: -23.55, longitude: -46.63 }, { latitude: 38.72, longitude: -9.14 }];
    const base = mocks.get.getMockImplementation()!;
    mocks.get.mockImplementation((path: string) => path === "/api/public/networks"
      ? Promise.resolve(networks.map((network, index) => ({ ...network, location: points[index] })))
      : base(path));
    mount();

    await screen.findByText("Rede Curitiba");
    expect(geolocation.getCurrentPosition).not.toHaveBeenCalled();
    expect(order()).toEqual(["Lisboa", "São Paulo", "Curitiba"]);
    openFilters();
    expect(screen.getByTestId("distance-origin").textContent).toContain("your network's location");

    // Only what is within reach of Lisbon stays.
    fireEvent.change(screen.getByRole("combobox", { name: "Distance" }), { target: { value: "500" } });
    await waitFor(() => expect(order()).toEqual(["Lisboa"]));
    fireEvent.change(screen.getByRole("combobox", { name: "Distance" }), { target: { value: "0" } });

    // The browser is asked on the button, and then it is the origin.
    geolocation.getCurrentPosition.mockImplementation((ok: (position: unknown) => void) => ok({ coords: { latitude: -23.55, longitude: -46.63 } }));
    fireEvent.click(screen.getByRole("button", { name: "Use my browser's location" }));
    expect(geolocation.getCurrentPosition).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(order()).toEqual(["São Paulo", "Curitiba", "Lisboa"]));
    expect(screen.getByTestId("distance-origin").textContent).toContain("your browser's location");
  });

  it("has no distance filter without a point to measure from", async () => {
    mount();
    await screen.findByText("Rede Curitiba");
    openFilters();
    expect((screen.getByRole("combobox", { name: "Distance" }) as HTMLSelectElement).disabled).toBe(true);
    expect(screen.getByTestId("distance-origin").textContent).toContain("set your network's location");
  });

  it("measures latency only when asked, and sorts by it", async () => {
    const rtt: Record<string, number> = { "pk:curitiba": 180, "pk:sao-paulo": 40 };
    const base = mocks.get.getMockImplementation()!;
    mocks.get.mockImplementation((path: string) => {
      if (path.startsWith("/api/ui/network-chat/lobby/latency?")) {
        const key = new URLSearchParams(path.split("?")[1]).get("key") ?? "";
        return key in rtt ? Promise.resolve({ rtt_ms: rtt[key] }) : Promise.reject(new Error("publisher_timeout"));
      }
      return base(path);
    });
    mount();
    await screen.findByText("Rede Curitiba");
    const asked = () => mocks.get.mock.calls.filter(([path]) => String(path).includes("/lobby/latency")).length;
    expect(asked()).toBe(0);

    openFilters();
    fireEvent.change(screen.getByRole("combobox", { name: "Sort by" }), { target: { value: "latency" } });
    await waitFor(() => expect(screen.getByTestId("public-network-latency-fingerprint-sao-paulo").textContent).toBe("40 ms"));
    await waitFor(() => expect(screen.getByTestId("public-network-latency-fingerprint-lisboa").textContent).toBe("no answer"));
    expect(screen.getByTestId("public-network-latency-fingerprint-curitiba").textContent).toBe("180 ms");
    expect(asked()).toBe(3);
    // The network that did not answer goes last.
    expect(order()).toEqual(["São Paulo", "Curitiba", "Lisboa"]);
  });

  it("orders by the measured latency once measured, marks no answer in red and dashes missing counts", async () => {
    const rtt: Record<string, number> = { "pk:curitiba": 180, "pk:sao-paulo": 40 };
    const base = mocks.get.getMockImplementation()!;
    mocks.get.mockImplementation((path: string) => {
      if (path.startsWith("/api/ui/network-chat/lobby/latency?")) {
        const key = new URLSearchParams(path.split("?")[1]).get("key") ?? "";
        return key in rtt ? Promise.resolve({ rtt_ms: rtt[key] }) : Promise.reject(new Error("publisher_timeout"));
      }
      return base(path);
    });
    mount();
    await screen.findByText("Rede Curitiba");
    // Nodes and VRAM a network does not publish read as a dash.
    const curitiba = screen.getByTestId("network-row-fingerprint-curitiba");
    // Nodes, VRAM and served models.
    expect(within(curitiba).getAllByText("-").length).toBe(3);

    // The toolbar button alone, with no sort chosen, already orders by latency.
    fireEvent.click(screen.getByRole("button", { name: "Measure latency" }));
    await waitFor(() => expect(order()).toEqual(["São Paulo", "Curitiba", "Lisboa"]));
    const failed = await screen.findByText("no answer");
    expect(failed.className).toContain("text-rose");
    // The color follows how much the round trip slows inference: 40 ms is fine, 180 ms is slow.
    expect(screen.getByTestId("public-network-latency-fingerprint-sao-paulo").getAttribute("data-band")).toBe("fast");
    expect(screen.getByTestId("public-network-latency-fingerprint-sao-paulo").className).toContain("text-lime");
    expect(screen.getByTestId("public-network-latency-fingerprint-curitiba").getAttribute("data-band")).toBe("slow");
    expect(screen.getByTestId("public-network-latency-fingerprint-curitiba").className).toContain("text-relay");
  });

  it("does not call a network missing from the DHT when the check itself failed", async () => {
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve(networks);
      return Promise.reject(new Error("HTTP 404"));
    });
    render(
      <LocaleProvider initialLocale="en">
        <PublicNetworks />
      </LocaleProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("public-network-reachability-fingerprint-curitiba").getAttribute("data-reachability")).toBe("unknown");
    });
    expect(screen.getAllByText("Could not check").length).toBeGreaterThan(0);
    expect(screen.queryByText("Not found on the DHT")).toBeNull();
  });

  it("shows reachability by the publisher key, resolved by the node, without an IP", async () => {
    const { container } = render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("public-network-reachability-fingerprint-curitiba").getAttribute("data-reachability")).toBe("direct");
      expect(screen.getByTestId("public-network-reachability-fingerprint-sao-paulo").getAttribute("data-reachability")).toBe("relayed");
      expect(screen.getByTestId("public-network-reachability-fingerprint-lisboa").getAttribute("data-reachability")).toBe("unresolved");
    });

    expect(mocks.get).toHaveBeenCalledWith("/api/public/networks/locate?key=pk%3Acuritiba", { auth: false });
    // The browser probes no address: the node does the resolving.
    expect(publicHealth).not.toHaveBeenCalled();
    expect(container.textContent).not.toMatch(/\d+\.\d+\.\d+\.\d+/);
  });

  it("shows the main facts of the clicked network in a side panel, and everything in a full view", async () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    fireEvent.click(await screen.findByText("Rede Curitiba"));

    const panel = await screen.findByRole("complementary", { name: "Rede Rede Curitiba" });
    expect(within(panel).getByRole("heading", { name: "Rede para colaborar" })).toBeTruthy();
    expect(within(panel).getByText("Aceitando novos membros")).toBeTruthy();
    // Each role keeps its name beside an icon and its own state.
    const roles = within(panel).getByTestId("network-enrollment-roles");
    expect(roles.querySelector('[data-role="node"]')?.textContent).toContain("Nós da rede");
    expect(roles.querySelector('[data-role="node"] svg')).toBeTruthy();
    expect(roles.querySelector('[data-role="infer"]')?.textContent).toContain("Nós de inferência");
    expect(within(panel).getByRole("link", { name: "Discord" })).toBeTruthy();
    expect(within(panel).getByText("Respeite os peers")).toBeTruthy();
    // The organization left the table and lives here; the place stays in the row.
    expect(within(panel).getByText("Equipe Sul")).toBeTruthy();
    expect(within(panel).queryByText("Curitiba, Brasil")).toBeNull();
    const actions = within(panel).getByTestId("network-panel-actions");
    expect(within(actions).getByRole("button", { name: "Pedir para entrar" })).toBeTruthy();
    expect(within(actions).getByRole("button", { name: "Lobby" })).toBeTruthy();
    // Another network's trust document is not offered for download.
    expect(within(actions).queryByRole("button", { name: "Documento de confiança" })).toBeNull();
    expect(screen.getByTestId("network-row-fingerprint-curitiba").getAttribute("aria-selected")).toBe("true");
    // Keys and fingerprints are for the full view, not for reading at a glance.
    expect(within(panel).queryByText("fingerprint-curitiba")).toBeNull();
    expect(within(panel).queryByText("pk:curitiba")).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();

    fireEvent.click(within(panel).getByRole("button", { name: "Abrir todos os detalhes" }));
    const dialog = await screen.findByRole("dialog", { name: "Detalhes de Rede Curitiba" });
    expect(within(dialog).getByText("fingerprint-curitiba")).toBeTruthy();
    expect(within(dialog).getByText("pk:curitiba")).toBeTruthy();

    fireEvent.click(within(dialog).getByRole("button", { name: "Fechar" }));
    fireEvent.click(within(panel).getByRole("button", { name: "Fechar painel" }));
    await waitFor(() => expect(screen.queryByRole("complementary", { name: "Rede Rede Curitiba" })).toBeNull());
  });

  it("shows long rules open in the side panel", async () => {
    const rules = `- ${"Respeite os peers e compartilhe a VRAM. ".repeat(12)}`;
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([{ ...networks[0], rules }]);
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    fireEvent.click(await screen.findByText("Rede Curitiba"));
    const panel = await screen.findByRole("complementary", { name: "Rede Rede Curitiba" });
    expect(panel.querySelector("details")).toBeNull();
    expect(within(panel).getByText(/Respeite os peers e compartilhe a VRAM/)).toBeTruthy();
  });

  it("offers the principal a quick setup of its network while discovery is off, and publishes it", async () => {
    const own = { ...networks[2], fingerprint: "fingerprint-own", name: "Minha rede", publisher_key: "pk:own", trust_document: "{}" };
    mocks.node.data = { authority_level: "primary" };
    mocks.networkInfo.data = {
      can_edit: true,
      network: { public_name: "Minha rede", organization: "", description: "", rules: "", channels: "", country_code: "", country_name: "", city: "", geo_label: "", visibility: "local", access_policy: "local_only", accepted_enrollments: [], public_stats: false, location: null },
    } as never;
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([]);
      if (path === "/api/ui/networks/pinned") return Promise.resolve({ own: { network: own, visibility: "local" }, custom: [] });
      return Promise.reject(new Error("no local network"));
    });
    mocks.post.mockImplementation((_path: string, body: unknown) => Promise.resolve({ can_edit: true, network: body }));
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    fireEvent.click(await screen.findByTestId("network-quick-setup-row"));
    const wizard = await screen.findByTestId("network-quick-setup");
    // Publishing is the suggestion.
    expect(within(wizard).getByRole("radio", { name: /Pública/ }).getAttribute("aria-checked")).toBe("true");
    // Hidden needs neither the place nor the stats.
    fireEvent.click(within(wizard).getByRole("radio", { name: /Oculta/ }));
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    fireEvent.change(within(wizard).getByLabelText(/^Descrição/), { target: { value: "Rede da casa" } });
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    expect(within(wizard).getByRole("radio", { name: /Aprovados por um administrador/ }).getAttribute("aria-checked")).toBe("true");
    expect(within(wizard).queryByText(/Publicar modelos servidos/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    expect(within(wizard).getByTestId("quick-setup-review").textContent).toContain("Oculta");
    fireEvent.click(screen.getByRole("button", { name: "Salvar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/settings", expect.objectContaining({
      visibility: "private",
      access_policy: "authorized",
      accepted_enrollments: ["node", "infer"],
      description: "Rede da casa",
    })));
    await waitFor(() => expect(screen.queryByTestId("network-quick-setup")).toBeNull());
  });

  it("suggests relaying for any network in the quick setup and saves the relay after the network", async () => {
    const own = { ...networks[2], fingerprint: "fingerprint-own", name: "Minha rede", publisher_key: "pk:own" };
    mocks.node.data = { authority_level: "primary" };
    mocks.networkInfo.data = {
      can_edit: true,
      network: { public_name: "Minha rede", organization: "", description: "", rules: "", channels: "", country_code: "", country_name: "", city: "", geo_label: "", visibility: "local", access_policy: "local_only", accepted_enrollments: [], public_stats: false, location: null },
    } as never;
    const relay = { everyone: false, cluster: true, transfers: true, fixed: { everyone: false, no_cluster: false, no_transfers: false }, reachable: true, serving: true };
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([]);
      if (path === "/api/ui/networks/pinned") return Promise.resolve({ own: { network: own, visibility: "local" }, custom: [] });
      if (path === "/api/ui/network/relay") return Promise.resolve(relay);
      return Promise.reject(new Error("no local network"));
    });
    mocks.post.mockImplementation((path: string, body: unknown) => Promise.resolve(path === "/api/ui/network/relay" ? { ...relay, ...(body as object) } : { can_edit: true, network: body }));
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    fireEvent.click(await screen.findByTestId("network-quick-setup-row"));
    const wizard = await screen.findByTestId("network-quick-setup");
    fireEvent.click(within(wizard).getByRole("radio", { name: /Desligada/ }));
    await waitFor(() => expect(within(wizard).getAllByRole("listitem").map((item) => item.textContent)).toEqual(["1Descoberta", "2Informações", "3Relé", "4Revisão"]));
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    // Relaying for any network is the suggestion, with the recommendation.
    const everyone = within(wizard).getByTestId("relay-everyone") as HTMLInputElement;
    expect(everyone.checked).toBe(true);
    expect(everyone.closest("label")!.textContent).toContain("Recomendado");
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    expect(within(wizard).getByTestId("quick-setup-review").textContent).toContain("Servindo de relé para qualquer rede");
    fireEvent.click(screen.getByRole("button", { name: "Salvar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/relay", { everyone: true, cluster: true, transfers: true }));
    const order = mocks.post.mock.calls.map(([path]) => path);
    expect(order.indexOf("/api/ui/network/settings")).toBeLessThan(order.indexOf("/api/ui/network/relay"));
  });

  it("leaves the relay step out of the quick setup behind a NAT", async () => {
    const own = { ...networks[2], fingerprint: "fingerprint-own", name: "Minha rede", publisher_key: "pk:own" };
    mocks.node.data = { authority_level: "primary" };
    mocks.networkInfo.data = {
      can_edit: true,
      network: { public_name: "Minha rede", organization: "", description: "", rules: "", channels: "", country_code: "", country_name: "", city: "", geo_label: "", visibility: "local", access_policy: "local_only", accepted_enrollments: [], public_stats: false, location: null },
    } as never;
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([]);
      if (path === "/api/ui/networks/pinned") return Promise.resolve({ own: { network: own, visibility: "local" }, custom: [] });
      if (path === "/api/ui/network/relay") return Promise.resolve({ everyone: false, cluster: true, transfers: true, fixed: { everyone: false, no_cluster: false, no_transfers: false }, reachable: false, serving: false });
      return Promise.reject(new Error("no local network"));
    });
    mocks.post.mockImplementation((_path: string, body: unknown) => Promise.resolve({ can_edit: true, network: body }));
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    fireEvent.click(await screen.findByTestId("network-quick-setup-row"));
    const wizard = await screen.findByTestId("network-quick-setup");
    fireEvent.click(within(wizard).getByRole("radio", { name: /Desligada/ }));
    await waitFor(() => expect(mocks.get).toHaveBeenCalledWith("/api/ui/network/relay", undefined));
    expect(within(wizard).getAllByRole("listitem").map((item) => item.textContent)).toEqual(["1Descoberta", "2Informações", "3Revisão"]);
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    expect(within(wizard).getByTestId("quick-setup-review").textContent).not.toContain("Relé");
    fireEvent.click(screen.getByRole("button", { name: "Salvar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/settings", expect.anything()));
    expect(mocks.post).not.toHaveBeenCalledWith("/api/ui/network/relay", expect.anything());
  });

  it("asks for the name and the place before publishing a public network", async () => {
    const own = { ...networks[2], fingerprint: "fingerprint-own", name: "", publisher_key: "pk:own" };
    mocks.node.data = { authority_level: "primary" };
    mocks.networkInfo.data = {
      can_edit: true,
      network: { public_name: "", organization: "", description: "", rules: "", channels: "", country_code: "", country_name: "", city: "", geo_label: "", visibility: "private", access_policy: "authorized", accepted_enrollments: ["node"], public_stats: false, location: null },
    } as never;
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([]);
      if (path === "/api/ui/networks/pinned") return Promise.resolve({ own: { network: own, visibility: "private" }, custom: [] });
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    fireEvent.click(await screen.findByRole("button", { name: "Configuração rápida", hidden: false }));
    const wizard = await screen.findByTestId("network-quick-setup");
    // A hidden network keeps its choice; going public is one click.
    expect(within(wizard).getByRole("radio", { name: /Oculta/ }).getAttribute("aria-checked")).toBe("true");
    fireEvent.click(within(wizard).getByRole("radio", { name: /Pública/ }));
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    const next = screen.getByRole("button", { name: "Próxima" }) as HTMLButtonElement;
    expect(next.disabled).toBe(true);
    fireEvent.change(within(wizard).getByLabelText(/^Nome público/), { target: { value: "Rede Sul" } });
    expect(next.disabled).toBe(false);
    fireEvent.click(next);
    // The role kept from before stays the only one.
    expect((within(wizard).getByRole("checkbox", { name: /Nós de inferência/ }) as HTMLInputElement).checked).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    expect(within(wizard).getByTestId("location-editor")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Próxima" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("offers no quick setup on a published network or to a member that is not its principal", async () => {
    const own = { ...networks[2], fingerprint: "fingerprint-own", name: "Minha rede", publisher_key: "pk:own" };
    mocks.node.data = { authority_level: "secondary" };
    mocks.networkInfo.data = { can_edit: false, network: { visibility: "local", accepted_enrollments: [] } } as never;
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([]);
      if (path === "/api/ui/networks/pinned") return Promise.resolve({ own: { network: own, visibility: "local" }, custom: [] });
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);
    await screen.findByText("Minha rede");
    expect(screen.queryByRole("button", { name: "Configuração rápida" })).toBeNull();
  });

  it("says in the side panel when a network is closed to new members", async () => {
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([{ ...networks[1], accepted_enrollments: [] }]);
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    fireEvent.click(await screen.findByText("Rede São Paulo"));
    const panel = await screen.findByRole("complementary", { name: "Rede Rede São Paulo" });
    expect(within(panel).getByTestId("network-enrollment-state").textContent).toContain("Fechada para novos membros");
    // Fields the owner left empty are not drawn at all.
    expect(within(panel).queryByText("Regras")).toBeNull();
    expect(within(panel).queryByText("Indisponível")).toBeNull();
  });

  it("pages the known networks and searches them on the node, and the search also filters this machine's network", async () => {
    const many = Array.from({ length: 30 }, (_, index) => ({ ...networks[2], cluster_id: `rede-${index}`, name: `Rede ${String(index).padStart(2, "0")}`, fingerprint: `fingerprint-${index}`, publisher_key: `pk:${index}` }));
    const own = { ...networks[0], fingerprint: "fingerprint-own", name: "Minha rede", publisher_key: "pk:own" };
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve(many);
      if (path === "/api/ui/networks/pinned") return Promise.resolve({ own: { network: own, visibility: "public" }, custom: [] });
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);

    await screen.findByText("Rede 00");
    await screen.findByText("Minha rede");
    expect(screen.getByTestId("public-networks-pagination").textContent).toContain("1-25 of 30 networks");
    expect(screen.queryByText("Rede 29")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Next page" }));
    await screen.findByText("Rede 29");
    expect(screen.queryByText("Rede 00")).toBeNull();
    expect(screen.getByTestId("public-networks-pagination").textContent).toContain("Page 2 of 2");
    expect(screen.getAllByTestId(/^network-row-/)[0].getAttribute("data-network-kind")).toBe("own");

    fireEvent.change(screen.getByRole("searchbox", { name: "Search" }), { target: { value: "rede 07" } });
    await waitFor(() => expect(screen.getAllByTestId(/^network-row-/).map((row) => row.getAttribute("data-testid"))).toEqual(["network-row-fingerprint-7"]));
    // A new search starts again from the first page.
    expect(mocks.knownRequests.some((path) => path.includes("q=rede+07") && path.includes("page=1&"))).toBe(true);
    expect(screen.getByTestId("public-networks-pagination").textContent).toContain("1-1 of 1 networks");

    // A search that matches this machine's network, accents folded, keeps it.
    fireEvent.change(screen.getByRole("searchbox", { name: "Search" }), { target: { value: "MINHA" } });
    await waitFor(() => expect(screen.getAllByTestId(/^network-row-/).map((row) => row.getAttribute("data-testid"))).toEqual(["network-row-fingerprint-own"]));
  });

  it("keeps the filters folded behind the + and searches on the button", async () => {
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
    await screen.findByText("Rede Curitiba");
    expect(screen.queryByRole("combobox", { name: "Country" })).toBeNull();
    expect(screen.queryByText(/coordinates it publishes/)).toBeNull();
    const toggle = screen.getByRole("button", { name: "Filters" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(toggle);
    expect(screen.getByRole("combobox", { name: "Country" })).toBeTruthy();

    mocks.knownRequests.length = 0;
    fireEvent.change(screen.getByRole("searchbox", { name: "Search" }), { target: { value: "lisboa" } });
    fireEvent.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() => expect(mocks.knownRequests.some((path) => path.includes("q=lisboa"))).toBe(true));
  });

  it("explains how to add a network before choosing the file", async () => {
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
    await screen.findByText("Rede Curitiba");
    const input = screen.getByTestId("network-import-file") as HTMLInputElement;
    const pick = vi.spyOn(input, "click").mockImplementation(() => {});
    fireEvent.click(screen.getByRole("button", { name: "Add network" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a network" });
    expect(within(dialog).getByText(/click Share on that network/)).toBeTruthy();
    expect(pick).not.toHaveBeenCalled();
    fireEvent.click(within(dialog).getByRole("button", { name: "Choose file" }));
    expect(pick).toHaveBeenCalledTimes(1);
    pick.mockRestore();
  });

  it("measures latency from the toolbar and offers the browser's location only on HTTPS", async () => {
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: false });
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
    await screen.findByText("Rede Curitiba");
    // Not a filter: reachable with the filters closed.
    expect(screen.getByRole("button", { name: "Measure latency" })).toBeTruthy();
    openFilters();
    expect(screen.queryByRole("button", { name: "Use my browser's location" })).toBeNull();
    expect(screen.getByTestId("distance-origin").textContent).not.toContain("browser");
  });

  it("draws cards instead of the wide table on a phone", async () => {
    const matchMedia = window.matchMedia;
    window.matchMedia = ((query: string) => ({ matches: true, media: query, addEventListener: () => {}, removeEventListener: () => {} })) as unknown as typeof window.matchMedia;
    try {
      const { container } = render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
      await screen.findByText("Rede Curitiba");
      expect(container.querySelector("table")).toBeNull();
      expect(screen.getAllByTestId(/^network-row-/).every((row) => row.tagName === "LI")).toBe(true);
    } finally {
      window.matchMedia = matchMedia;
    }
  });

  it("lists the networks the node heard of before and marks the ones the live search did not find", async () => {
    mocks.stored = [{ ...networks[2], fingerprint: "fingerprint-old", name: "Rede antiga", publisher_key: "pk:old" }];
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);

    await screen.findByText("Rede antiga");
    await waitFor(() => expect(screen.getByTestId("network-cached-fingerprint-old").textContent).toContain("Not seen now"));
    expect(screen.queryByTestId("network-cached-fingerprint-curitiba")).toBeNull();
  });

  it("offers Leave instead of Join on the network this node already belongs to", async () => {
    mocks.node.data = { authority_level: null };
    serve(networks[0]);
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    const row = (await screen.findByText("Rede Curitiba")).closest("tr")!;
    const leave = await within(row).findByRole("button", { name: "Sair da rede" });
    expect(within(row).queryByRole("button", { name: "Conversar" })).toBeNull();
    const other = screen.getByText("Rede Lisboa").closest("tr")!;
    expect(within(other).getByRole("button", { name: "Conversar" })).toBeTruthy();

    fireEvent.click(leave);
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Sair da rede" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/leave"));
  });

  it("offers no action to the principal on its own network", async () => {
    mocks.node.data = { authority_level: "primary" };
    serve(networks[0]);
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    const other = (await screen.findByText("Rede Lisboa")).closest("tr")!;
    const row = screen.getByText("Rede Curitiba").closest("tr")!;
    await waitFor(() => expect(within(row).queryByRole("button", { name: "Conversar" })).toBeNull());
    expect(within(row).queryByRole("button", { name: "Sair da rede" })).toBeNull();
    expect(within(other).getByRole("button", { name: "Conversar" })).toBeTruthy();
  });

  it("asks to join a network whose lobby has no conversation in a dialog with the request only", async () => {
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([{ ...networks[0], lobby_chat: false }, { ...networks[1], lobby_chat: false, accepted_enrollments: [] }]);
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      if (path.startsWith("/api/ui/network-chat/lobby?")) {
        return Promise.resolve({ accepted: true, error: null, node_id: "me", card: { hostname: "eu", os: "linux", arch: "x86_64", cpu_count: 4, ram_total_mb: 8192, gpus: [] }, messages: [], messages_closed: true, admission: null });
      }
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    const row = (await screen.findByText("Rede Curitiba")).closest("tr")!;
    expect(within(row).queryByRole("button", { name: "Conversar" })).toBeNull();
    // Closed and with nothing to talk in, there is nothing to do.
    const closed = screen.getByText("Rede São Paulo").closest("tr")!;
    expect((within(closed).getByRole("button", { name: "Pedir para entrar" }) as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(within(row).getByRole("button", { name: "Pedir para entrar" }));
    // Nothing to talk in: a dialog with the request side only, and no lobby tab.
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByTestId("lobby-admission");
    expect(within(dialog).queryByTestId("lobby-messages")).toBeNull();
    expect(screen.queryByRole("region", { name: "Lobby de Rede Curitiba" })).toBeNull();
    expect(screen.queryByRole("tab", { name: "Rede Curitiba" })).toBeNull();
  });

  it("says in the request dialog that a network's lobby is off, without opening a tab", async () => {
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([{ ...networks[0], lobby_chat: false }]);
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      if (path.startsWith("/api/ui/network-chat/lobby?")) return Promise.resolve({ accepted: false, error: "lobby_disabled" });
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    const row = (await screen.findByText("Rede Curitiba")).closest("tr")!;
    fireEvent.click(within(row).getByRole("button", { name: "Pedir para entrar" }));
    const dialog = await screen.findByRole("dialog");
    expect(await within(dialog).findByTestId("lobby-disabled")).toBeTruthy();
    expect(within(dialog).queryByTestId("lobby-request")).toBeNull();
    expect(screen.queryByRole("region", { name: "Lobby de Rede Curitiba" })).toBeNull();
  });

  it("asks for admission through the lobby, which every listed network has", async () => {
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") {
        return Promise.resolve([
          { ...networks[0], accepted_enrollments: ["infer"] },
          { ...networks[1], accepted_enrollments: [] },
          networks[2],
        ]);
      }
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      if (path.startsWith("/api/ui/network-chat/lobby?")) {
        return Promise.resolve({ accepted: true, error: null, node_id: "me", card: { hostname: "eu", os: "linux", arch: "x86_64", cpu_count: 4, ram_total_mb: 8192, gpus: [] }, messages: [], admission: null });
      }
      return Promise.reject(new Error("no local network"));
    });
    mocks.post.mockResolvedValue({
      accepted: true, error: null, node_id: "me", messages: [],
      card: { hostname: "eu", os: "linux", arch: "x86_64", cpu_count: 4, ram_total_mb: 8192, gpus: [] },
      admission: { key: "pk:curitiba", name: "Rede Curitiba", role: "infer", status: "pending" },
    });
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    const open = (await screen.findByText("Rede Curitiba")).closest("tr")!;
    // A network that takes no new member can still be talked to.
    const closed = screen.getByText("Rede São Paulo").closest("tr")!;
    expect(within(closed).getByRole("button", { name: "Conversar" })).toBeTruthy();
    // There is no network without a lobby: every listed one can be asked.
    const third = screen.getByText("Rede Lisboa").closest("tr")!;
    expect((within(third).getByRole("button", { name: "Conversar" }) as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(within(open).getByRole("button", { name: "Conversar" }));
    const dialog = await screen.findByRole("region", { name: "Lobby de Rede Curitiba" });
    await waitFor(() => expect(mocks.get).toHaveBeenCalledWith("/api/ui/network-chat/lobby?key=pk%3Acuritiba", { auth: true }));

    // Only the role the network accepts is offered, and the request goes
    // through this node with the network's name to label it.
    const role = await within(dialog).findByRole("combobox", { name: "Entrar como" });
    expect(within(role).getAllByRole("option").map((option) => option.textContent)).toEqual(["Nós de inferência"]);
    fireEvent.click(within(dialog).getByRole("button", { name: "Pedir admissão" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/network-chat/lobby/admission",
      { key: "pk:curitiba", role: "infer", name: "Rede Curitiba" },
    ));
    expect(await within(dialog).findByText(/Aguardando um administrador/)).toBeTruthy();
  });

  it("shows models, nodes and VRAM of a network that publishes them, and asks no other", async () => {
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([{ ...networks[0], public_stats: true }, networks[1]]);
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      if (path === "/api/ui/network-chat/lobby/info?key=pk%3Acuritiba") {
        return Promise.resolve({ accepted: true, error: null, info: { models: ["gemma", "qwen3"], node_count: 4, vram_total_mb: 49152 } });
      }
      return Promise.reject(new Error("no local network"));
    });
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    const row = (await screen.findByText("Rede Curitiba")).closest("tr")!;
    // One served model per line; nodes and VRAM share a column.
    expect(await within(row).findByText("gemma")).toBeTruthy();
    expect(within(row).getByText("qwen3")).toBeTruthy();
    expect(within(row).getByText("4 nós")).toBeTruthy();
    expect(within(row).getByText("48.0 GB")).toBeTruthy();
    // Latency to a network that was not joined is not measured by anyone: nothing is drawn.
    expect(within(row).queryByText("Indisponível")).toBeNull();
    expect(within(row).getByTestId("public-network-latency-fingerprint-curitiba").textContent).toBe("");

    const silent = screen.getByText("Rede São Paulo").closest("tr")!;
    expect(within(silent).queryByText("gemma")).toBeNull();
    expect(mocks.get.mock.calls.some(([path]) => String(path).includes("lobby/info?key=pk%3Asao-paulo"))).toBe(false);
  });

  it("lists three models and counts the rest, while the panel names them all", async () => {
    const models = ["m1", "m2", "m3", "m4", "m5"];
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([{ ...networks[0], public_stats: true }]);
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      if (path.startsWith("/api/ui/network-chat/lobby/info?")) return Promise.resolve({ accepted: true, error: null, info: { models, node_count: 2, vram_total_mb: 1024 } });
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
    const row = (await screen.findByText("Rede Curitiba")).closest("tr")!;
    expect(await within(row).findByText("and 2 more")).toBeTruthy();
    expect(within(row).getByText("m3")).toBeTruthy();
    expect(within(row).queryByText("m4")).toBeNull();
    fireEvent.click(within(row).getByRole("button", { name: /Open details/ }));
    const panel = await screen.findByTestId("network-panel-models");
    expect(within(panel).getAllByRole("listitem").map((item) => item.textContent)).toEqual(models);
  });

  it("explains each accepted role on its icon, in a column of its own, without opening the row", async () => {
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
    const row = (await screen.findByText("Rede Curitiba")).closest("tr")!;
    const icons = within(row).getByTestId("network-enrollment-icons");
    // Its own cell, apart from the action buttons.
    expect(icons.closest("td")).not.toBe(within(row).getByRole("button", { name: "Talk" }).closest("td"));
    const infer = within(icons).getByRole("button", { name: "Accepting Inference nodes" });
    fireEvent.mouseEnter(infer);
    expect(screen.getByRole("tooltip").textContent).toContain("only use its models");
    fireEvent.click(infer);
    expect(screen.queryByTestId("network-side-panel")).toBeNull();
  });

  it("has no page title: the tab row carries the list's actions", async () => {
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
    await screen.findByText("Rede Curitiba");
    expect(screen.queryByRole("heading", { level: 1 })).toBeNull();
    expect(screen.getByRole("tab", { name: "Networks" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.getByRole("button", { name: "Add network" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Refresh" })).toBeTruthy();
    expect(screen.queryByRole("columnheader", { name: "Organization" })).toBeNull();
  });

  it("keeps this machine's network and the imported ones on top, apart from the public ones", async () => {
    const own = { ...networks[2], fingerprint: "fingerprint-own", name: "Minha rede", publisher_key: "pk:own", trust_document: "{\"own\":true}" };
    const custom = { ...networks[1], fingerprint: "fingerprint-custom", name: "Rede privada", publisher_key: "pk:custom", discovered_via: "custom", trust_document: "{\"custom\":true}" };
    let pinned = { own: { network: own, visibility: "local" }, custom: [{ network: custom, visibility: "private" }] };
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve(networks);
      if (path === "/api/ui/networks/pinned") return Promise.resolve(pinned);
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      return Promise.reject(new Error("no local network"));
    });
    mocks.del.mockImplementation(() => {
      pinned = { ...pinned, custom: [] };
      return Promise.resolve({});
    });
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);

    await screen.findByText("Rede privada");
    const kinds = screen.getAllByTestId(/^network-row-/).map((row) => row.getAttribute("data-network-kind"));
    expect(kinds).toEqual(["own", "custom", "public", "public", "public"]);
    // Before each name, the discovery icon of Network settings.
    const visibilities = screen.getAllByTestId("network-visibility-icon").map((icon) => icon.getAttribute("data-visibility"));
    expect(visibilities).toEqual(["local", "private", "public", "public", "public"]);
    const ownRow = screen.getByTestId("network-row-fingerprint-own");
    expect(within(ownRow).getByText("Your network")).toBeTruthy();
    // The machine's own network offers neither joining nor removal.
    expect(within(ownRow).queryByRole("button", { name: /^Talk$/ })).toBeNull();
    const customRow = screen.getByTestId("network-row-fingerprint-custom");
    expect(within(customRow).getByText("Private · added here")).toBeTruthy();
    expect(within(customRow).getByRole("button", { name: "Talk" })).toBeTruthy();

    // Pinned rows follow the same filters as the list.
    openFilters();
    fireEvent.change(screen.getByLabelText("Country"), { target: { value: "PT" } });
    await waitFor(() => expect(screen.queryByText("Rede Curitiba")).toBeNull());
    // The own network is in Portugal like the filter; the imported one is not.
    expect(screen.getByText("Minha rede")).toBeTruthy();
    expect(screen.queryByText("Rede privada")).toBeNull();
    fireEvent.change(screen.getByLabelText("Country"), { target: { value: "" } });
    await screen.findByText("Rede privada");

    // Only this machine's own network exports its trust document: that is
    // the one worth handing to another machine.
    expect(within(screen.getByTestId("network-row-fingerprint-custom")).queryByRole("button", { name: "Export the trust document of Rede privada" })).toBeNull();
    const blobs: Blob[] = [];
    vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: (blob: Blob) => { blobs.push(blob); return "blob:x"; }, revokeObjectURL: () => {} }));
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    // The share action says what it does, not only an icon.
    expect(within(screen.getByTestId("network-row-fingerprint-own")).getByRole("button", { name: "Export the trust document of Minha rede" }).textContent).toContain("Share");
    fireEvent.click(within(screen.getByTestId("network-row-fingerprint-own")).getByRole("button", { name: "Export the trust document of Minha rede" }));
    // Share explains the trip first; the file is downloaded from the dialog.
    const shareDialog = await screen.findByRole("dialog", { name: "Share this network" });
    expect(within(shareDialog).getByTestId("trust-transfer-diagram")).toBeTruthy();
    expect(within(shareDialog).getByText(/Add network and choose the file/)).toBeTruthy();
    expect(click).not.toHaveBeenCalled();
    fireEvent.click(within(shareDialog).getByRole("button", { name: "Download file" }));
    expect(click).toHaveBeenCalledTimes(1);
    // jsdom's Blob cannot be read back; its size is the document's.
    expect(blobs[0].size).toBe("{\"own\":true}".length);
    click.mockRestore();

    // The filter remounted the row.
    fireEvent.click(within(screen.getByTestId("network-row-fingerprint-custom")).getByRole("button", { name: "Remove Rede privada from the list" }));
    fireEvent.click(await screen.findByRole("button", { name: "Remove from the list" }));
    await waitFor(() => expect(screen.queryByText("Rede privada")).toBeNull());
    expect(mocks.del).toHaveBeenCalledWith("/api/ui/networks/custom/fingerprint-custom");
  });

  it("adds a private network from a trust document file", async () => {
    const custom = { ...networks[1], fingerprint: "fingerprint-custom", name: "Rede privada", publisher_key: "pk:custom", discovered_via: "custom" };
    let pinned: { own: null; custom: { network: typeof custom; visibility: string }[] } = { own: null, custom: [] };
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve([]);
      if (path === "/api/ui/networks/pinned") return Promise.resolve(pinned);
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      return Promise.reject(new Error("no local network"));
    });
    mocks.post.mockImplementation(() => {
      pinned = { own: null, custom: [{ network: custom, visibility: "private" }] };
      return Promise.resolve(pinned.custom[0]);
    });
    render(<LocaleProvider initialLocale="en"><PublicNetworks /></LocaleProvider>);
    await screen.findByText("No networks found");
    expect(screen.getByRole("button", { name: "Add network" })).toBeTruthy();

    const file = new File(["{\"anchor\":{}}"], "rede-trust.json", { type: "application/json" });
    Object.defineProperty(file, "text", { value: () => Promise.resolve("{\"anchor\":{}}") });
    fireEvent.change(screen.getByTestId("network-import-file"), { target: { files: [file] } });
    await screen.findByText("Rede privada");
    expect(mocks.post).toHaveBeenCalledWith("/api/ui/networks/custom", { trust_document: "{\"anchor\":{}}" });
  });

  it("opens lobbies in place of the list, keeps them open in tabs and goes back to the networks", async () => {
    mocks.get.mockImplementation((path: string) => {
      if (path === "/api/public/networks") return Promise.resolve(networks);
      if (path.startsWith("/api/public/networks/locate?")) return Promise.resolve({ reachability: "direct", relays: 0 });
      if (path.startsWith("/api/ui/network-chat/lobby?")) {
        return Promise.resolve({ accepted: true, error: null, node_id: "me", card: { hostname: "eu", os: "linux", arch: "x86_64", cpu_count: 4, ram_total_mb: 8192, gpus: [] }, messages: [], admission: null });
      }
      return Promise.reject(new Error("no local network"));
    });
    render(<LocaleProvider initialLocale="pt-BR"><PublicNetworks /></LocaleProvider>);

    fireEvent.click(await screen.findByText("Rede Curitiba"));
    const panel = await screen.findByRole("complementary", { name: "Rede Rede Curitiba" });
    fireEvent.click(within(panel).getByRole("button", { name: "Lobby" }));
    const lobby = await screen.findByRole("region", { name: "Lobby de Rede Curitiba" });
    // The list is out of sight while the lobby fills the screen.
    expect(screen.getByText("Rede Lisboa").closest("div.hidden")).toBeTruthy();

    // No back button: the tabs stay on top of the lobby, and the Networks tab goes back.
    expect(within(lobby).queryByRole("button", { name: "Voltar para redes" })).toBeNull();
    fireEvent.click(within(screen.getByRole("tablist", { name: "Lobbies abertos" })).getByRole("tab", { name: "Redes" }));
    await waitFor(() => expect(screen.getByText("Rede Lisboa").closest("div.hidden")).toBeNull());
    // The lobby stays open in its tab.
    const tabs = screen.getByRole("tablist", { name: "Lobbies abertos" });
    expect(within(tabs).getByRole("tab", { name: "Rede Curitiba" })).toBeTruthy();

    // A second lobby opens beside the first, and the tabs switch between them.
    const row = screen.getByTestId("network-row-fingerprint-lisboa");
    fireEvent.click(within(row).getByRole("button", { name: "Conversar" }));
    const second = await screen.findByRole("region", { name: "Lobby de Rede Lisboa" });
    await waitFor(() => expect(mocks.get).toHaveBeenCalledWith("/api/ui/network-chat/lobby?key=pk%3Alisboa", { auth: true }));
    const strip = await screen.findByRole("tablist", { name: "Lobbies abertos" });
    expect(within(strip).getByRole("tab", { name: "Rede Lisboa" }).getAttribute("aria-selected")).toBe("true");
    expect(second).toBeTruthy();
    expect(within(strip).getAllByRole("tab").map((tab) => tab.textContent)).toEqual(["Redes", "Rede Curitiba", "Rede Lisboa"]);

    fireEvent.click(within(strip).getByRole("button", { name: "Fechar o lobby de Rede Lisboa" }));
    expect(screen.queryByRole("region", { name: "Lobby de Rede Lisboa" })).toBeNull();
    expect(within(strip).getAllByRole("tab").map((tab) => tab.textContent)).toEqual(["Redes", "Rede Curitiba"]);
  });
});

describe("latencyBand", () => {
  it("grades the round trip by how much it slows inference", () => {
    expect([10, 59, 60, 149, 150, 299, 300, 900].map(latencyBand)).toEqual(["fast", "fast", "fair", "fair", "slow", "slow", "poor", "poor"]);
  });
});
