// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { resetPublicLatencyCache } from "../lib/publicLatency";
import { resetPublicLocatorCache } from "../lib/publicLocator";
import { resetPublicStatsCache } from "../lib/publicStats";
import { PublicNetworks } from "./PublicNetworks";

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

    fireEvent.change(screen.getByRole("combobox", { name: "Sort by" }), { target: { value: "latency" } });
    await waitFor(() => expect(screen.getByTestId("public-network-latency-fingerprint-sao-paulo").textContent).toBe("40 ms"));
    await waitFor(() => expect(screen.getByTestId("public-network-latency-fingerprint-lisboa").textContent).toBe("no answer"));
    expect(screen.getByTestId("public-network-latency-fingerprint-curitiba").textContent).toBe("180 ms");
    expect(asked()).toBe(3);
    // The network that did not answer goes last.
    expect(order()).toEqual(["São Paulo", "Curitiba", "Lisboa"]);
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
    expect(within(panel).getByRole("link", { name: "Discord" })).toBeTruthy();
    expect(within(panel).getByText("Respeite os peers")).toBeTruthy();
    // What the row already shows is not repeated.
    expect(within(panel).queryByText("Equipe Sul")).toBeNull();
    expect(within(panel).queryByText("Curitiba, Brasil")).toBeNull();
    const actions = within(panel).getByTestId("network-panel-actions");
    expect(within(actions).getByRole("button", { name: "Pedir para entrar" })).toBeTruthy();
    expect(within(actions).getByRole("button", { name: "Lobby" })).toBeTruthy();
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

  it("pages the known networks and searches them on the node, keeping this machine's network on top", async () => {
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
    await waitFor(() => expect(screen.getAllByTestId(/^network-row-/).map((row) => row.getAttribute("data-testid"))).toEqual(["network-row-fingerprint-own", "network-row-fingerprint-7"]));
    // A new search starts again from the first page.
    expect(mocks.knownRequests.some((path) => path.includes("q=rede+07") && path.includes("page=1&"))).toBe(true);
    expect(screen.getByTestId("public-networks-pagination").textContent).toContain("1-1 of 1 networks");
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
    expect(within(row).queryByRole("button", { name: "Conversar e pedir para entrar" })).toBeNull();
    const other = screen.getByText("Rede Lisboa").closest("tr")!;
    expect(within(other).getByRole("button", { name: "Conversar e pedir para entrar" })).toBeTruthy();

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
    await waitFor(() => expect(within(row).queryByRole("button", { name: "Conversar e pedir para entrar" })).toBeNull());
    expect(within(row).queryByRole("button", { name: "Sair da rede" })).toBeNull();
    expect(within(other).getByRole("button", { name: "Conversar e pedir para entrar" })).toBeTruthy();
  });

  it("only asks to join, in a window, a network whose lobby has no conversation", async () => {
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
    expect(within(row).queryByRole("button", { name: "Conversar e pedir para entrar" })).toBeNull();
    // Closed and with nothing to talk in, there is nothing to do.
    const closed = screen.getByText("Rede São Paulo").closest("tr")!;
    expect((within(closed).getByRole("button", { name: "Pedir para entrar" }) as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(within(row).getByRole("button", { name: "Pedir para entrar" }));
    await screen.findByRole("dialog");
    // A window over the list, not a lobby tab in its place.
    expect(screen.getByText("Rede São Paulo")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Voltar para redes" })).toBeNull();
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
    expect((within(third).getByRole("button", { name: "Conversar e pedir para entrar" }) as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(within(open).getByRole("button", { name: "Conversar e pedir para entrar" }));
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
    expect(await within(row).findByText("gemma, qwen3")).toBeTruthy();
    expect(within(row).getByText("4")).toBeTruthy();
    expect(within(row).getByText("48.0 GB")).toBeTruthy();
    // Latency to a network that was not joined is not measured by anyone.
    expect(within(row).getAllByText("Indisponível").length).toBe(1);

    const silent = screen.getByText("Rede São Paulo").closest("tr")!;
    expect(within(silent).queryByText("gemma, qwen3")).toBeNull();
    expect(mocks.get.mock.calls.some(([path]) => String(path).includes("lobby/info?key=pk%3Asao-paulo"))).toBe(false);
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
    const ownRow = screen.getByTestId("network-row-fingerprint-own");
    expect(within(ownRow).getByText("Your network")).toBeTruthy();
    // The machine's own network offers neither joining nor removal.
    expect(within(ownRow).queryByRole("button", { name: /Talk and ask to join/ })).toBeNull();
    const customRow = screen.getByTestId("network-row-fingerprint-custom");
    expect(within(customRow).getByText("Private · added here")).toBeTruthy();
    expect(within(customRow).getByRole("button", { name: "Talk and ask to join" })).toBeTruthy();

    // Pinned rows stay whatever the filter hides.
    fireEvent.change(screen.getByLabelText("Country"), { target: { value: "PT" } });
    await waitFor(() => expect(screen.queryByText("Rede Curitiba")).toBeNull());
    expect(screen.getByText("Minha rede")).toBeTruthy();
    expect(screen.getByText("Rede privada")).toBeTruthy();

    // Every row exports its trust document as a file.
    const blobs: Blob[] = [];
    vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: (blob: Blob) => { blobs.push(blob); return "blob:x"; }, revokeObjectURL: () => {} }));
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    fireEvent.click(within(customRow).getByRole("button", { name: "Export the trust document of Rede privada" }));
    expect(click).toHaveBeenCalledTimes(1);
    // jsdom's Blob cannot be read back; its size is the document's.
    expect(blobs[0].size).toBe("{\"custom\":true}".length);
    click.mockRestore();

    fireEvent.click(within(customRow).getByRole("button", { name: "Remove Rede privada from the list" }));
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

    fireEvent.click(within(lobby).getByRole("button", { name: "Voltar para redes" }));
    await waitFor(() => expect(screen.getByText("Rede Lisboa").closest("div.hidden")).toBeNull());
    // The lobby stays open in its tab.
    const tabs = screen.getByRole("tablist", { name: "Lobbies abertos" });
    expect(within(tabs).getByRole("tab", { name: "Rede Curitiba" })).toBeTruthy();

    // A second lobby opens beside the first.
    const row = screen.getByTestId("network-row-fingerprint-lisboa");
    fireEvent.click(within(row).getByRole("button", { name: "Conversar e pedir para entrar" }));
    await screen.findByRole("region", { name: "Lobby de Rede Lisboa" });
    expect(within(tabs).getAllByRole("tab").map((tab) => tab.textContent)).toEqual(["Redes", "Rede Curitiba", "Rede Lisboa"]);
    await waitFor(() => expect(mocks.get).toHaveBeenCalledWith("/api/ui/network-chat/lobby?key=pk%3Alisboa", { auth: true }));

    fireEvent.click(within(tabs).getByRole("button", { name: "Fechar o lobby de Rede Lisboa" }));
    expect(screen.queryByRole("region", { name: "Lobby de Rede Lisboa" })).toBeNull();
    expect(within(tabs).getAllByRole("tab").map((tab) => tab.textContent)).toEqual(["Redes", "Rede Curitiba"]);
  });
});
