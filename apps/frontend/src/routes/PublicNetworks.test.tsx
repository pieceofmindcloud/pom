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
}));

vi.mock("../lib/api", () => ({
  api: { get: mocks.get, post: mocks.post, del: mocks.del },
}));
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
    expect(screen.queryByText("Rede Lisboa")).toBeNull();
    expect(screen.getByText("Rede Curitiba")).toBeTruthy();

    const city = screen.getByRole("combobox", { name: "Cidade" });
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
    expect(order()).toEqual(["Lisboa"]);
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

  it("opens the full details of the public network when the row is clicked", async () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    const networkName = await screen.findByText("Rede Curitiba");
    fireEvent.click(networkName);

    const dialog = await screen.findByRole("dialog", { name: "Detalhes de Rede Curitiba" });
    expect(within(dialog).getByRole("heading", { name: "Rede para colaborar" })).toBeTruthy();
    expect(within(dialog).getByText("Respeite os peers")).toBeTruthy();
    expect(within(dialog).getByRole("link", { name: "Discord" })).toBeTruthy();
    expect(within(dialog).getByText("Equipe Sul")).toBeTruthy();
    expect(within(dialog).getByText("fingerprint-curitiba")).toBeTruthy();
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
    const dialog = await screen.findByRole("dialog", { name: "Lobby de Rede Curitiba" });
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
});
